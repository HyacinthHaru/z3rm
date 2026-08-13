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
use std::sync::Mutex;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use shadow_snapshot::{
    DebounceQueue, EventKind, FileEvent, GitCommitHookMode, GitCommitTracker, GitCommitWatcher,
    Monitor, QuotaMode, SnapshotConfig, SnapshotTrigger, WatchHandle,
};
use zlog; // external crate, not crate::zlog

#[derive(Debug, Clone)]
pub struct FileVersion {
    pub version_id: u64,
    pub seq_no: u64,
    pub trigger: SnapshotTrigger,
}

#[derive(Debug, Clone)]
pub struct FileReviewState {
    pub versions: Vec<FileVersion>,
    pub latest_seq_no: u64,
    pub current_content: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
pub struct DeclineOutcome {
    pub version_id: u64,
    pub seq_no: u64,
}

/// §4 One file that accumulated shadow versions during this session.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub version_count: u64,
    pub latest_seq_no: u64,
    pub current_exists: bool,
    /// Trigger of the oldest retained version; `"create"` means this session
    /// saw the file appear.
    pub first_trigger: String,
}

enum ShadowCommand {
    ListChangedFiles {
        reply: mpsc::Sender<Result<Vec<FileChange>>>,
    },
    ListChangedFilesReconciled {
        reply: mpsc::Sender<Result<Vec<FileChange>>>,
    },
    ListVersions {
        path: PathBuf,
        reply: mpsc::Sender<Result<Vec<FileVersion>>>,
    },
    GetVersion {
        path: PathBuf,
        version_id: u64,
        reply: mpsc::Sender<Result<Option<shadow_snapshot::VersionContent>>>,
    },
    GetReviewState {
        path: PathBuf,
        reply: mpsc::Sender<Result<FileReviewState>>,
    },
    Decline {
        path: PathBuf,
        version_id: u64,
        expected_latest_seq_no: u64,
        expected_current_exists: bool,
        expected_current_hash: Option<shadow_snapshot::ContentHash>,
        reply: mpsc::Sender<Result<DeclineOutcome>>,
    },
    /// §4 WAL checkpoint: rotate the WAL on the recorder (single-writer)
    /// thread after every prior entry is persisted. The reply carries the
    /// rotation result; also the mechanism the stop path uses for the final
    /// graceful-shutdown checkpoint.
    Checkpoint { reply: mpsc::Sender<Result<()>> },
    /// §4.9 a git commit landed on the watched worktree; the recorder marks
    /// pre-commit deltas gc-eligible on its own (single-writer) thread.
    GitCommit { commit: String },
}

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
    /// §4.9 `.git` watcher feeding commit notifications into the recorder.
    /// Absent when the worktree is not a git repository or the user selected
    /// `git_commit_hook: "skip"`.
    git_watch_handle: Mutex<Option<GitCommitWatcher>>,
    /// Feed of changed paths from watcher thread → recorder thread.
    /// Dropping this sender makes the recorder's recv loop exit.
    path_sender: Mutex<Option<mpsc::Sender<(PathBuf, shadow_snapshot::SnapshotTrigger)>>>,
    /// Commands from RPC handlers into the single-writer recorder thread.
    command_sender: Mutex<Option<mpsc::Sender<ShadowCommand>>>,
    /// Recorder thread handle, joined on stop/drop.
    recorder: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn lock_for_shutdown<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Shutdown releases handles and joins the recorder on a best-effort
    // basis; a panic that poisoned the lock must not abort teardown, and the
    // guarded value is still valid to take.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stop_inner(inner: &WatchInner) {
    // Graceful shutdown: run one final WAL checkpoint on the recorder thread
    // BEFORE the channels are dropped, so the loop is guaranteed to still be
    // alive and the command is serialized with any still-queued work. The
    // reply is awaited before teardown proceeds. Failures are logged —
    // teardown must not abort on a checkpoint error, and the WAL entries
    // remain on disk either way.
    {
        let sender = lock_for_shutdown(&inner.command_sender);
        if let Some(sender) = sender.as_ref() {
            let (reply, response) = mpsc::channel();
            if sender.send(ShadowCommand::Checkpoint { reply }).is_ok() {
                match response.recv() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        zlog::warn!(
                            "shadow final checkpoint failed: session={} error={}",
                            inner.session_id,
                            error,
                        );
                    }
                    Err(_) => {
                        zlog::warn!(
                            "shadow recorder exited before final checkpoint: session={}",
                            inner.session_id,
                        );
                    }
                }
            } else {
                zlog::warn!(
                    "shadow final checkpoint not sent: recorder already stopped, session={}",
                    inner.session_id,
                );
            }
        }
    }
    if let Some(handle) = lock_for_shutdown(&inner.watch_handle).take() {
        drop(handle);
    }
    if let Some(handle) = lock_for_shutdown(&inner.git_watch_handle).take() {
        drop(handle);
    }
    if let Some(sender) = lock_for_shutdown(&inner.path_sender).take() {
        drop(sender);
    }
    if let Some(sender) = lock_for_shutdown(&inner.command_sender).take() {
        drop(sender);
    }
    if let Some(join) = lock_for_shutdown(&inner.recorder).take() {
        if join.join().is_err() {
            zlog::warn!("shadow snapshot recorder thread panicked during shutdown");
        }
    }
}
impl SnapshotWatch {
    /// Stop watching and recording for this session. Safe to call more than once.
    pub fn stop(&self) {
        stop_inner(&self.inner);
        if !self.inner.session_id.is_empty() {
            zlog::info!("shadow snapshot stopped: session={}", self.inner.session_id);
        }
    }

    pub fn list_changed_files(&self) -> Result<Vec<FileChange>> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::ListChangedFiles { reply })?;
        response
            .recv()
            .context("shadow recorder stopped before listing changed files")?
    }

    pub fn list_changed_files_reconciled(&self) -> Result<Vec<FileChange>> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::ListChangedFilesReconciled { reply })?;
        response
            .recv()
            .context("shadow recorder stopped before reconciling changed files")?
    }

    pub fn list_versions(&self, path: PathBuf) -> Result<Vec<FileVersion>> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::ListVersions { path, reply })?;
        response
            .recv()
            .context("shadow recorder stopped before listing versions")?
    }

    pub fn get_version(
        &self,
        path: PathBuf,
        version_id: u64,
    ) -> Result<Option<shadow_snapshot::VersionContent>> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::GetVersion {
            path,
            version_id,
            reply,
        })?;
        response
            .recv()
            .context("shadow recorder stopped before reading version")?
    }

    pub fn get_review_state(&self, path: PathBuf) -> Result<FileReviewState> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::GetReviewState { path, reply })?;
        response
            .recv()
            .context("shadow recorder stopped before reading review state")?
    }

    pub fn decline(
        &self,
        path: PathBuf,
        version_id: u64,
        expected_latest_seq_no: u64,
        expected_current_exists: bool,
        expected_current_hash: Option<shadow_snapshot::ContentHash>,
    ) -> Result<DeclineOutcome> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::Decline {
            path,
            version_id,
            expected_latest_seq_no,
            expected_current_exists,
            expected_current_hash,
            reply,
        })?;
        response
            .recv()
            .context("shadow recorder stopped before restoring version")?
    }

    /// §4 WAL checkpoint: rotate the WAL on the recorder thread after all
    /// prior entries are persisted. Also the mechanism [`SnapshotWatch::stop`]
    /// uses for the final graceful-shutdown checkpoint. Errors (including a
    /// refusal while a decline restore is still incomplete) propagate to the
    /// caller.
    pub fn checkpoint(&self) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.send_command(ShadowCommand::Checkpoint { reply })?;
        response
            .recv()
            .context("shadow recorder stopped before checkpoint")?
    }

    fn send_command(&self, command: ShadowCommand) -> Result<()> {
        // A prior panic may poison this mutex, but the optional sender itself is
        // still usable. Recover the guard so an RPC returns an ordinary channel
        // error instead of aborting the process.
        let sender = lock_for_shutdown(&self.inner.command_sender);
        sender
            .as_ref()
            .context("shadow recorder is stopped")?
            .send(command)
            .context("sending command to shadow recorder")
    }
}

impl Drop for WatchInner {
    fn drop(&mut self) {
        stop_inner(self);
    }
}

