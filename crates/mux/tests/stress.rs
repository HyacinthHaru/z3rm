//! # Mux stress tests
//!
//! These drive a real `z3rm-server` subprocess hard enough to expose the
//! failure modes that only show up under load: a generation counter that goes
//! backwards, a notification channel that drops lifecycle events when it fills,
//! scrollback that hands back rows it has already evicted, and connections that
//! leak across repeated attach/detach.
//!
//! Spec §15.5 sets the throughput target (sustained single-pane output above
//! 50 MB/s). The assertions here are about *correctness under load* rather than
//! absolute numbers: a throughput figure measured on a loaded CI box is noise,
//! but "the client always converges on the server's state" holds regardless of
//! how fast the machine is.

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto;
use mux_protocol::proto::envelope::Payload as EnvelopePayload;
use mux_protocol::proto::request::Body as RequestBody;
use mux_protocol::proto::split_node::SplitDirection;
use mux_protocol::{Envelope, Request};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Isolated `z3rm-server` process bound to a socket inside its own temp dir.
struct TestServer {
    child: std::process::Child,
    socket_path: PathBuf,
    _tmp: TempDir,
}

impl TestServer {
    fn spawn() -> Result<Self> {
        let tmp = tempfile::tempdir().context("create temp dir")?;
        let socket_path = tmp.path().join("mux.sock");
        let db_path = tmp.path().join("mux.db");

        let exe = std::env::var("Z3RM_SERVER_BIN").ok().unwrap_or_else(|| {
            let manifest = std::env::var("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."));
            let candidates = [
                manifest.join("../../target/debug/z3rm-server"),
                manifest.join("../../target/release/z3rm-server"),
            ];
            for candidate in &candidates {
                if candidate.exists() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
            "z3rm-server".to_string()
        });

        let child = std::process::Command::new(&exe)
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn z3rm-server at {exe}"))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket_path.exists() {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "z3rm-server failed to bind socket at {} within 10s",
                    socket_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));

        Ok(Self {
            child,
            socket_path,
            _tmp: tmp,
        })
    }

    async fn connect(&self) -> Result<MuxDomain> {
        mux::connect_local(Some(self.socket_path.as_path()))
            .await
            .context("connect_local failed")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("failed to kill stress mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap stress mux server: {error}");
        }
    }
}

/// Panes live under tabs in a snapshot; the stress assertions care about the
/// session-wide count.
fn snapshot_pane_ids(snapshot: &proto::SessionSnapshot) -> Vec<String> {
    snapshot
        .tabs
        .iter()
        .flat_map(|tab| tab.panes.iter().map(|pane| pane.id.clone()))
        .collect()
}

