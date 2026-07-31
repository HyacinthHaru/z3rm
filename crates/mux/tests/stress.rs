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
use mux_protocol::proto::split_node::SplitDirection;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

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
    anyhow::ensure!(
        total > 0,
        "writing 50k lines produced no scrollback at all"
    );

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
