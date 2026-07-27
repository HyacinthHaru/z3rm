// §4 Shadow snapshot session wiring.
//
// Connects shadow_snapshot's worktree Monitor to a Session's cwd so that file
// changes inside the session working directory flow through the snapshot
// engine (WAL → version tree → blob store).
//
// Why a dedicated recorder thread:
//   ShadowSnapshotEngine is !Send + !Sync — it owns an Arc<StorageEngine>
//   wrapping a rusqlite::Connection, whose InnerConnection uses RefCell. It
//   therefore cannot leave the thread that created it nor be shared across
//   threads. To satisfy the spec §4.3 "single-writer thread for WAL"
//   constraint AND keep the engine usable, we pin the engine to one OS
//   thread (the recorder thread): the engine is constructed and used only
//   there, so nothing !Send/:Send-,:Sync-only ever crosses a thread boundary.
//
// Why a channel between the watcher and the recorder:
//   Monitor::watch_directory spawns its OWN background thread that drives the
//   on_event callback. That thread is not the recorder thread, so the callback
//   cannot touch the engine directly. It hands each changed path to the
//   recorder via an std::sync::mpsc channel; the recorder reads the file and
//   calls engine.record_change on its own thread.
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::sync::Mutex;

use anyhow::{Context, Result};
use zlog;  // external crate, not crate::zlog
use shadow_snapshot::{EventKind, FileEvent, Monitor, SnapshotTrigger, WatchHandle};

/// Handle to one session's shadow-snapshot watcher + recorder.
///
/// Cheaply shareable via [`Arc<SnapshotWatch>`] (the inner state is behind a
/// shared `Arc`). Dropping the last clone — or calling [`SnapshotWatch::stop`]
/// — stops the filesystem watcher, closes the recorder channel, and joins the
/// recorder thread, releasing all engine resources for that session.
pub struct SnapshotWatch {
    inner: Arc<WatchInner>,
}