/// §16.11 the `shadow_snapshot` section of the user's `settings.json`.
///
/// Every field is optional so a partial section keeps the documented defaults
/// instead of collapsing to Rust zero values. The daemon deliberately parses
/// this itself rather than depending on `settings_content` / the GPUI settings
/// stack — the same reasoning as `server_settings.rs`: a remote daemon must be
/// able to read its own configuration without the client crate graph.
#[derive(Debug, Default, Deserialize)]
struct ShadowSnapshotSettingsFile {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    quota_mode: Option<String>,
    #[serde(default)]
    per_project_quota_mb: Option<u64>,
    #[serde(default)]
    ignore_patterns: Option<Vec<String>>,
    #[serde(default)]
    binary_detection: Option<bool>,
    #[serde(default)]
    debounce_ms: Option<u64>,
    #[serde(default)]
    frequency_circuit_breaker_k: Option<f64>,
    #[serde(default)]
    git_commit_hook: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    shadow_snapshot: Option<ShadowSnapshotSettingsFile>,
}

impl ShadowSnapshotSettingsFile {
    /// Translate user settings into the engine's runtime config. Unknown enum
    /// spellings fall back to the documented default and are logged rather than
    /// failing session creation over a typo.
    fn to_config(&self) -> SnapshotConfig {
        let defaults = SnapshotConfig::default();
        let quota_mode = match self.quota_mode.as_deref() {
            None => defaults.quota_mode,
            Some("per_project") => QuotaMode::PerProject,
            Some("global") => QuotaMode::Global,
            Some(other) => {
                zlog::warn!("shadow_snapshot.quota_mode: unknown value {}", other);
                defaults.quota_mode
            }
        };
        let git_commit_hook = match self.git_commit_hook.as_deref() {
            None => defaults.git_commit_hook,
            Some("clear") => GitCommitHookMode::Clear,
            Some("keep") => GitCommitHookMode::Keep,
            Some("skip") => GitCommitHookMode::Skip,
            Some(other) => {
                zlog::warn!("shadow_snapshot.git_commit_hook: unknown value {}", other);
                defaults.git_commit_hook
            }
        };
        SnapshotConfig {
            enabled: self.enabled.unwrap_or(defaults.enabled),
            quota_mode,
            // 0 MB is the documented "unlimited" setting (§4.9), so it is not
            // clamped up to the default.
            quota_bytes: self
                .per_project_quota_mb
                .map(|megabytes| megabytes.saturating_mul(1024 * 1024))
                .unwrap_or(defaults.quota_bytes),
            ignore_patterns: self
                .ignore_patterns
                .clone()
                .unwrap_or(defaults.ignore_patterns),
            binary_detection: self.binary_detection.unwrap_or(defaults.binary_detection),
            debounce: self
                .debounce_ms
                .map(Duration::from_millis)
                .unwrap_or(defaults.debounce),
            circuit_breaker_writes_per_second: self
                .frequency_circuit_breaker_k
                // A non-positive K would suspend every single write; treat it
                // as "unset" instead of silently disabling snapshotting.
                .filter(|k| *k > 0.0)
                .unwrap_or(defaults.circuit_breaker_writes_per_second),
            git_commit_hook,
        }
    }
}

/// Resolve the settings file the daemon should read. `Z3RM_SETTINGS` overrides;
/// otherwise this mirrors `paths::config_dir()` (which the daemon cannot depend
/// on) so the daemon reads exactly the file the settings UI writes.
fn settings_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("Z3RM_SETTINGS") {
        return Some(PathBuf::from(path));
    }
    let config_dir = if cfg!(target_os = "windows") {
        dirs::config_dir().map(|dir| dir.join("Z3rm"))
    } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        dirs::config_dir().map(|dir| dir.join("z3rm"))
    } else {
        dirs::home_dir().map(|home| home.join(".config").join("z3rm"))
    };
    Some(config_dir?.join("settings.json"))
}

/// Load the effective `shadow_snapshot` configuration. A missing or unreadable
/// settings file yields the documented defaults — snapshotting must not be
/// silently disabled because a user has not written a settings file yet.
fn load_config() -> SnapshotConfig {
    let Some(path) = settings_file_path() else {
        return SnapshotConfig::default();
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SnapshotConfig::default();
        }
        Err(error) => {
            zlog::warn!(
                "shadow snapshot settings unreadable ({}): {}",
                path.display(),
                error,
            );
            return SnapshotConfig::default();
        }
    };
    parse_settings(&contents).unwrap_or_else(|error| {
        zlog::warn!(
            "shadow snapshot settings parse failed ({}): {}",
            path.display(),
            error,
        );
        SnapshotConfig::default()
    })
}

fn parse_settings(contents: &str) -> Result<SnapshotConfig> {
    let file: SettingsFile = serde_json::from_str(&strip_jsonc(contents))
        .context("parsing settings.json shadow_snapshot section")?;
    Ok(file.shadow_snapshot.unwrap_or_default().to_config())
}

/// Rewrite JSONC into JSON that `serde_json` accepts: drop `//` and `/* */`
/// comments and trailing commas. settings.json is authored by hand (the shipped
/// default file itself is commented), so a strict parse would reject the very
/// file the settings UI maintains.
///
/// Comments are removed first: a trailing comma is only recognisable once the
/// comment that may sit between it and the closing brace is gone.
fn strip_jsonc(input: &str) -> String {
    strip_trailing_commas(&strip_comments(input))
}

fn strip_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(character) = characters.next() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                output.push(character);
            }
            '/' if characters.peek() == Some(&'/') => {
                for next in characters.by_ref() {
                    if next == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                characters.next();
                let mut previous = '\0';
                for next in characters.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            _ => output.push(character),
        }
    }
    output
}

fn strip_trailing_commas(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(character) = characters.next() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            continue;
        }
        if character != ',' {
            output.push(character);
            continue;
        }
        // A comma followed only by whitespace and a closer is trailing.
        let mut whitespace = String::new();
        while let Some(&next) = characters.peek() {
            if next.is_whitespace() {
                whitespace.push(next);
                characters.next();
            } else {
                break;
            }
        }
        if !matches!(characters.peek(), Some('}') | Some(']')) {
            output.push(',');
        }
        output.push_str(&whitespace);
    }
    output
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
/// Returns `Ok(None)` when the user disabled shadow snapshots or the cwd is not
/// a usable directory (e.g. recovered or test sessions with an abstract cwd) —
/// session creation still succeeds, the snapshot subsystem is simply not armed
/// for it. Returns `Err` only for truly unexpected failures so the caller can
/// decide how much noise to make.
///
/// The engine's DB / WAL / blobs are placed under
/// `$LOCAL_DATA/z3rm/shadow/<session_id>/` so each session gets its own
/// single-writer engine instance.
///
/// Settings are read once per session start: a settings change applies to
/// sessions created afterwards, existing sessions keep the config they booted
/// with (their engine, quota and watcher are already constructed).
pub fn start(session_id: &str, cwd: &str) -> Result<Option<Arc<SnapshotWatch>>> {
    start_with_config(session_id, cwd, load_config(), |monitor, root| {
        monitor.watch_directory(root)
    })
}