fn shell(program: &str, args: &[&str]) -> proto::ShellCommand {
    proto::ShellCommand {
        program: program.to_string(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        env: Default::default(),
    }
}

fn size(cols: u32, rows: u32) -> proto::TerminalSize {
    proto::TerminalSize { cols, rows }
}

/// Open a session with one tab and return `(session_id, tab_id)`.
async fn open_session(domain: &MuxDomain, name: &str) -> Result<(String, String)> {
    let session_id = domain
        .create_session(name, std::path::Path::new("/tmp"))
        .await?;
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach
        .snapshot
        .as_ref()
        .context("attach returned no snapshot")?
        .tabs
        .first()
        .context("session has no tab")?
        .id
        .clone();
    Ok((session_id, tab_id))
}

/// Poll `fetch_grid_update` until `predicate` holds, or fail with the last
/// generation seen. Grid convergence is asynchronous: the server publishes a
/// generation, pushes `PaneDirty`, and the client pulls — under load the pull
/// can lag several publishes behind.
async fn wait_for_generation(
    domain: &MuxDomain,
    pane_id: &str,
    predicate: impl Fn(u64) -> bool,
    timeout: Duration,
    what: &str,
) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = domain.fetch_grid_update(pane_id, 0).await?;
        let generation = response.to_generation;
        if predicate(generation) {
            return Ok(generation);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for {what}; last generation was {generation}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Render a grid update (diff or full snapshot) as plain text, row by row.
/// Used to assert on the *content* the server hands back, not just the
/// generation counter.
fn grid_text(update: &proto::fetch_grid_update_response::Update) -> String {
    use proto::fetch_grid_update_response::Update;
    match update {
        Update::Diff(diff) => {
            let mut rows: std::collections::BTreeMap<u32, String> = Default::default();
            for row_change in &diff.rows {
                let line: String = row_change
                    .cells
                    .iter()
                    .map(|cell| cell.char.clone())
                    .collect();
                rows.insert(row_change.row, line);
            }
            rows.into_values().collect::<Vec<_>>().join("\n")
        }
        Update::FullSnapshot(snapshot) => {
            let cols = snapshot.cols as usize;
            if cols == 0 {
                return String::new();
            }
            let mut out = String::new();
            for (i, cell) in snapshot.cells.iter().enumerate() {
                out.push_str(&cell.char);
                if (i + 1) % cols == 0 {
                    out.push('\n');
                }
            }
            out
        }
    }
}

/// Poll `fetch_grid_update(0)` until the generation holds still for 400ms,
/// then return it. Used with bounded floods (`yes | head -n N; cat`) so the
/// test can assert against a settled, deterministic end state instead of a
/// racing wall clock.
async fn wait_for_settled_generation(
    domain: &MuxDomain,
    pane_id: &str,
    timeout: Duration,
    what: &str,
) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<(u64, Instant)> = None;
    loop {
        let response = domain.fetch_grid_update(pane_id, 0).await?;
        let generation = response.to_generation;
        if let Some((previous, at)) = last {
            if previous == generation {
                // Same generation as the previous sample: this is the start of
                // a stable run, not a fresh observation — keep the original
                // timestamp so the window can actually elapse.
                if at.elapsed() >= Duration::from_millis(400) {
                    return Ok(generation);
                }
            } else {
                last = Some((generation, Instant::now()));
            }
        } else {
            last = Some((generation, Instant::now()));
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for {what}; last generation was {generation}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// What a subscriber drain task has observed so far. Keyed so assertions can
/// name exactly which notification was missed.
#[derive(Default)]
struct NotificationTally {
    pane_dirty: HashMap<String, usize>,
    pane_added: HashSet<String>,
    layout_changed: usize,
}

/// Poll a `NotificationTally` until `predicate` holds or the deadline passes.
/// Delivery across the wire is asynchronous; a direct `assert!` right after
/// the triggering RPC would race the server's fan-out.
async fn wait_for_tally(
    tally: &Arc<parking_lot::Mutex<NotificationTally>>,
    timeout: Duration,
    mut predicate: impl FnMut(&NotificationTally) -> bool,
    what: &str,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let observed = tally.lock();
            if predicate(&observed) {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            let observed = tally.lock();
            anyhow::bail!(
                "timed out waiting for {what}; tally: dirty={:?} added={:?} layout_changed={}",
                observed.pane_dirty,
                observed.pane_added,
                observed.layout_changed
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// §3.3 A pane under sustained output must keep its generation monotonic, and a
/// client that keeps pulling must always converge on the latest one.
///
/// A generation that repeats or goes backwards would let the client cache a
/// stale grid forever, because `fetch_grid_update(since)` would answer
/// `NoChange` for state that had in fact moved on.
#[tokio::test(flavor = "multi_thread")]
async fn sustained_output_keeps_generations_monotonic() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, tab_id) = open_session(&domain, "throughput").await?;

    // `yes` is the cheapest way to saturate a PTY: no syscall per line beyond
    // the write itself, so the server side is the bottleneck under test.
    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            size(200, 50),
            Some(shell("/usr/bin/yes", &["z3rm-stress-line"])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    let mut previous = 0u64;
    let mut samples = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let response = domain.fetch_grid_update(&pane_id, previous).await?;
        let current = response.to_generation;
        anyhow::ensure!(
            current >= previous,
            "generation went backwards: {previous} then {current}"
        );
        if current > previous {
            samples += 1;
            previous = current;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    anyhow::ensure!(
        samples > 10,
        "expected the pane to advance many generations under `yes`, saw {samples}"
    );

    // The stream is still running: a fetch from generation 0 must still return
    // a full snapshot rather than an error or an empty update.
    let full = domain.fetch_grid_update(&pane_id, 0).await?;
    anyhow::ensure!(
        full.update.is_some(),
        "a from-scratch fetch during heavy output returned no update"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §3.4 Lifecycle notifications are at-least-once. Creating many panes quickly
/// must not drop a single `PaneAdded`, or the client ends up rendering fewer
/// panes than the server owns.
#[tokio::test(flavor = "multi_thread")]
async fn many_panes_deliver_every_lifecycle_event() -> Result<()> {
    const PANE_COUNT: usize = 40;

    let server = TestServer::spawn()?;
    let domain = Arc::new(server.connect().await?);
    let (session_id, tab_id) = open_session(&domain, "many-panes").await?;

    let notifications = domain.subscribe();

    let mut spawned = Vec::with_capacity(PANE_COUNT);
    for _ in 0..PANE_COUNT {
        let pane_id = domain
            .spawn_pane(
                &session_id,
                &tab_id,
                size(80, 24),
                Some(shell("/bin/cat", &[])),
                Some(std::path::Path::new("/tmp")),
            )
            .await?;
        spawned.push(pane_id);
    }

    // Drain until every spawned pane has been announced, or time out with the
    // ones still missing so a failure names them.
    let mut announced = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while announced.len() < spawned.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "PaneAdded never arrived for {} of {} panes",
            spawned.len() - announced.len(),
            spawned.len()
        );
        match tokio::time::timeout(remaining, notifications.recv()).await {
            Ok(Ok(notification)) => {
                if let Some(proto::notification::Event::PaneAdded(added)) = notification.event {
                    announced.insert(added.pane_id);
                }
            }
            Ok(Err(error)) => anyhow::bail!("notification channel closed: {error}"),
            Err(_) => continue,
        }
    }

    for pane_id in &spawned {
        anyhow::ensure!(
            announced.contains(pane_id),
            "pane {pane_id} was created but never announced"
        );
    }

    // The layout must agree with what we asked for: one tab holding the
    // original pane plus everything spawned into it.
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let snapshot = attach.snapshot.context("reattach returned no snapshot")?;
    let listed = snapshot_pane_ids(&snapshot);
    anyhow::ensure!(
        listed.len() >= spawned.len(),
        "snapshot lists {} panes, expected at least {}",
        listed.len(),
        spawned.len()
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §15.4 Repeated attach/detach must not accumulate state on the server. A
/// leaked client registration shows up as a snapshot that keeps growing, or as
/// a server that eventually refuses new connections.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_attach_detach_does_not_leak() -> Result<()> {
    const CYCLES: usize = 30;

    let server = TestServer::spawn()?;
    let owner = server.connect().await?;
    let (session_id, tab_id) = open_session(&owner, "attach-cycles").await?;
    owner
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell("/bin/cat", &[])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    let baseline = snapshot_pane_ids(
        &owner
            .attach(&session_id, AttachMode::Shared)
            .await?
            .snapshot
            .context("baseline snapshot")?,
    )
    .len();

    for cycle in 0..CYCLES {
        let client = server.connect().await?;
        let snapshot = client
            .attach(&session_id, AttachMode::Shared)
            .await?
            .snapshot
            .with_context(|| format!("cycle {cycle} returned no snapshot"))?;
        let listed = snapshot_pane_ids(&snapshot).len();
        anyhow::ensure!(
            listed == baseline,
            "cycle {cycle} saw {listed} panes, expected {baseline}"
        );
        client.detach().await?;
        drop(client);
    }

    // The session must still be usable by the original client after all that.
    let final_snapshot = owner
        .attach(&session_id, AttachMode::Shared)
        .await?
        .snapshot
        .context("final snapshot")?;
    let final_count = snapshot_pane_ids(&final_snapshot).len();
    anyhow::ensure!(
        final_count == baseline,
        "after {CYCLES} attach cycles the session holds {final_count} panes, expected {baseline}"
    );

    owner.kill_session(&session_id).await?;
    Ok(())
}

/// §16.4 Writing far past the scrollback capacity must evict old rows without
/// ever handing back a row outside the range the server reports.
///
/// Returning rows past `total_lines` would make the client render history the
/// server no longer has, and an off-by-one there is invisible until it isn't.
#[tokio::test(flavor = "multi_thread")]
async fn scrollback_eviction_never_returns_out_of_range_rows() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, tab_id) = open_session(&domain, "scrollback").await?;

    // seq writes a bounded, ordered stream, so the amount of history produced
    // is predictable regardless of how fast the machine drains the PTY. The
    // trailing `cat` keeps the pane alive afterwards — a pane whose process has
    // exited is reaped, and its scrollback goes with it.
    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell("/bin/sh", &["-c", "seq 1 50000; cat"])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    wait_for_generation(
        &domain,
        &pane_id,
        |generation| generation > 0,
        Duration::from_secs(20),
        "the first grid publication",
    )
    .await?;

    // Let the writer finish and the server settle.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let probe = domain.fetch_scrollback(&pane_id, 0, 1, 0).await?;
    let total = probe.total_lines;
    anyhow::ensure!(total > 0, "writing 50k lines produced no scrollback at all");

    // Ask for a window that runs off the end: the server must clamp rather than
    // invent rows.
    let overshoot = domain
        .fetch_scrollback(&pane_id, total.saturating_sub(10), 1, 1000)
        .await?;
    anyhow::ensure!(
        overshoot.lines.len() <= 10,
        "asking for 1000 rows near the end returned {}, which runs past total_lines {total}",
        overshoot.lines.len()
    );

    // A start index at or past the end has no rows to give.
    let past_end = domain.fetch_scrollback(&pane_id, total, 1, 16).await?;
    anyhow::ensure!(
        past_end.lines.is_empty(),
        "fetching from total_lines ({total}) returned {} rows",
        past_end.lines.len()
    );

    // History is bounded: the server must have evicted, not retained all 50k.
    anyhow::ensure!(
        total < 50_000,
        "scrollback kept {total} rows, so eviction never ran"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §3.3 Splitting and closing panes in a tight loop must leave the layout tree
/// consistent with the pane registry — a split that half-applies leaves a
/// zombie node that renders as an empty region forever.
#[tokio::test(flavor = "multi_thread")]
async fn split_and_close_churn_keeps_layout_consistent() -> Result<()> {
    const ROUNDS: usize = 25;

    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, tab_id) = open_session(&domain, "layout-churn").await?;

    let root = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            size(120, 40),
            Some(shell("/bin/cat", &[])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    for round in 0..ROUNDS {
        let direction = if round % 2 == 0 {
            SplitDirection::LeftRight
        } else {
            SplitDirection::TopBottom
        };
        let child = domain.split_pane(&root, direction).await?;
        domain.close_pane(&child).await?;
    }

    let snapshot = domain
        .attach(&session_id, AttachMode::Shared)
        .await?
        .snapshot
        .context("snapshot after churn")?;
    let surviving = snapshot_pane_ids(&snapshot);
    anyhow::ensure!(
        surviving.len() == 1,
        "after {ROUNDS} split/close rounds the session holds {} panes, expected 1",
        surviving.len()
    );
    anyhow::ensure!(
        surviving.first().map(String::as_str) == Some(root.as_str()),
        "the surviving pane is not the one that was never closed"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// Poll until the domain reports its connection dead. After a SIGKILL the
/// client's IO worker must notice EOF/broken pipe promptly and fail pending
/// requests — never leave them hanging on the 15s request timeout.
async fn wait_for_dead_connection(domain: &MuxDomain, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if !domain.check_connection().await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("connection still reports live {timeout:?} after the server was killed");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// §15.4 / §3.5 Killing the server mid-session (`kill -9` semantics) must make
/// in-flight clients fail loudly, and a fresh server instance must recover to
/// a clean slate — no leaked state from the dead process, no client that
/// mistakes a dead transport for an empty success.
#[tokio::test(flavor = "multi_thread")]
async fn killed_server_fails_loudly_and_fresh_instance_recovers() -> Result<()> {
    let mut server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, _tab_id) = open_session(&domain, "killed-server").await?;
    anyhow::ensure!(
        domain.check_connection().await,
        "connection must be live before the kill"
    );

    // SIGKILL mid-session, exactly like `kill -9`: no graceful detach.
    server.child.kill().context("SIGKILL the mux server")?;
    server.child.wait().context("reap the killed mux server")?;

    // Requests on the dead connection must surface an error — a silent empty
    // response or a hang would leave the UI pointing at a ghost server.
    wait_for_dead_connection(&domain, Duration::from_secs(5)).await?;
    let list = domain.list_sessions().await;
    anyhow::ensure!(
        list.is_err(),
        "list_sessions on a killed server must error, got {list:?}"
    );

    // Recovery path: a fresh instance on a fresh socket serves a fresh client,
    // and the dead process's session state does not leak into it.
    let replacement = TestServer::spawn()?;
    let recovered = replacement.connect().await?;
    let recovered_session = recovered
        .create_session("recovered", std::path::Path::new("/tmp"))
        .await?;
    let attach = recovered
        .attach(&recovered_session, AttachMode::Shared)
        .await?;
    anyhow::ensure!(
        attach.snapshot.is_some(),
        "recovered session attach returned no snapshot"
    );
    let sessions = recovered.list_sessions().await?;
    anyhow::ensure!(
        sessions
            .iter()
            .any(|session| session.id == recovered_session),
        "replacement server does not list the session it just created"
    );
    anyhow::ensure!(
        !sessions.iter().any(|session| session.id == session_id),
        "session from the killed server leaked into the replacement instance"
    );
    Ok(())
}

/// Flip every byte after the single-byte length prefix so the frame carries a
/// plausible prefix but a payload no `Envelope` encodes to.
fn corrupt_payload(mut frame: Vec<u8>) -> Vec<u8> {
    for byte in &mut frame[1..] {
        *byte = 0xFF;
    }
    frame
}

/// §9 One malformed frame must take down only its own connection. Each garbage
/// connection is followed by a fresh, real client that must still be served
/// and must still see the session created before the garbage — a server that
/// panics, wedges its accept loop, or drops shared state fails here.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_frames_do_not_poison_the_server() -> Result<()> {
    let server = TestServer::spawn()?;
    let baseline = server.connect().await?;
    let (session_id, _tab_id) = open_session(&baseline, "garbage").await?;

    let good_frame = mux_protocol::frame(&mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(
            mux_protocol::Request {
                request_id: 1,
                body: Some(mux_protocol::proto::request::Body::ListSessions(
                    mux_protocol::ListSessionsRequest {},
                )),
            },
        )),
    })?;
    anyhow::ensure!(
        good_frame.len() < 128,
        "probe frame must keep a single-byte prefix, got {}",
        good_frame.len()
    );

    let garbage_frames: Vec<(&str, Vec<u8>)> = vec![
        (
            "overlong varint prefix",
            vec![0xFF; mux_protocol::MAX_VARINT_LEN + 1],
        ),
        (
            "declared length near i64::MAX with no payload",
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F],
        ),
        (
            "valid prefix with corrupted payload",
            corrupt_payload(good_frame.clone()),
        ),
        (
            "frame truncated mid-payload",
            good_frame[..good_frame.len() / 2].to_vec(),
        ),
    ];

    for (label, garbage) in garbage_frames {
        let mut junk = UnixStream::connect(&server.socket_path)
            .await
            .with_context(|| format!("{label}: connect"))?;
        junk.write_all(&garbage)
            .await
            .with_context(|| format!("{label}: write"))?;
        junk.shutdown()
            .await
            .with_context(|| format!("{label}: half-close"))?;
        drop(junk);

        // The fresh connection is also the synchronization point: if the
        // server died on the garbage, this connect or RPC fails.
        let probe = server
            .connect()
            .await
            .with_context(|| format!("{label}: server no longer accepts connections"))?;
        let sessions = probe
            .list_sessions()
            .await
            .with_context(|| format!("{label}: server stopped serving after malformed frame"))?;
        anyhow::ensure!(
            sessions.iter().any(|session| session.id == session_id),
            "{label}: session {session_id} vanished after the malformed frame"
        );
    }
    Ok(())
}

/// §3.4 / §15.5 A subscriber that never drains its bounded (4096) channel
/// must not stall the wire: lossy notifications pile up or drop, but grid
/// fetches keep answering, generations stay monotonic, and ordinary RPCs
/// still work for the whole burst. If fan-out blocked the I/O thread on the
/// lossy path, the per-fetch timeout below would trip. (Reliable lifecycle
/// events apply backpressure by design per §3.1 and are not this test.)
#[tokio::test(flavor = "multi_thread")]
async fn undrained_subscriber_does_not_stall_the_wire() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, tab_id) = open_session(&domain, "backpressure").await?;

    // Bounded and intentionally never drained during the burst.
    let undrained = domain.subscribe();

    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            size(200, 50),
            Some(shell("/usr/bin/yes", &["z3rm-backpressure"])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    let mut previous = 0u64;
    let mut advances = 0u64;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let fetch = tokio::time::timeout(
            Duration::from_secs(2),
            domain.fetch_grid_update(&pane_id, previous),
        )
        .await
        .context("grid fetch stalled while the subscriber channel was never drained")?
        .context("grid fetch failed under backpressure")?;
        let current = fetch.to_generation;
        anyhow::ensure!(
            current >= previous,
            "generation went backwards under backpressure: {previous} then {current}"
        );
        if current > previous {
            advances += 1;
            previous = current;
        }
    }
    anyhow::ensure!(
        advances > 5,
        "the pane barely advanced under backpressure: {advances} generation bumps in 3s"
    );

    // The RPC path must still be live for requests unrelated to the pane.
    anyhow::ensure!(
        domain.check_connection().await,
        "connection died while the subscriber channel was never drained"
    );
    let sessions = domain.list_sessions().await?;
    anyhow::ensure!(
        sessions.iter().any(|session| session.id == session_id),
        "the session vanished under subscriber backpressure"
    );

    domain.kill_session(&session_id).await?;

    // With the session gone the notification stream must drain and go quiet
    // rather than block: a wedged fan-out would hang this loop.
    let mut drained = 0usize;
    while tokio::time::timeout(Duration::from_millis(500), undrained.recv())
        .await
        .is_ok()
    {
        drained += 1;
        anyhow::ensure!(
            drained < 10_000_000,
            "notification stream never went quiet after the session was killed"
        );
    }
    anyhow::ensure!(
        drained > 0,
        "no notification was ever delivered while the channel was under pressure"
    );
    Ok(())
}

/// §3.4 / §15.4 A client that was away while PaneDirty and lifecycle
/// notifications happened must recover exclusively from authoritative state —
/// the attach snapshot and generation-based grid fetches — never from
/// notifications it never received.
///
/// Phase 1: the owner disconnects while a bounded flood runs and a new pane
/// is spawned. The reconnecting client must (a) see the exact pane set the
/// actor sees, and (b) get a full grid snapshot for its pre-disconnect
/// generation — a partial merged diff or a NoChange for state that moved on
/// would leave the UI stale forever.
///
/// Phase 2: an in-place reconnect while connected must re-register the
/// subscriber pipeline (synthetic `SessionLayoutChanged`, then real lifecycle
/// notifications) and keep converging on the same generation as the actor.
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_recovers_authoritative_snapshot_after_missed_notifications() -> Result<()> {
    let server = TestServer::spawn()?;
    let actor = server.connect().await?;
    let (session_id, tab_id) = open_session(&actor, "reconnect-snap").await?;

    // Bounded flood with a settle point: the pane keeps producing output for
    // a few seconds, then `cat` holds the pane open so the session survives.
    // 200k lines ≈ 4.4 MB ≈ hundreds of grid generations.
    let p1 = actor
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell(
                "/bin/sh",
                &["-c", "yes z3rm-reconnect-marker | head -n 200000; cat"],
            )),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    // Owner attaches, samples the generation early in the flood, then
    // disconnects — every later PaneDirty and lifecycle notification misses it.
    let owner1 = server.connect().await?;
    owner1.attach(&session_id, AttachMode::Shared).await?;
    let stale_generation = wait_for_generation(
        &owner1,
        &p1,
        |generation| generation > 0,
        Duration::from_secs(10),
        "the first flood generation",
    )
    .await?;
    owner1.detach().await?;
    drop(owner1);

    // While the owner is away: a lifecycle change it will never hear about.
    let p2 = actor
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell("/bin/cat", &[])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    // The flood is bounded, so it must settle to a fixed generation.
    let settled_generation =
        wait_for_settled_generation(&actor, &p1, Duration::from_secs(30), "the flood to finish")
            .await?;
    anyhow::ensure!(
        settled_generation >= stale_generation + 128,
        "flood advanced only {} generations past the stale checkpoint ({} → {}); \
         the reconnect must be tested against a long absence",
        settled_generation.saturating_sub(stale_generation),
        stale_generation,
        settled_generation,
    );

    // The reconnecting client recovers the missed lifecycle events from the
    // authoritative attach snapshot.
    let owner2 = server.connect().await?;
    let attach = owner2.attach(&session_id, AttachMode::Shared).await?;
    let seen: HashSet<String> = snapshot_pane_ids(
        attach
            .snapshot
            .as_ref()
            .context("attach returned no snapshot")?,
    )
    .into_iter()
    .collect();
    let expected: HashSet<String> = HashSet::from([p1.clone(), p2.clone()]);
    anyhow::ensure!(
        seen == expected,
        "reconnecting client sees panes {seen:?}, expected {expected:?}"
    );

    // Authoritative grid recovery: fetching from the pre-disconnect generation
    // must yield a full snapshot of the current state, not a partial diff.
    let resync = owner2.fetch_grid_update(&p1, stale_generation).await?;
    let update = resync
        .update
        .as_ref()
        .context("stale-generation fetch returned no update")?;
    anyhow::ensure!(
        matches!(
            update,
            proto::fetch_grid_update_response::Update::FullSnapshot(_)
        ),
        "reconnect fetch since a stale generation must return a full snapshot, got {update:?}"
    );
    anyhow::ensure!(
        resync.to_generation == settled_generation,
        "reconnect full snapshot is generation {}, expected the settled generation {}",
        resync.to_generation,
        settled_generation,
    );

    // Both clients converge on the same authoritative grid.
    let from_scratch = owner2.fetch_grid_update(&p1, 0).await?;
    let text = grid_text(
        from_scratch
            .update
            .as_ref()
            .context("from-scratch fetch returned no update")?,
    );
    anyhow::ensure!(
        text.contains("z3rm-reconnect-marker"),
        "authoritative grid lost the flood content"
    );
    let actor_view = actor.fetch_grid_update(&p1, 0).await?;
    anyhow::ensure!(
        actor_view.to_generation == settled_generation,
        "actor drifted from the settled generation: {} vs {}",
        actor_view.to_generation,
        settled_generation,
    );

    // Phase 2: in-place reconnect while connected. The synthetic
    // SessionLayoutChanged must arrive, and a pane spawned afterwards must
    // reach the reconnected subscriber through the fresh transport.
    let tally = Arc::new(parking_lot::Mutex::new(NotificationTally::default()));
    {
        let rx = owner2.subscribe();
        let tally = tally.clone();
        tokio::spawn(async move {
            while let Ok(notification) = rx.recv().await {
                let mut observed = tally.lock();
                match notification.event {
                    Some(proto::notification::Event::PaneDirty(dirty)) => {
                        *observed.pane_dirty.entry(dirty.pane_id).or_default() += 1;
                    }
                    Some(proto::notification::Event::PaneAdded(added)) => {
                        observed.pane_added.insert(added.pane_id);
                    }
                    Some(proto::notification::Event::SessionLayoutChanged(_)) => {
                        observed.layout_changed += 1;
                    }
                    _ => {}
                }
            }
        });
    }

    owner2
        .reconnect_local_in_place(&session_id, AttachMode::Shared)
        .await
        .context("in-place reconnect failed")?;
    wait_for_tally(
        &tally,
        Duration::from_secs(10),
        |observed| observed.layout_changed > 0,
        "the synthetic SessionLayoutChanged after in-place reconnect",
    )
    .await?;

    let p3 = actor
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell("/bin/cat", &[])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;
    wait_for_tally(
        &tally,
        Duration::from_secs(10),
        |observed| observed.pane_added.contains(&p3),
        "PaneAdded for a pane spawned after the in-place reconnect",
    )
    .await?;

    // The reconnected client and the actor still agree on everything.
    let converged = owner2.fetch_grid_update(&p1, 0).await?;
    anyhow::ensure!(
        converged.to_generation == settled_generation,
        "reconnected client drifted from the settled generation: {} vs {}",
        converged.to_generation,
        settled_generation,
    );
    let reattached = owner2.attach(&session_id, AttachMode::Shared).await?;
    let seen: HashSet<String> = snapshot_pane_ids(
        reattached
            .snapshot
            .as_ref()
            .context("reattach returned no snapshot")?,
    )
    .into_iter()
    .collect();
    let expected: HashSet<String> = HashSet::from([p1, p2, p3]);
    anyhow::ensure!(
        seen == expected,
        "after reconnect the client sees panes {seen:?}, expected {expected:?}"
    );

    owner2.kill_session(&session_id).await?;
    Ok(())
}

/// §3.3 GridDiffRing wrap: a client whose checkpoint falls out of the ring — a
/// slow consumer that stops fetching while output continues — must be handed a
/// full snapshot. A merged diff built from only the surviving entries would
/// silently skip intermediate generations and never converge.
///
/// The pane writes in place (`\rA`) so the cursor never moves: every
/// generation is row-representable, so only ring retention decides between a
/// diff and a full snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn slow_consumer_wrapping_diff_ring_gets_full_snapshot() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, tab_id) = open_session(&domain, "ring-wrap").await?;

    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell(
                "/bin/sh",
                &["-c", "while :; do printf \"\\rA\"; done"],
            )),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    // Sample the generation once, then stop fetching — the slow consumer.
    let stale = wait_for_generation(
        &domain,
        &pane_id,
        |generation| generation > 0,
        Duration::from_secs(10),
        "the first generation",
    )
    .await?;

    // Keep the server publishing until the ring has provably wrapped past the
    // stale checkpoint: twice the documented 64-entry capacity, so the oldest
    // retained entry is newer than the checkpoint regardless of scheduling.
    let current = wait_for_generation(
        &domain,
        &pane_id,
        |generation| generation >= stale + 128,
        Duration::from_secs(30),
        "the generation to advance 128 past the stale checkpoint",
    )
    .await?;

    // The checkpoint fell out of the ring: the server must answer with a full
    // snapshot, never a diff that omits the missing generations.
    let resync = domain.fetch_grid_update(&pane_id, stale).await?;
    anyhow::ensure!(
        matches!(
            resync.update,
            Some(proto::fetch_grid_update_response::Update::FullSnapshot(_))
        ),
        "fetch since a wrapped generation must return a full snapshot, got {:?}",
        resync.update,
    );

    // The snapshot reflects current state, and a from-scratch fetch agrees.
    let from_scratch = domain.fetch_grid_update(&pane_id, 0).await?;
    let text = grid_text(
        from_scratch
            .update
            .as_ref()
            .context("from-scratch fetch returned no update")?,
    );
    anyhow::ensure!(
        text.contains('A'),
        "full snapshot lost the in-place flood content"
    );
    anyhow::ensure!(
        from_scratch.to_generation >= current,
        "generation went backwards: {current} then {}",
        from_scratch.to_generation
    );

    // A follow-up fetch from a fresh checkpoint must stay anchored: any
    // update is either a full snapshot (from_generation 0) or a diff rooted
    // exactly at the requested generation — never at an earlier one that
    // would silently skip generations. (The flood is continuous, so a
    // NoChange answer is a race between fetch and publish, not an invariant.)
    let follow_up = domain
        .fetch_grid_update(&pane_id, from_scratch.to_generation)
        .await?;
    anyhow::ensure!(
        follow_up.to_generation >= from_scratch.to_generation,
        "generation went backwards: {} then {}",
        from_scratch.to_generation,
        follow_up.to_generation
    );
    if follow_up.update.is_some() {
        anyhow::ensure!(
            follow_up.from_generation == 0
                || follow_up.from_generation == from_scratch.to_generation,
            "update anchored at generation {}, expected 0 (full snapshot) or {} (diff)",
            follow_up.from_generation,
            from_scratch.to_generation
        );
    }

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §3.1 / §9 A connection whose socket is never drained — a client that
/// attaches and then stops reading while a pane floods output — must stall
/// only itself. The server's write path for that connection queues frames, but
/// shared session state (grid fetches, lifecycle RPCs, pane fan-out) must keep
/// serving every other client with bounded per-op latency.
#[tokio::test(flavor = "multi_thread")]
async fn stalled_wire_consumer_does_not_stall_other_clients() -> Result<()> {
    let server = TestServer::spawn()?;
    let live = server.connect().await?;
    let (session_id, tab_id) = open_session(&live, "wire-isolation").await?;

    let flooded = live
        .spawn_pane(
            &session_id,
            &tab_id,
            size(80, 24),
            Some(shell("/usr/bin/yes", &["z3rm-wire-flood"])),
            Some(std::path::Path::new("/tmp")),
        )
        .await?;

    // A raw connection that attaches to the session and then never reads a
    // single byte. The server registers it as a pane subscriber; the flood
    // fills its socket buffer and wedges its write loop.
    let mut raw = UnixStream::connect(&server.socket_path)
        .await
        .context("raw client connect")?;
    let attach_frame = mux_protocol::frame(&Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(EnvelopePayload::Request(Request {
            request_id: 1,
            body: Some(RequestBody::Attach(proto::AttachRequest {
                session_id: session_id.clone(),
                mode: 1, // proto AttachMode::SHARED
                window_id: String::new(),
                identity: None,
            })),
        })),
    })?;
    raw.write_all(&attach_frame)
        .await
        .context("raw client attach")?;
    // Give the flood time to fill the socket buffer and wedge the write loop.
    // Not asserted against; the per-op timeouts below are the real bound.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // While the raw connection is wedged, the live client's RPC surface must
    // keep working, interleaving grid pulls and lifecycle churn.
    let mut previous = 0u64;
    for round in 0..40 {
        let fetch =
            tokio::time::timeout(Duration::from_secs(2), live.fetch_grid_update(&flooded, 0))
                .await
                .with_context(|| {
                    format!("round {round}: grid fetch stalled behind a wedged connection")
                })?
                .with_context(|| format!("round {round}: grid fetch failed"))?;
        anyhow::ensure!(
            fetch.to_generation >= previous,
            "round {round}: generation went backwards: {previous} then {}",
            fetch.to_generation
        );
        previous = fetch.to_generation;

        let scratch = tokio::time::timeout(
            Duration::from_secs(3),
            live.spawn_pane(
                &session_id,
                &tab_id,
                size(80, 24),
                Some(shell("/bin/cat", &[])),
                Some(std::path::Path::new("/tmp")),
            ),
        )
        .await
        .with_context(|| format!("round {round}: spawn stalled behind a wedged connection"))?
        .with_context(|| format!("round {round}: spawn failed"))?;
        tokio::time::timeout(Duration::from_secs(3), live.close_pane(&scratch))
            .await
            .with_context(|| format!("round {round}: close stalled behind a wedged connection"))?
            .with_context(|| format!("round {round}: close failed"))?;
    }

    let sessions = tokio::time::timeout(Duration::from_secs(2), live.list_sessions())
        .await
        .context("list_sessions stalled behind a wedged connection")?
        .context("list_sessions failed")?;
    anyhow::ensure!(
        sessions.iter().any(|session| session.id == session_id),
        "session vanished while a connection was wedged"
    );

    // Letting the wedged connection go must release the server cleanly.
    raw.shutdown().await.context("raw client shutdown")?;
    drop(raw);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = tokio::time::timeout(Duration::from_secs(2), live.fetch_grid_update(&flooded, 0))
        .await
        .context("fetch after dropping the wedged connection timed out")?
        .context("fetch after dropping the wedged connection failed")?;
    anyhow::ensure!(
        after.to_generation >= previous,
        "generation went backwards after the wedged connection was dropped"
    );

    live.kill_session(&session_id).await?;
    Ok(())
}

/// §15.5 A throughput/latency smoke check: 300 round trips (spawn, grid fetch,
/// close) must all complete with correct results. The deterministic bound is
/// per-op — every operation must finish inside its own timeout, so a slow
/// machine fails the op, not a wall-clock average. The aggregate is measured
/// and reported rather than asserted tightly.
#[tokio::test(flavor = "multi_thread")]
async fn throughput_smoke_hundred_plus_operations() -> Result<()> {
    const PANE_COUNT: usize = 100;

    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let (session_id, tab_id) = open_session(&domain, "throughput-smoke").await?;

    let started = Instant::now();
    let mut latencies = Vec::with_capacity(PANE_COUNT * 3);
    let mut panes = Vec::with_capacity(PANE_COUNT);

    for _ in 0..PANE_COUNT {
        let op_started = Instant::now();
        let pane = tokio::time::timeout(
            Duration::from_secs(10),
            domain.spawn_pane(
                &session_id,
                &tab_id,
                size(80, 24),
                Some(shell("/bin/cat", &[])),
                Some(std::path::Path::new("/tmp")),
            ),
        )
        .await
        .context("spawn_pane stalled")?
        .context("spawn_pane failed")?;
        latencies.push(op_started.elapsed());
        panes.push(pane);
    }

    for pane in &panes {
        let op_started = Instant::now();
        let response =
            tokio::time::timeout(Duration::from_secs(2), domain.fetch_grid_update(pane, 0))
                .await
                .context("fetch_grid_update stalled")?
                .context("fetch_grid_update failed")?;
        anyhow::ensure!(
            response.update.is_some(),
            "from-scratch fetch for a fresh pane returned no update"
        );
        latencies.push(op_started.elapsed());
    }

    for pane in &panes {
        let op_started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), domain.close_pane(pane))
            .await
            .context("close_pane stalled")?
            .context("close_pane failed")?;
        latencies.push(op_started.elapsed());
    }

    let total = started.elapsed();
    let ops = PANE_COUNT * 3;
    let max_latency = latencies.iter().copied().max().unwrap_or_default();
    eprintln!(
        "throughput smoke: {ops} ops in {total:?} ({:.0} ops/s), max single-op latency {max_latency:?}",
        ops as f64 / total.as_secs_f64()
    );

    // The deterministic bound: every op cleared its own timeout above, and the
    // aggregate must clear a very generous floor (nothing quadratic or
    // serialized behind a growing queue).
    anyhow::ensure!(
        total < Duration::from_secs(60),
        "{ops} ops took {total:?}; the server's RPC path is not scaling"
    );

    // Lifecycle correctness after the mass churn: every pane spawned and
    // closed again, leaving the session with no panes.
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let remaining = snapshot_pane_ids(attach.snapshot.as_ref().context("final snapshot")?);
    anyhow::ensure!(
        remaining.is_empty(),
        "after spawning and closing {PANE_COUNT} panes the session still holds {remaining:?}"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}