/// Shared inner state; refcounted so [`Session`] (which derives Clone) and the
/// server registry can both hold it without the non-Clone `WatchHandle` /
/// `JoinHandle` breaking `Clone`.
struct WatchInner {
    /// Session id — used only for logging on stop.
    session_id: String,
    /// notify watcher; dropping stops watching and ends the watcher thread.
    watch_handle: Mutex<Option<WatchHandle>>,
    /// Feed of changed paths from watcher thread → recorder thread.
    /// Dropping this sender makes the recorder's recv loop exit.
    path_sender: Mutex<Option<mpsc::Sender<PathBuf>>>,
    /// Recorder thread handle, joined on stop/drop.
    recorder: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Idempotently tear down the watcher + recorder for a session.
///
/// Order matters for a prompt, deadlock-free shutdown:
///   1. Drop the WatchHandle → notify watcher thread sees the channel close
///      and exits, releasing its clone of the path sender.
///   2. Drop our path sender → rx.iter() in the recorder returns None.
///   3. join() the recorder — guaranteed to have exited at step 2, so the
///      join returns promptly without blocking Drop.
fn stop_inner(inner: &WatchInner) {
    if let Some(handle) = inner.watch_handle.lock().expect("watch mutex poisoned").take() {
        drop(handle);
    }
    if let Some(sender) = inner.path_sender.lock().expect("sender mutex poisoned").take() {
        drop(sender);
    }
    if let Some(join) = inner.recorder.lock().expect("recorder mutex poisoned").take() {
        // Recorder only blocks on path_rx.recv(); with all senders dropped it
        // has already exited, so join completes immediately.
        let _ = join.join();
    }
}
impl SnapshotWatch {
    /// Stop watching and recording for this session. Safe to call more than
    /// once (subsequent calls are no-ops).
    pub fn stop(&self) {
        stop_inner(&self.inner);
        if !self.inner.session_id.is_empty() {
            zlog::info!("shadow snapshot stopped: session={}", self.inner.session_id);
        }
    }
}

impl Drop for WatchInner {
    fn drop(&mut self) {
        stop_inner(self);
    }
}

/// Per-session storage directory layout root: `$LOCAL_DATA/z3rm/shadow/<session_id>`.
fn session_shadow_dir(session_id: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("z3rm")
        .join("shadow")
        .join(session_id)
}

/// Start shadow-snapshot watching + recording for a session's cwd.
///
/// Returns `Ok(None)` when the cwd is not a usable directory (e.g. recovered or
/// test sessions with an abstract cwd) — session creation still succeeds, the
/// snapshot subsystem is simply not armed for it. Returns `Err` only for truly
/// unexpected failures so the caller can decide how much noise to make.
///
/// The engine's DB / WAL / blobs are placed under
/// `$LOCAL_DATA/z3rm/shadow/<session_id>/` so each session gets its own
/// single-writer engine instance.
pub fn start(session_id: &str, cwd: &str) -> Result<Option<Arc<SnapshotWatch>>> {
    let root = Path::new(cwd);
    // A usable watch root must be an existing directory. Recovered/test
    // sessions carry a cwd that is just a string (e.g. "/home/user"); starting
    // a recursive notify watcher there would either fail or watch the wrong
    // tree, so we opt out cleanly rather than best-effort.
    if !root.is_dir() {
        zlog::info!(
            "shadow snapshot skipped: session={} cwd not a directory ({})",
            session_id,
            cwd,
        );
        return Ok(None);
    }

    let dir = session_shadow_dir(session_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("shadow snapshot dir create: {}", dir.display()))?;

    let db_path = dir.join("shadow.db");
    let wal_path = dir.join("wal.bin");
    let blob_dir = dir.join("blobs");
    std::fs::create_dir_all(&blob_dir)
        .with_context(|| format!("shadow blob dir create: {}", blob_dir.display()))?;

    // Channel: watcher thread (inside Monitor) → recorder thread (owns engine).
    let (path_tx, path_rx) = mpsc::channel::<PathBuf>();
    // Channel: recorder reports engine-open result back to us before looping.
    let (init_tx, init_rx) = mpsc::channel::<Result<()>>();

    // Recorder thread owns the engine for the session's lifetime. The engine is
    // constructed here (not on the caller thread) precisely because it is
    // !Send: it must never cross threads. Only Send types (path_rx, init_tx)
    // move into the closure.
    let recorder_session_id = session_id.to_string();
    let root_for_recorder = root.to_path_buf();
    let recorder = std::thread::Builder::new()
        .name(format!("shadow-snap-{}", session_id))
        .spawn(move || {
            let root = root_for_recorder;
            let engine = match shadow_snapshot::ShadowSnapshotEngine::open(&db_path, &wal_path, &blob_dir) {
                Ok(engine) => {
                    // §4.8: complete any Decline intents that crashed mid-restore.
                    // Resolve path_hash by hashing every file under the session cwd
                    // (no persistent reverse index yet; walk is bounded to one tree).
                    let path_index = build_path_hash_index(&root);
                    match engine.recover_incomplete_restores(|path_hash| {
                        path_index.get(path_hash).cloned()
                    }) {
                        Ok(n) if n > 0 => {
                            zlog::info!(
                                "shadow decline recovery: session={} completed={}",
                                recorder_session_id,
                                n,
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            zlog::warn!(
                                "shadow decline recovery failed: session={} error={}",
                                recorder_session_id,
                                error,
                            );
                        }
                    }
                    // Engine opened fine; tell the caller and enter the loop.
                    // A send failure means the caller gave up — just exit.
                    let _ = init_tx.send(Ok(()));
                    engine
                }
                Err(error) => {
                    // Surface the open failure; the caller surfaces it as Err.
                    let _ = init_tx.send(Err(error));
                    return;
                }
            };

            // Single-writer loop: the only synthetic SeqNo source is the engine
            // itself (fetch_add on its atomic), and every record_change runs
            // here, satisfying spec §4.3/§4.5 ordering.
            for path in path_rx.iter() {
                // Read current content; transient read errors (file deleted
                // mid-write, permission race) are logged and skipped rather
                // than killing the recorder — a bad snapshot for one file
                // must not halt versioning for the rest of the worktree.
                match std::fs::read(&path) {
                    Ok(content) => {
                        if let Err(error) = engine.record_change(&path, &content) {
                        zlog::warn!(
                            "shadow snapshot record failed: session={} path={} error={}",
                            recorder_session_id,
                            path.display(),
                            error,
                        );
                        }
                    }
                    Err(error) => {
                        zlog::warn!(
                            "shadow snapshot read failed: session={} path={} error={}",
                            recorder_session_id,
                            path.display(),
                            error,
                        );
                    }
                }
            }
        })
        .context("spawn shadow snapshot recorder thread")?;

    // Synchronously confirm the engine opened successfully before we start the
    // watcher — there's no point watching a cwd whose changes can't be logged.
    init_rx
        .recv()
        .context("shadow recorder thread exit before init")??;

    // on_event runs on Monitor's own watcher thread; it must not touch the
    // engine, so it only enqueues the changed path and maps the event kind to
    // a trigger. The Drop of the sender is silently ignored: a failed send
    // merely means the recorder channel drained / stopped, which is not an
    // error worth propagating into the watcher pipeline's trigger decisions.
    let on_event = {
        let path_tx = path_tx.clone();
        let session_id = session_id.to_string();
        move |event: FileEvent| -> SnapshotTrigger {
            let _ = path_tx.send(event.path);
            event_to_trigger(event.kind, &session_id)
        }
    };

    let monitor = Arc::new(Monitor::new(root.to_path_buf(), on_event));
    let watch_handle = match monitor.watch_directory(root.to_path_buf()) {
        Ok(handle) => {
            zlog::info!(
                "shadow snapshot started: session={} cwd={}",
                session_id,
                cwd,
            );
            handle
        }
        Err(error) => {
            // watch_directory failed — drop the recorder sender so the recorder
            // thread exits, and surface the error to the caller.
            drop(path_tx);
            let _ = recorder.join();
            return Err(error).with_context(|| {
                format!("shadow snapshot watch_directory: session={} cwd={}", session_id, cwd)
            });
        }
    };

    Ok(Some(Arc::new(SnapshotWatch {
        inner: Arc::new(WatchInner {
            session_id: session_id.to_string(),
            watch_handle: Mutex::new(Some(watch_handle)),
            path_sender: Mutex::new(Some(path_tx)),
            recorder: Mutex::new(Some(recorder)),
        }),
    })))
}


/// Build path_hash → PathBuf index for decline recovery by walking the session cwd.
/// Matches `shadow_snapshot::compute_path_hash` (blake3 of path.to_string_lossy).
fn build_path_hash_index(root: &Path) -> std::collections::HashMap<[u8; 32], PathBuf> {
    let mut index = std::collections::HashMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        let hash = shadow_snapshot::compute_path_hash(&path);
        index.insert(hash, path);
    }
    index
}

/// Map a filesystem event kind to the snapshot trigger reason it represents.
/// Deleted files surface as `Delete` so the version tree records the
/// removal rather than a content write.
fn event_to_trigger(kind: EventKind, _session_id: &str) -> SnapshotTrigger {
    match kind {
        EventKind::Created | EventKind::Modified | EventKind::Renamed => SnapshotTrigger::Write,
        EventKind::Deleted => SnapshotTrigger::Delete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_mapping_covers_all_kinds() {
        assert_eq!(event_to_trigger(EventKind::Created, "s"), SnapshotTrigger::Write);
        assert_eq!(event_to_trigger(EventKind::Modified, "s"), SnapshotTrigger::Write);
        assert_eq!(event_to_trigger(EventKind::Renamed, "s"), SnapshotTrigger::Write);
        assert_eq!(event_to_trigger(EventKind::Deleted, "s"), SnapshotTrigger::Delete);
    }

    #[test]
    fn skip_when_cwd_not_a_directory() {
        // A path that is not an existing directory must opt out, not error.
        let result = start("noop-nota-real-dir-123", "/definitely/not/real/cwd/zzz");
        match result {
            Ok(None) => {}
            other => panic!("expected Ok(None), got {:?}", other.map(|o| o.is_some())),
        }
    }
}