pub fn start_with_config(
    session_id: &str,
    cwd: &str,
    config: SnapshotConfig,
    watch: impl FnOnce(Arc<Monitor>, PathBuf) -> std::io::Result<shadow_snapshot::WatchHandle>,
) -> Result<Option<Arc<SnapshotWatch>>> {
    // `shadow_snapshot.enabled = false` must really turn the engine off: no
    // storage directory, no watcher, no recorder thread for this session.
    if !config.enabled {
        zlog::info!(
            "shadow snapshot disabled by settings: session={}",
            session_id,
        );
        return Ok(None);
    }

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
    // Carries the trigger so the recorder can route Delete vs Write without
    // re-deriving it from the path (a delete leaves no file to inspect).
    let (path_tx, path_rx) = mpsc::channel::<(PathBuf, shadow_snapshot::SnapshotTrigger)>();
    let (command_tx, command_rx) = mpsc::channel::<ShadowCommand>();
    // Channel: recorder reports engine-open result back to us before looping.
    let (init_tx, init_rx) = mpsc::channel::<Result<()>>();

    // Recorder thread owns the engine for the session's lifetime. The engine is
    // constructed here (not on the caller thread) precisely because it is
    // !Send: it must never cross threads. Only Send types (path_rx, init_tx)
    // move into the closure.
    let recorder_session_id = session_id.to_string();
    let root_for_recorder = root.to_path_buf();
    let recorder_config = config.clone();
    let recorder = std::thread::Builder::new()
        .name(format!("shadow-snap-{}", session_id))
        .spawn(move || {
            let root = root_for_recorder;
            let config = recorder_config;
            // Reverse index for `path_hash → path`. The engine stores only the
            // hash, so both crash recovery and `ListChangedFiles` need this to
            // name a file. Seeded by walking the worktree, then kept current by
            // `route_record_event` as new paths are recorded — a file created
            // after startup is otherwise unresolvable, and a deleted one has to
            // stay resolvable because its versions outlive it on disk.
            let mut path_index = build_path_hash_index(&root);
            let engine =
                // §4.9 age-based FIFO GC bounded by the user's quota setting
                // (`per_project_quota_mb`, 0 = unlimited). Without a quota,
                // growth stays bounded per-path by `D_MAX` but accumulates
                // full base versions forever.
                match shadow_snapshot::ShadowSnapshotEngine::open_with_config(
                    &db_path, &wal_path, &blob_dir, &config,
                ) {
                    Ok(engine) => {
                        // §4.8: complete any Decline intents that crashed mid-restore.
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
                        // A send failure means the caller gave up before the
                        // engine was ready; nothing left to serve, so exit.
                        if init_tx.send(Ok(())).is_err() {
                            zlog::warn!(
                                "shadow snapshot starter gone before engine ready: session={}",
                                recorder_session_id,
                            );
                            return;
                        }
                        engine
                    }
                    Err(error) => {
                        // Surface the open failure; the caller surfaces it as Err.
                        if init_tx.send(Err(error)).is_err() {
                            zlog::warn!(
                                "shadow snapshot engine open failed and starter is gone: session={}",
                                recorder_session_id,
                            );
                        }
                        return;
                    }
                };

            // §4.7 single-writer loop with a per-path debounce whose window
            // comes from `shadow_snapshot.debounce_ms`. Events are coalesced
            // per-path so a chatty editor saving many times per second produces
            // one version per quiet period, not one per save. `recv_timeout`
            // wakes the loop to flush due paths even when the watcher is
            // silent, satisfying the debounce window without a second timer
            // thread.
            let mut debouncer = DebounceQueue::new(config.debounce);
            let mut suppressed_writes = std::collections::HashMap::new();
            let suppression_ttl = std::time::Duration::from_secs(5);
            // Poll well inside the window so flush latency stays close to it,
            // and never slower than 100ms for the default 500ms window.
            let poll = (config.debounce / 5).clamp(
                std::time::Duration::from_millis(5),
                std::time::Duration::from_millis(100),
            );
            let mut path_disconnected = false;
            loop {
                let now = std::time::Instant::now();
                suppressed_writes.retain(|_path_hash, (_content_hash, deadline)| *deadline > now);
                match path_rx.recv_timeout(poll) {
                    Ok((path, trigger)) => {
                        debouncer.note(path, trigger, std::time::Instant::now());
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        path_disconnected = true;
                    }
                }

                while let Ok(command) = command_rx.try_recv() {
                    match command {
                        ShadowCommand::GetReviewState { path, reply } => {
                            handle_review_state_command(
                                &engine,
                                path,
                                reply,
                                &mut debouncer,
                                &mut suppressed_writes,
                                &mut path_index,
                            );
                        }
                        ShadowCommand::ListChangedFilesReconciled { reply } => {
                            let paths = path_index.values().cloned().collect::<Vec<_>>();
                            let mut failure = None;
                            for path in paths {
                                match engine.list_versions(&path) {
                                    Ok(versions) if versions.is_empty() => continue,
                                    Ok(_) => {}
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                                let (state_reply, state_response) = mpsc::channel();
                                handle_review_state_command(
                                    &engine,
                                    path,
                                    state_reply,
                                    &mut debouncer,
                                    &mut suppressed_writes,
                                    &mut path_index,
                                );
                                match state_response
                                    .recv()
                                    .context("shadow reconciliation response dropped")
                                    .and_then(|result| result)
                                {
                                    Ok(_) => {}
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            let result = match failure {
                                Some(error) => Err(error),
                                None => Ok(collect_changed_files(&engine, &path_index)),
                            };
                            if reply.send(result).is_err() {
                                zlog::warn!(
                                    "shadow reconciled changed-files requester disconnected"
                                );
                            }
                        }
                        command => {
                            if let Some((path_hash, content_hash)) =
                                handle_command(&engine, command, config.git_commit_hook, &path_index)
                            {
                                suppressed_writes
                                    .insert(path_hash, (content_hash, now + suppression_ttl));
                            }
                        }
                    }
                }

                for (path, trigger) in debouncer.flush_due(std::time::Instant::now()) {
                    route_record_event(
                        &engine,
                        &path,
                        trigger,
                        &mut suppressed_writes,
                        &mut path_index,
                    );
                }

                if path_disconnected {
                    break;
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
    // a trigger. A send failure means the recorder stopped: it is returned as
    // an error so the watcher pipeline logs it instead of dropping events into
    // a closed channel unnoticed.
    let on_event = {
        let path_tx = path_tx.clone();
        let session_id = session_id.to_string();
        move |event: FileEvent| -> Result<SnapshotTrigger> {
            // §4.4 send the path together with its trigger so the recorder can
            // route Delete vs Write without re-reading the (now-absent) file.
            let trigger = event_to_trigger(event.kind, &session_id);
            path_tx
                .send((event.path, trigger))
                .context("shadow recorder channel closed")?;
            Ok(trigger)
        }
    };

    let monitor = Arc::new(Monitor::with_config(root.to_path_buf(), &config, on_event));
    let watch_handle = match watch(monitor.clone(), root.to_path_buf()) {
        Ok(handle) => {
            zlog::info!(
                "shadow snapshot started: session={} cwd={} quota_bytes={} debounce_ms={}",
                session_id,
                cwd,
                config.quota_bytes,
                config.debounce.as_millis(),
            );
            handle
        }
        Err(error) => {
            // Drop the path sender AND the monitor (which holds a clone of
            // path_tx inside its on_event closure) so path_rx sees Disconnected
            // and the recorder thread exits before join.
            drop(path_tx);
            drop(monitor);
            if recorder.join().is_err() {
                zlog::warn!("shadow snapshot recorder panicked after watcher startup failed");
            }
            return Err(error).with_context(|| {
                format!(
                    "shadow snapshot watch_directory: session={} cwd={}",
                    session_id, cwd
                )
            });
        }
    };

    let git_watch_handle = start_git_commit_watch(session_id, root, &config, command_tx.clone());

    Ok(Some(Arc::new(SnapshotWatch {
        inner: Arc::new(WatchInner {
            session_id: session_id.to_string(),
            watch_handle: Mutex::new(Some(watch_handle)),
            git_watch_handle: Mutex::new(git_watch_handle),
            path_sender: Mutex::new(Some(path_tx)),
            command_sender: Mutex::new(Some(command_tx)),
            recorder: Mutex::new(Some(recorder)),
        }),
    })))
}

/// §4.9 Arm git commit detection for the session worktree.
///
/// Returns `None` when the user selected `git_commit_hook: "skip"`, when the
/// cwd is not inside a git repository (shadow snapshot must work in non-git
/// directories, §4.1), or when the watcher could not be installed — none of
/// those are reasons to fail session creation, so they are logged instead.
///
/// The callback runs on the notify watcher thread and therefore must not touch
/// the engine: it only posts a command to the single-writer recorder thread
/// (§4.3), which does the actual GC marking.
fn start_git_commit_watch(
    session_id: &str,
    root: &Path,
    config: &SnapshotConfig,
    command_tx: mpsc::Sender<ShadowCommand>,
) -> Option<GitCommitWatcher> {
    if config.git_commit_hook == GitCommitHookMode::Skip {
        return None;
    }
    let tracker = Arc::new(GitCommitTracker::new(root)?);
    let git_dir = tracker.git_dir().to_path_buf();
    let watch_session_id = session_id.to_string();
    match shadow_snapshot::watch_git_commits(tracker, move |commit| {
        if command_tx
            .send(ShadowCommand::GitCommit { commit })
            .is_err()
        {
            zlog::warn!(
                "shadow git commit hook: recorder gone, session={}",
                watch_session_id,
            );
        }
    }) {
        Ok(watcher) => {
            zlog::info!(
                "shadow git commit hook armed: session={} git_dir={}",
                session_id,
                git_dir.display(),
            );
            Some(watcher)
        }
        Err(error) => {
            zlog::warn!(
                "shadow git commit hook not armed: session={} error={}",
                session_id,
                error,
            );
            None
        }
    }
}

fn handle_review_state_command(
    engine: &shadow_snapshot::ShadowSnapshotEngine,
    path: PathBuf,
    reply: mpsc::Sender<Result<FileReviewState>>,
    debouncer: &mut DebounceQueue,
    suppressed_writes: &mut std::collections::HashMap<
        shadow_snapshot::PathHash,
        (shadow_snapshot::ContentHash, std::time::Instant),
    >,
    path_index: &mut std::collections::HashMap<shadow_snapshot::PathHash, PathBuf>,
) {
    let result = (|| {
        let queued_trigger = debouncer.take(&path);
        let current_content = match std::fs::read(&path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        if !engine.head_matches_content(&path, current_content.as_deref())? {
            let path_hash = shadow_snapshot::compute_path_hash(&path);
            path_index.entry(path_hash).or_insert_with(|| path.clone());
            suppressed_writes.remove(&path_hash);
            match current_content.as_deref() {
                Some(content) => {
                    let trigger = if engine.head_trigger(&path).is_none() {
                        SnapshotTrigger::Create
                    } else {
                        match queued_trigger.unwrap_or(SnapshotTrigger::Write) {
                            SnapshotTrigger::Delete => SnapshotTrigger::Write,
                            trigger => trigger,
                        }
                    };
                    engine.record_change_with_trigger(&path, content, trigger)?;
                }
                None => {
                    engine.record_delete(&path)?;
                }
            }
        }

        let versions = engine.list_versions(&path)?;
        let latest_seq_no = versions.last().map_or(0, |(_, seq_no, _)| *seq_no);
        Ok(FileReviewState {
            versions: versions
                .into_iter()
                .map(|(version_id, seq_no, trigger)| FileVersion {
                    version_id,
                    seq_no,
                    trigger,
                })
                .collect(),
            latest_seq_no,
            current_content,
        })
    })();
    if reply.send(result).is_err() {
        zlog::warn!("shadow review-state requester disconnected");
    }
}

fn collect_changed_files(
    engine: &shadow_snapshot::ShadowSnapshotEngine,
    path_index: &std::collections::HashMap<shadow_snapshot::PathHash, PathBuf>,
) -> Vec<FileChange> {
    engine
        .list_path_summaries()
        .into_iter()
        .filter_map(|(path_hash, version_count, latest_seq_no)| {
            path_index.get(&path_hash).map(|path| FileChange {
                // The review queue classifies rows from these two, so it does
                // not have to fetch review state per file just to tell an
                // added file from a deleted one.
                current_exists: path.exists(),
                first_trigger: engine
                    .list_versions(path)
                    .ok()
                    .and_then(|versions| {
                        versions
                            .first()
                            .map(|(_, _, trigger)| format!("{trigger:?}").to_ascii_lowercase())
                    })
                    .unwrap_or_default(),
                path: path.clone(),
                version_count,
                latest_seq_no,
            })
        })
        .collect()
}

fn handle_command(
    engine: &shadow_snapshot::ShadowSnapshotEngine,
    command: ShadowCommand,
    git_commit_hook: GitCommitHookMode,
    path_index: &std::collections::HashMap<shadow_snapshot::PathHash, PathBuf>,
) -> Option<(shadow_snapshot::PathHash, shadow_snapshot::ContentHash)> {
    match command {
        ShadowCommand::ListChangedFiles { reply } => {
            // A summary whose path_hash is not in the index belongs to a file
            // recorded by an earlier run of this session that no longer exists
            // on disk. Naming it is impossible (the hash is one-way), so it is
            // dropped rather than reported under a placeholder path.
            let changed = collect_changed_files(engine, path_index);
            if reply.send(Ok(changed)).is_err() {
                zlog::warn!("shadow list-changed-files requester disconnected");
            }
            None
        }
        ShadowCommand::ListChangedFilesReconciled { reply } => {
            if reply
                .send(Err(anyhow::anyhow!(
                    "reconciled changed-files command must run in the recorder loop"
                )))
                .is_err()
            {
                zlog::warn!("shadow reconciled changed-files requester disconnected");
            }
            None
        }
        ShadowCommand::ListVersions { path, reply } => {
            let result = engine.list_versions(&path).map(|versions| {
                versions
                    .into_iter()
                    .map(|(version_id, seq_no, trigger)| FileVersion {
                        version_id,
                        seq_no,
                        trigger,
                    })
                    .collect()
            });
            if reply.send(result).is_err() {
                zlog::warn!("shadow list-versions requester disconnected");
            }
            None
        }
        ShadowCommand::GetVersion {
            path,
            version_id,
            reply,
        } => {
            if reply
                .send(engine.query_version_content_for_path(&path, version_id))
                .is_err()
            {
                zlog::warn!("shadow get-version requester disconnected");
            }
            None
        }
        ShadowCommand::GetReviewState { path, reply } => {
            let result = engine.list_versions(&path).and_then(|versions| {
                let latest_seq_no = versions.last().map_or(0, |(_, seq_no, _)| *seq_no);
                let versions = versions
                    .into_iter()
                    .map(|(version_id, seq_no, trigger)| FileVersion {
                        version_id,
                        seq_no,
                        trigger,
                    })
                    .collect();
                let current_content = match std::fs::read(&path) {
                    Ok(content) => Some(content),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                Ok(FileReviewState {
                    versions,
                    latest_seq_no,
                    current_content,
                })
            });
            if reply.send(result).is_err() {
                zlog::warn!("shadow review-state requester disconnected");
            }
            None
        }
        ShadowCommand::Decline {
            path,
            version_id,
            expected_latest_seq_no,
            expected_current_exists,
            expected_current_hash,
            reply,
        } => {
            let result = engine
                .decline_if_current(
                    &path,
                    version_id,
                    expected_latest_seq_no,
                    expected_current_exists,
                    expected_current_hash,
                )
                .map(|result| {
                    let suppression = result.content_hash.map(|content_hash| {
                        (shadow_snapshot::compute_path_hash(&path), content_hash)
                    });
                    (
                        DeclineOutcome {
                            version_id: result.version_id,
                            seq_no: result.seq_no,
                        },
                        suppression,
                    )
                });
            let suppression = result
                .as_ref()
                .ok()
                .and_then(|(_, suppression)| *suppression);
            if reply.send(result.map(|(outcome, _)| outcome)).is_err() {
                zlog::warn!("shadow decline requester disconnected");
            }
            suppression
        }
        ShadowCommand::Checkpoint { reply } => {
            // Rotate the WAL only after every prior entry is persisted; the
            // engine refuses while a decline restore is incomplete (§4.8).
            if reply.send(engine.checkpoint()).is_err() {
                zlog::warn!("shadow checkpoint requester disconnected");
            }
            None
        }
        ShadowCommand::GitCommit { commit } => {
            // §4.9 Clear marks pre-commit deltas gc-eligible; Keep records the
            // boundary but retains everything. Skip never reaches here (no
            // watcher is installed for it).
            match git_commit_hook {
                GitCommitHookMode::Clear => {
                    let marked = engine.on_git_commit();
                    zlog::info!("shadow git commit: commit={} gc_marked={}", commit, marked);
                }
                GitCommitHookMode::Keep | GitCommitHookMode::Skip => {
                    zlog::info!("shadow git commit: commit={} history kept", commit);
                }
            }
            None
        }
    }
}

/// Build path_hash → PathBuf index for decline recovery by walking the session cwd.
/// Matches `shadow_snapshot::compute_path_hash` (blake3 of path.to_string_lossy).
///
/// Traversal errors (permission denied, broken symlink, etc.) are logged with
/// the offending path and root so they cannot be mistaken for a clean omission,
/// but do not halt indexing — remaining entries are still collected.
fn build_path_hash_index(root: &Path) -> std::collections::HashMap<[u8; 32], PathBuf> {
    let mut index = std::collections::HashMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // Do NOT silently drop traversal errors. Each one is at least
                // distinctive in the log so a permission failure is observable
                // and cannot be confused with "this directory is simply empty".
                match error.path() {
                    Some(path) => {
                        zlog::warn!(
                            "shadow snapshot walkdir error: root={} path={} depth={} error={}",
                            root.display(),
                            path.display(),
                            error.depth(),
                            error,
                        );
                    }
                    None => {
                        zlog::warn!(
                            "shadow snapshot walkdir error: root={} depth={} error={}",
                            root.display(),
                            error.depth(),
                            error,
                        );
                    }
                }
                continue;
            }
        };
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
/// removal rather than a content write; close-after-write surfaces as `Close`
/// so §4.7's "file close → force flush version" bypasses the debounce window.
fn event_to_trigger(kind: EventKind, _session_id: &str) -> SnapshotTrigger {
    match kind {
        EventKind::Created | EventKind::Modified | EventKind::Renamed => SnapshotTrigger::Write,
        EventKind::Closed => SnapshotTrigger::Close,
        EventKind::Deleted => SnapshotTrigger::Delete,
    }
}

/// §4.4 Route one filesystem event to the matching engine API.
///
/// Whether this is a delete is decided by reading the file, not by the incoming
/// trigger. The trigger cannot carry that answer on macOS: FSEvents coalesces a
/// path's create/modify/remove flags into one batch and notify emits them in
/// flag order rather than chronological order, so a plain `rm` arrives as
/// `Created, Deleted, Modified` and the debounce queue keeps the trailing
/// `Modified`. The reverse shape (`Deleted, Created, Modified` for a
/// remove-then-recreate) carries the identical set of kinds, so no ordering or
/// priority rule over the events can separate the two cases — only the file
/// itself can. Whatever is on disk when the debounce window closes is the state
/// worth recording.
///
/// Failures are logged, not fatal — a transient I/O error must not halt
/// versioning for the rest of the worktree. Extracted from the recorder loop so
/// the routing decision is unit-testable without a live fs watcher.
fn route_record_event(
    engine: &shadow_snapshot::ShadowSnapshotEngine,
    path: &Path,
    trigger: shadow_snapshot::SnapshotTrigger,
    suppressed_writes: &mut std::collections::HashMap<
        shadow_snapshot::PathHash,
        (shadow_snapshot::ContentHash, std::time::Instant),
    >,
    path_index: &mut std::collections::HashMap<shadow_snapshot::PathHash, PathBuf>,
) {
    let path_hash = shadow_snapshot::compute_path_hash(path);
    // Record the mapping even for a delete: the versions survive the file, and
    // `ListChangedFiles` can only name them through this index.
    path_index
        .entry(path_hash)
        .or_insert_with(|| path.to_path_buf());
    match std::fs::read(path) {
        Ok(content) => {
            match engine.head_matches_content(path, Some(&content)) {
                Ok(true) => {
                    suppressed_writes.remove(&path_hash);
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    zlog::warn!(
                        "shadow snapshot head comparison failed: path={} error={}",
                        path.display(),
                        error,
                    );
                    return;
                }
            }
            if suppressed_writes
                .remove(&path_hash)
                .is_some_and(|(expected_hash, _deadline)| {
                    expected_hash == shadow_snapshot::BlobStore::compute_hash(&content)
                })
            {
                return;
            }
            // A Delete trigger on a path that still has content means the file
            // was removed and recreated inside one debounce window. The content
            // is what survived, so it is versioned as a write; recording it
            // under `Delete` would label a content node as a tombstone.
            let trigger = if engine.head_trigger(path).is_none() {
                shadow_snapshot::SnapshotTrigger::Create
            } else if trigger == shadow_snapshot::SnapshotTrigger::Delete {
                shadow_snapshot::SnapshotTrigger::Write
            } else {
                trigger
            };
            if let Err(error) = engine.record_change_with_trigger(path, &content, trigger) {
                zlog::warn!(
                    "shadow snapshot record failed: path={} error={}",
                    path.display(),
                    error,
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            suppressed_writes.remove(&path_hash);
            match engine.head_matches_content(path, None) {
                Ok(true) => {}
                Ok(false) => record_tombstone(engine, path),
                Err(error) => {
                    zlog::warn!(
                        "shadow snapshot head comparison failed: path={} error={}",
                        path.display(),
                        error,
                    );
                }
            }
        }
        Err(error) => {
            zlog::warn!(
                "shadow snapshot read failed: path={} error={}",
                path.display(),
                error,
            );
        }
    }
}

/// §4.4 Tombstone a path that is gone from disk, unless there is nothing to
/// tombstone.
///
/// A path with no recorded history has no content the tombstone could point
/// back to — editor scratch files that appear and vanish inside one debounce
/// window are the common case, and versioning them would list unrestorable
/// files as changed. A path whose HEAD is already a tombstone was recorded as
/// deleted before; FSEvents re-reports a removed path in later batches, and
/// each repeat would otherwise append another empty node.
fn record_tombstone(engine: &shadow_snapshot::ShadowSnapshotEngine, path: &Path) {
    match engine.head_trigger(path) {
        None | Some(shadow_snapshot::SnapshotTrigger::Delete) => {}
        Some(_) => {
            if let Err(error) = engine.record_delete(path) {
                zlog::warn!(
                    "shadow snapshot delete failed: path={} error={}",
                    path.display(),
                    error,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_mapping_covers_all_kinds() {
        assert_eq!(
            event_to_trigger(EventKind::Created, "s"),
            SnapshotTrigger::Write
        );
        assert_eq!(
            event_to_trigger(EventKind::Modified, "s"),
            SnapshotTrigger::Write
        );
        assert_eq!(
            event_to_trigger(EventKind::Renamed, "s"),
            SnapshotTrigger::Write
        );
        assert_eq!(
            event_to_trigger(EventKind::Deleted, "s"),
            SnapshotTrigger::Delete
        );
    }

    /// §4.4 a delete event must reach `record_delete`, not `record_change`.
    /// Spins a real in-process engine (no fs watcher) and routes one Delete
    /// event through the same function the recorder loop uses, then asserts
    /// the resulting HEAD node carries `trigger=Delete`.
    #[test]
    fn route_record_event_versions_delete_as_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("route.db");
        let wal = dir.path().join("route.wal");
        let blobs = dir.path().join("routeblobs");
        std::fs::create_dir_all(&blobs).unwrap();

        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&db, &wal, &blobs).unwrap();
        let target_file = dir.path().join("victim.txt");
        let _ = engine.record_change(&target_file, b"alive").unwrap();

        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();
        route_record_event(
            &engine,
            &target_file,
            shadow_snapshot::SnapshotTrigger::Delete,
            &mut suppressed_writes,
            &mut path_index,
        );

        let versions = engine.list_versions(&target_file).unwrap();
        let last = versions.last().expect("a node after delete");
        assert_eq!(
            last.2,
            shadow_snapshot::SnapshotTrigger::Delete,
            "delete must be versioned with trigger=Delete, got {:?}",
            last.2
        );
    }

    /// §4.4 the regression this file exists to prevent: on macOS a real `rm`
    /// reaches the recorder carrying `Write`, because FSEvents replays the
    /// path's create/modify flags alongside the remove and the debounce queue
    /// keeps the trailing `Modified`. The file being gone is what decides it,
    /// so a `Write` trigger on a missing path must still tombstone.
    #[test]
    fn route_record_event_tombstones_a_write_trigger_for_a_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("missing.db");
        let wal = directory.path().join("missing.wal");
        let blobs = directory.path().join("missing-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();

        let path = directory.path().join("alpha.txt");
        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();
        std::fs::write(&path, b"alpha one").unwrap();
        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        std::fs::remove_file(&path).unwrap();

        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );

        let versions = engine.list_versions(&path).unwrap();
        assert_eq!(
            versions.len(),
            2,
            "the write and the tombstone are both versions, got {versions:?}"
        );
        assert_eq!(
            versions[1].2,
            shadow_snapshot::SnapshotTrigger::Delete,
            "a Write trigger for a file that is gone must be recorded as a delete, got {:?}",
            versions[1].2
        );
    }

    /// The mirror image, and the reason a "Delete always wins" debounce rule
    /// would be wrong: FSEvents reports remove-then-recreate with the very same
    /// set of event kinds, so the trigger can say `Delete` while the recreated
    /// content is sitting on disk. That content is what must be versioned.
    #[test]
    fn route_record_event_records_content_when_a_delete_trigger_finds_a_live_file() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("recreated.db");
        let wal = directory.path().join("recreated.wal");
        let blobs = directory.path().join("recreated-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();

        let path = directory.path().join("beta.txt");
        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();
        std::fs::write(&path, b"beta one").unwrap();
        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"beta two").unwrap();

        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Delete,
            &mut suppressed_writes,
            &mut path_index,
        );

        let versions = engine.list_versions(&path).unwrap();
        assert_eq!(versions.len(), 2, "got {versions:?}");
        assert_eq!(
            versions[1].2,
            shadow_snapshot::SnapshotTrigger::Write,
            "a live file must be versioned as content, not as a tombstone: {versions:?}"
        );
        let newest = engine
            .query_version_for_path(&path, versions[1].0)
            .unwrap()
            .expect("the newest version has content");
        assert_eq!(
            newest,
            b"beta two",
            "the recreated content must be what got versioned, got {:?}",
            String::from_utf8_lossy(&newest),
        );
    }

    /// Reading the filesystem now decides delete-ness, so every transient file
    /// that vanishes before its debounce window closes reaches the tombstone
    /// path. A path the engine never versioned has no content to point back to,
    /// so it must not become an unrestorable entry in `list_changed_files`.
    #[test]
    fn route_record_event_does_not_tombstone_a_path_without_history() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("transient.db");
        let wal = directory.path().join("transient.wal");
        let blobs = directory.path().join("transient-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();

        let scratch = directory.path().join("editor-scratch-4913");
        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();

        route_record_event(
            &engine,
            &scratch,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );

        assert!(
            engine.list_versions(&scratch).unwrap().is_empty(),
            "a path that was never versioned must not gain a tombstone",
        );
    }

    /// FSEvents re-reports a removed path in later batches, so the same delete
    /// can reach the recorder repeatedly. Only the first one is a state change.
    #[test]
    fn route_record_event_tombstones_a_deleted_file_only_once() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("repeat.db");
        let wal = directory.path().join("repeat.wal");
        let blobs = directory.path().join("repeat-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();

        let path = directory.path().join("gamma.txt");
        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();
        std::fs::write(&path, b"gamma").unwrap();
        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        std::fs::remove_file(&path).unwrap();

        for _ in 0..4 {
            route_record_event(
                &engine,
                &path,
                shadow_snapshot::SnapshotTrigger::Delete,
                &mut suppressed_writes,
                &mut path_index,
            );
        }

        let versions = engine.list_versions(&path).unwrap();
        assert_eq!(
            versions.len(),
            2,
            "repeated delete events must collapse to one tombstone, got {versions:?}"
        );
    }

    /// Full pipeline on a real filesystem with a real watcher: `Monitor` →
    /// trigger channel → `DebounceQueue` → `route_record_event`, wired exactly
    /// as the recorder loop wires it. This is the shape the daemon runs and the
    /// only place the FSEvents event ordering that caused the bug is actually
    /// in play; the unit tests above pin the routing decision, this one proves
    /// a real `rm` still reaches it as a tombstone.
    #[test]
    fn real_watcher_delete_reaches_the_engine_as_a_tombstone() {
        let directory = tempfile::tempdir().expect("temp directory");
        // macOS hands back `/var/...` while the watcher reports `/private/var/...`.
        // Path hashes are computed from the string, so the test has to compare
        // like for like or every lookup misses.
        let root = directory
            .path()
            .canonicalize()
            .expect("canonical temp directory");
        let database = root.join("live.db");
        let wal = root.join("live.wal");
        let blobs = root.join("live-blobs");
        std::fs::create_dir_all(&blobs).expect("blob dir");
        let engine =
            shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).expect("engine");

        let config = SnapshotConfig {
            debounce: Duration::from_millis(100),
            // The engine files live in the watched root, so ignore everything
            // this test is not asserting on.
            ignore_patterns: vec!["live.db".into(), "live.wal".into(), "live-blobs/".into()],
            ..SnapshotConfig::default()
        };
        let (path_tx, path_rx) = mpsc::channel::<(PathBuf, SnapshotTrigger)>();
        let monitor = Arc::new(Monitor::with_config(root.clone(), &config, {
            let path_tx = path_tx.clone();
            move |event: FileEvent| {
                let trigger = event_to_trigger(event.kind, "watcher-delete-test");
                path_tx
                    .send((event.path, trigger))
                    .context("test recorder channel closed")?;
                Ok(trigger)
            }
        }));
        let watch_handle = monitor
            .watch_directory(root.clone())
            .expect("install watcher");

        let target = root.join("alpha.txt");
        std::fs::write(&target, b"alpha one\n").expect("seed file");

        // Wall-clock deadline: a real watcher plus a real debounce window, so
        // every wait has to poll rather than assume a single pump is enough.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut debouncer = DebounceQueue::new(config.debounce);
        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();
        let mut observed_triggers = Vec::new();
        // Everything the recorder loop mutates is passed in rather than
        // captured, so the polling loops below can still read it between pumps.
        let pump = |debouncer: &mut DebounceQueue,
                    suppressed_writes: &mut std::collections::HashMap<_, _>,
                    path_index: &mut std::collections::HashMap<_, _>,
                    observed_triggers: &mut Vec<(PathBuf, SnapshotTrigger)>| {
            while let Ok((path, trigger)) = path_rx.try_recv() {
                observed_triggers.push((path.clone(), trigger));
                debouncer.note(path, trigger, std::time::Instant::now());
            }
            for (path, trigger) in debouncer.flush_due(std::time::Instant::now()) {
                route_record_event(&engine, &path, trigger, suppressed_writes, path_index);
            }
        };

        while std::time::Instant::now() < deadline
            && engine.list_versions(&target).unwrap_or_default().is_empty()
        {
            pump(
                &mut debouncer,
                &mut suppressed_writes,
                &mut path_index,
                &mut observed_triggers,
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !engine.list_versions(&target).unwrap_or_default().is_empty(),
            "the initial write was never versioned; watcher triggers seen: {observed_triggers:?}",
        );

        std::fs::remove_file(&target).expect("delete the watched file");

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let tombstoned = |engine: &shadow_snapshot::ShadowSnapshotEngine| {
            engine
                .list_versions(&target)
                .unwrap_or_default()
                .last()
                .is_some_and(|version| version.2 == SnapshotTrigger::Delete)
        };
        while std::time::Instant::now() < deadline && !tombstoned(&engine) {
            pump(
                &mut debouncer,
                &mut suppressed_writes,
                &mut path_index,
                &mut observed_triggers,
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        let versions = engine.list_versions(&target).unwrap_or_default();
        assert!(
            tombstoned(&engine),
            "deleting a watched file must leave a Delete version. versions={versions:?}, \
             watcher triggers seen={observed_triggers:?}",
        );
        assert!(
            observed_triggers
                .iter()
                .any(|(path, trigger)| path == &target && *trigger == SnapshotTrigger::Delete),
            "the watcher itself must report the removal; it did not: {observed_triggers:?}",
        );

        drop(watch_handle);
    }

    #[test]
    fn route_record_event_consumes_matching_decline_write_once() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("suppression.db");
        let wal = directory.path().join("suppression.wal");
        let blobs = directory.path().join("suppression-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();
        let path = directory.path().join("declined.txt");
        std::fs::write(&path, b"restored").unwrap();
        let path_hash = shadow_snapshot::compute_path_hash(&path);
        let content_hash = shadow_snapshot::BlobStore::compute_hash(b"restored");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut suppressed_writes =
            std::collections::HashMap::from([(path_hash, (content_hash, deadline))]);
        let mut path_index = std::collections::HashMap::new();

        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );

        assert!(engine.list_versions(&path).unwrap().is_empty());
        assert!(suppressed_writes.is_empty());

        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        assert_eq!(engine.list_versions(&path).unwrap().len(), 1);
    }

    #[test]
    fn route_record_event_keeps_user_edit_that_differs_from_decline() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("user-edit.db");
        let wal = directory.path().join("user-edit.wal");
        let blobs = directory.path().join("user-edit-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();
        let path = directory.path().join("edited.txt");
        std::fs::write(&path, b"user edit").unwrap();
        let path_hash = shadow_snapshot::compute_path_hash(&path);
        let declined_hash = shadow_snapshot::BlobStore::compute_hash(b"restored");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut suppressed_writes =
            std::collections::HashMap::from([(path_hash, (declined_hash, deadline))]);
        let mut path_index = std::collections::HashMap::new();

        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );

        assert_eq!(engine.list_versions(&path).unwrap().len(), 1);
        assert!(suppressed_writes.is_empty());
    }

    /// A checkpoint command must reach the recorder's engine, rotate the WAL,
    /// and carry the result back through the reply channel.
    #[test]
    fn handle_command_checkpoint_rotates_wal() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("command-checkpoint.db");
        let wal_path = directory.path().join("command-checkpoint.wal");
        let blobs = directory.path().join("command-checkpoint-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine =
            shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal_path, &blobs).unwrap();
        let path = directory.path().join("checked.txt");
        engine.record_change(&path, b"v1").unwrap();

        let (reply, response) = mpsc::channel();
        let suppression = handle_command(
            &engine,
            ShadowCommand::Checkpoint { reply },
            GitCommitHookMode::Skip,
            &std::collections::HashMap::new(),
        );
        assert!(suppression.is_none(), "checkpoint must not suppress writes");
        assert!(
            response.recv().unwrap().is_ok(),
            "checkpoint reply must be Ok"
        );

        let wal = shadow_snapshot::Wal::open(&wal_path).unwrap();
        assert!(
            wal.replay().unwrap().is_empty(),
            "checkpoint command must rotate the WAL"
        );
    }

    /// 在 recorder 空闲时直接向该会话的 WAL 追加两条已持久化风格的条目,
    /// 供命令 / 关停测试观察 checkpoint 是否真正旋转。会话目录与 WAL 在
    /// `$LOCAL_DATA`,不在被 watch 的 cwd 下,不会触发 fs 事件。
    fn seed_wal_entries(session_id: &str) {
        let wal_path = session_shadow_dir(session_id).join("wal.bin");
        let wal = shadow_snapshot::Wal::open(&wal_path).unwrap();
        for seq in 1..=2u64 {
            wal.append(&shadow_snapshot::WalEntry {
                seq_no: seq,
                path_hash: [seq as u8; 32],
                parent_id: None,
                content_ref: None,
                delta_ref: None,
                trigger: shadow_snapshot::SnapshotTrigger::Write,
            })
            .unwrap();
        }
        wal.commit().unwrap();
    }

    /// 清理该会话的存储目录,让测试从确定性的空状态开始(会话 id 是测试
    /// 专用,目录里只可能有先前失败运行留下的残留)。
    fn reset_session_dir(session_id: &str) {
        let dir = session_shadow_dir(session_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// 用真实 fs watcher 启动一个会话的 snapshot 子系统。
    fn armed_watch(session_id: &str, cwd: &str) -> Arc<SnapshotWatch> {
        start_with_config(
            session_id,
            cwd,
            SnapshotConfig::default(),
            |monitor, root| monitor.watch_directory(root),
        )
        .expect("start succeeds")
        .expect("watch armed")
    }

    /// §4 explicit checkpoint command: rotates the WAL on the recorder thread
    /// and surfaces success to the caller.
    #[test]
    fn checkpoint_command_rotates_wal_and_replies() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "checkpoint-command-test";
        reset_session_dir(session_id);
        let watch = armed_watch(session_id, &cwd);

        seed_wal_entries(session_id);
        watch
            .checkpoint()
            .expect("explicit checkpoint must succeed");

        let wal_path = session_shadow_dir(session_id).join("wal.bin");
        let wal = shadow_snapshot::Wal::open(&wal_path).unwrap();
        assert!(
            wal.replay().unwrap().is_empty(),
            "checkpoint command must rotate the WAL empty"
        );
    }

    /// A checkpoint that cannot rotate (blocked archive path) must surface
    /// its error to the command caller and leave the WAL fully replayable.
    #[test]
    fn checkpoint_command_surfaces_rotation_failure() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "checkpoint-failure-test";
        reset_session_dir(session_id);
        let watch = armed_watch(session_id, &cwd);

        seed_wal_entries(session_id);
        std::fs::create_dir(session_shadow_dir(session_id).join("wal.bin.old")).unwrap();

        let error = watch
            .checkpoint()
            .expect_err("blocked rotation must surface");
        assert!(
            error.to_string().contains("checkpoint"),
            "error must name the failing checkpoint: {error:#}"
        );

        let wal_path = session_shadow_dir(session_id).join("wal.bin");
        let wal = shadow_snapshot::Wal::open(&wal_path).unwrap();
        assert_eq!(
            wal.replay().unwrap().len(),
            2,
            "a failed checkpoint must leave the WAL replayable"
        );
    }

    /// Graceful shutdown runs a final checkpoint on the recorder thread
    /// before the channels are torn down: after stop() returns, the WAL is
    /// rotated empty.
    #[test]
    fn stop_runs_final_checkpoint_and_rotates_wal() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "stop-checkpoint-test";
        reset_session_dir(session_id);
        let watch = armed_watch(session_id, &cwd);

        seed_wal_entries(session_id);
        watch.stop();

        let wal_path = session_shadow_dir(session_id).join("wal.bin");
        let wal = shadow_snapshot::Wal::open(&wal_path).unwrap();
        assert!(
            wal.replay().unwrap().is_empty(),
            "stop must run the final checkpoint and rotate the WAL"
        );
    }

    /// A final checkpoint that cannot rotate must not abort teardown and must
    /// not discard the WAL: stop() still completes, entries stay replayable.
    #[test]
    fn stop_with_blocked_rotation_preserves_wal_and_completes() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "stop-blocked-checkpoint-test";
        reset_session_dir(session_id);
        let watch = armed_watch(session_id, &cwd);

        seed_wal_entries(session_id);
        std::fs::create_dir(session_shadow_dir(session_id).join("wal.bin.old")).unwrap();
        watch.stop();

        let wal_path = session_shadow_dir(session_id).join("wal.bin");
        let wal = shadow_snapshot::Wal::open(&wal_path).unwrap();
        assert_eq!(
            wal.replay().unwrap().len(),
            2,
            "a failed final checkpoint must not discard the WAL"
        );
    }

    /// §4 `ListChangedFiles` has to name files the engine only knows by hash.
    /// Routing writes through `route_record_event` is what populates that
    /// reverse index, so recording two files must make both listable with the
    /// most recently touched one first.
    #[test]
    fn list_changed_files_names_paths_recorded_after_startup() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("changed.db");
        let wal = directory.path().join("changed.wal");
        let blobs = directory.path().join("changed-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();

        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        let mut suppressed_writes = std::collections::HashMap::new();
        // The index starts empty on purpose: neither file existed when the
        // recorder booted, which is exactly the case a startup-only walk misses.
        let mut path_index = std::collections::HashMap::new();

        std::fs::write(&first, b"one").unwrap();
        route_record_event(
            &engine,
            &first,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        std::fs::write(&first, b"one edited").unwrap();
        route_record_event(
            &engine,
            &first,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        std::fs::write(&second, b"two").unwrap();
        route_record_event(
            &engine,
            &second,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );

        let (reply, response) = mpsc::channel();
        let suppression = handle_command(
            &engine,
            ShadowCommand::ListChangedFiles { reply },
            GitCommitHookMode::Skip,
            &path_index,
        );
        assert!(suppression.is_none());
        let changed = response.recv().unwrap().unwrap();

        assert_eq!(changed.len(), 2, "both recorded files must be listed");
        assert_eq!(changed[0].path, second, "newest SeqNo comes first");
        assert_eq!(changed[0].version_count, 1);
        assert_eq!(changed[1].path, first);
        assert_eq!(changed[1].version_count, 2, "two writes, two versions");
        assert!(changed[0].latest_seq_no > changed[1].latest_seq_no);
    }

    /// A deleted file keeps its versions, so it must stay listable — the index
    /// entry is what makes the tombstone reachable for restore.
    #[test]
    fn list_changed_files_still_names_a_deleted_file() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("deleted.db");
        let wal = directory.path().join("deleted.wal");
        let blobs = directory.path().join("deleted-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let engine = shadow_snapshot::ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();

        let path = directory.path().join("gone.txt");
        let mut suppressed_writes = std::collections::HashMap::new();
        let mut path_index = std::collections::HashMap::new();

        std::fs::write(&path, b"here").unwrap();
        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Write,
            &mut suppressed_writes,
            &mut path_index,
        );
        std::fs::remove_file(&path).unwrap();
        route_record_event(
            &engine,
            &path,
            shadow_snapshot::SnapshotTrigger::Delete,
            &mut suppressed_writes,
            &mut path_index,
        );

        let (reply, response) = mpsc::channel();
        handle_command(
            &engine,
            ShadowCommand::ListChangedFiles { reply },
            GitCommitHookMode::Skip,
            &path_index,
        );
        let changed = response.recv().unwrap().unwrap();

        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, path);
        assert_eq!(
            changed[0].version_count, 2,
            "the write and the tombstone are both versions"
        );
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

    #[test]
    fn watch_directory_failure_joins_recorder_without_deadlock() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();

        let result = start_with_config(
            "fail-watch-test",
            &cwd,
            SnapshotConfig::default(),
            |_monitor, _root| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected watcher failure",
                ))
            },
        );

        assert!(
            result.is_err(),
            "injected watch failure must surface as Err"
        );
    }

    /// `shadow_snapshot.enabled = false` must stop the engine from being armed
    /// at all: no storage directory, no watcher, no recorder thread.
    #[test]
    fn disabled_setting_prevents_engine_start() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "shadow-disabled-test";
        let config = SnapshotConfig {
            enabled: false,
            ..SnapshotConfig::default()
        };

        let watch = start_with_config(session_id, &cwd, config, |_monitor, _root| {
            panic!("a disabled engine must never install a watcher")
        })
        .expect("disabled settings are not an error");

        assert!(watch.is_none(), "disabled settings must not arm a session");
        assert!(
            !session_shadow_dir(session_id).exists(),
            "disabled settings must not create session storage",
        );
    }

    /// The settings the UI writes must reach the engine's runtime config: a
    /// non-default quota, debounce, circuit breaker K and git hook mode all
    /// have to survive the JSON → SnapshotConfig translation.
    #[test]
    fn settings_json_drives_runtime_config() {
        let config = parse_settings(
            r#"{
                // z3rm settings
                "mux": { "keep_alive": true },
                "shadow_snapshot": {
                    "enabled": true,
                    "quota_mode": "global",
                    "per_project_quota_mb": 12,
                    "ignore_patterns": ["*.generated.rs"],
                    "binary_detection": false,
                    "debounce_ms": 50,
                    "frequency_circuit_breaker_k": 3,
                    "git_commit_hook": "keep",
                },
            }"#,
        )
        .expect("settings parse");

        assert!(config.enabled);
        assert_eq!(config.quota_mode, QuotaMode::Global);
        assert_eq!(config.quota_bytes, 12 * 1024 * 1024);
        assert_eq!(config.ignore_patterns, vec!["*.generated.rs".to_string()]);
        assert!(!config.binary_detection);
        assert_eq!(config.debounce, Duration::from_millis(50));
        assert_eq!(config.circuit_breaker_writes_per_second, 3.0);
        assert_eq!(config.git_commit_hook, GitCommitHookMode::Keep);
    }

    /// A missing section, a missing field, and `enabled: false` must all behave
    /// predictably: documented defaults except for what the user actually set.
    #[test]
    fn settings_json_partial_sections_keep_documented_defaults() {
        let defaults = parse_settings(r#"{ "mux": {} }"#).expect("settings parse");
        assert!(defaults.enabled);
        assert_eq!(defaults.quota_bytes, 500 * 1024 * 1024);
        assert_eq!(defaults.debounce, Duration::from_millis(500));
        assert_eq!(defaults.circuit_breaker_writes_per_second, 10.0);

        let partial = parse_settings(r#"{ "shadow_snapshot": { "enabled": false } }"#)
            .expect("settings parse");
        assert!(!partial.enabled);
        assert_eq!(partial.quota_bytes, 500 * 1024 * 1024);
        assert_eq!(partial.debounce, Duration::from_millis(500));

        // 0 MB is the documented "unlimited" value, not "fall back to 500MB".
        let unlimited = parse_settings(r#"{ "shadow_snapshot": { "per_project_quota_mb": 0 } }"#)
            .expect("settings parse");
        assert_eq!(unlimited.quota_bytes, 0);
        assert!(unlimited.quota_manager().is_none());
    }

    /// The shipped default settings file is JSONC (comments); the daemon parser
    /// has to accept exactly the file the settings UI maintains.
    #[test]
    fn shipped_default_settings_parse_with_documented_values() {
        let repository_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|crates| crates.parent())
            .expect("workspace root");
        let defaults = repository_root.join("assets/settings/default.json");
        let contents = std::fs::read_to_string(&defaults)
            .unwrap_or_else(|error| panic!("read {}: {error}", defaults.display()));

        let config = parse_settings(&contents).expect("default settings parse");

        assert!(config.enabled);
        assert_eq!(config.quota_mode, QuotaMode::PerProject);
        assert_eq!(config.quota_bytes, 500 * 1024 * 1024);
        assert!(config.binary_detection);
        assert_eq!(config.debounce, Duration::from_millis(500));
        assert_eq!(config.circuit_breaker_writes_per_second, 10.0);
        assert_eq!(config.git_commit_hook, GitCommitHookMode::Clear);
    }

    #[test]
    fn review_state_reconciles_current_bytes_before_reply_without_duplicates() {
        let directory = tempfile::tempdir().expect("temp directory");
        let unrelated_watch_root = directory.path().join("unrelated");
        std::fs::create_dir(&unrelated_watch_root).expect("create unrelated watch root");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "review-state-reconcile-test";
        reset_session_dir(session_id);
        let config = SnapshotConfig {
            debounce: Duration::from_secs(30),
            ..SnapshotConfig::default()
        };
        let watch = start_with_config(session_id, &cwd, config, move |monitor, _root| {
            monitor.watch_directory(unrelated_watch_root)
        })
        .expect("start snapshot")
        .expect("snapshot enabled");
        let path = directory.path().join("same-size.txt");

        std::fs::write(&path, b"one").expect("write first content");
        let first = watch
            .get_review_state(path.clone())
            .expect("capture first review state");
        assert_eq!(first.current_content.as_deref(), Some(b"one".as_slice()));
        assert_eq!(first.versions.len(), 1);

        std::fs::write(&path, b"two").expect("write intervening same-size content");
        let second = watch
            .get_review_state(path.clone())
            .expect("capture reconciled review state");
        assert_eq!(second.current_content.as_deref(), Some(b"two".as_slice()));
        assert_eq!(second.versions.len(), 2, "{:?}", second.versions);
        assert!(second.latest_seq_no > first.latest_seq_no);

        let unchanged = watch
            .get_review_state(path)
            .expect("capture unchanged review state");
        assert_eq!(unchanged.versions.len(), 2);
        assert_eq!(unchanged.latest_seq_no, second.latest_seq_no);
    }

    #[test]
    fn changed_files_reconciles_current_bytes_before_listing() {
        let directory = tempfile::tempdir().expect("temp directory");
        let unrelated_watch_root = directory.path().join("unrelated");
        std::fs::create_dir(&unrelated_watch_root).expect("create unrelated watch root");
        let cwd = directory.path().to_str().expect("utf-8 cwd").to_string();
        let session_id = "changed-files-reconcile-test";
        reset_session_dir(session_id);
        let config = SnapshotConfig {
            debounce: Duration::from_secs(30),
            ..SnapshotConfig::default()
        };
        let watch = start_with_config(session_id, &cwd, config, move |monitor, _root| {
            monitor.watch_directory(unrelated_watch_root)
        })
        .expect("start snapshot")
        .expect("snapshot enabled");
        let path = directory.path().join("listed.txt");

        std::fs::write(&path, b"one").expect("write first content");
        watch
            .get_review_state(path.clone())
            .expect("seed reviewed path");
        std::fs::write(&path, b"two").expect("write unseen content");

        let changed = watch
            .list_changed_files_reconciled()
            .expect("list reconciled files");
        let listed = changed
            .iter()
            .find(|change| change.path == path)
            .expect("changed file listed");
        assert_eq!(listed.version_count, 2, "{changed:?}");
        let state = watch.get_review_state(path).expect("read reconciled state");
        assert_eq!(state.current_content.as_deref(), Some(b"two".as_slice()));
        assert_eq!(state.versions.len(), 2);
    }

    #[test]
    fn jsonc_stripping_preserves_string_contents() {
        let stripped = strip_jsonc(
            r#"{
                "a": "http://example.com//not-a-comment",
                /* block */ "b": "sl\"ash // inside",
                "c": [1, 2,], // trailing
            }"#,
        );
        let value: serde_json::Value = serde_json::from_str(&stripped).expect("valid JSON");
        assert_eq!(value["a"], "http://example.com//not-a-comment");
        assert_eq!(value["b"], "sl\"ash // inside");
        assert_eq!(value["c"], serde_json::json!([1, 2]));
    }
}
