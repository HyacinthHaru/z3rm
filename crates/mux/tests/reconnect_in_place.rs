//! # §15.4 / §15.12 authoritative in-place reconnect
//!
//! Verifies `MuxDomain::reconnect_local_in_place`:
//! - connects a fresh domain and swaps its transport/inner into the existing
//!   `MuxDomain` so every `Arc<MuxDomain>`/subscriber keeps working,
//! - preserves `window_id`,
//! - re-attaches the supplied active session,
//! - broadcasts a synthetic `SessionLayoutChanged` from the full snapshot to
//!   subscribers that were registered *before* the reconnect, and
//! - the swapped domain serves real RPCs afterwards.
//!
//! Like the other e2e tests, it spawns a real `z3rm-server` subprocess on an
//! isolated socket.

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto::notification::Event as NotifEvent;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

static SOCKET_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
            for c in &candidates {
                if c.exists() {
                    return c.to_string_lossy().into_owned();
                }
            }
            "z3rm-server".to_string()
        });

        let child = std::process::Command::new(&exe)
            .env("RUST_LOG", "off")
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn z3rm-server at {}", exe))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if socket_path.exists() {
                break;
            }
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
        // The in-place reconnect API intentionally reconnects the process default
        // socket; point it at this test's isolated server for the test lifetime.
        unsafe { std::env::set_var("Z3RM_MUX_SOCKET", &self.socket_path) };
        mux::connect_local(None)
            .await
            .context("connect_local failed")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("failed to kill reconnect mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap reconnect mux server: {error}");
        }
    }
}

async fn wait_for_notification(
    rx: &mux::NotificationReceiver,
    timeout: Duration,
    mut predicate: impl FnMut(&NotifEvent) -> bool,
) -> Result<NotifEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for notification");
        }
        let notif = smol::future::or(async { rx.recv().await.ok() }, async move {
            smol::Timer::after(remaining.min(Duration::from_millis(200))).await;
            None
        })
        .await;
        let Some(notif) = notif else { continue };
        if let Some(event) = notif.event {
            if predicate(&event) {
                return Ok(event);
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for notification");
        }
    }
}

/// §15.4 in-place reconnect swaps the transport/inner, preserves `window_id`
/// and subscribers, reattaches the supplied session, and broadcasts a
/// synthetic `SessionLayoutChanged` derived from the authoritative snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_in_place_preserves_subscribers_and_window() -> Result<()> {
    let _socket_env_guard = SOCKET_ENV_LOCK.lock().await;
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    // §3.10 Create a session, spawn a pane so the snapshot is non-empty.
    let session_id = domain
        .create_session("reconnect-test", std::path::Path::new("/tmp"))
        .await?;
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach
        .snapshot
        .as_ref()
        .and_then(|s| s.tabs.first().map(|t| t.id.clone()))
        .context("attach snapshot missing first tab")?;
    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            mux_protocol::TerminalSize { cols: 80, rows: 24 },
            None,
            None,
        )
        .await
        .context("spawn_pane failed")?;

    // Capture window_id and register a subscriber BEFORE reconnect. The same
    // receiver must keep receiving notifications after the in-place swap.
    let window_id_before = domain.window_id();
    let notif_rx = domain.subscribe();

    // Sanity: the connection is alive before reconnect.
    assert!(
        domain.check_connection().await,
        "connection must be alive before reconnect"
    );

    // §15.4 Authoritative in-place reconnect against the live server.
    domain
        .reconnect_local_in_place(&session_id, AttachMode::Shared)
        .await
        .context("reconnect_local_in_place failed")?;

    // window_id is preserved across the swap (same logical window).
    assert_eq!(
        domain.window_id(),
        window_id_before,
        "window_id must survive in-place reconnect"
    );

    // The pre-reconnect subscriber must receive the synthetic
    // SessionLayoutChanged broadcast from the full snapshot.
    let event = wait_for_notification(&notif_rx, Duration::from_secs(5), |event| {
        matches!(event, NotifEvent::SessionLayoutChanged(_))
    })
    .await
    .context("did not receive synthetic SessionLayoutChanged after reconnect")?;
    let changed = match event {
        NotifEvent::SessionLayoutChanged(changed) => changed,
        event => panic!("expected SessionLayoutChanged, got {event:?}"),
    };
    assert!(
        changed.layout.is_some(),
        "synthetic resync must carry the layout tree"
    );
    let resync_snapshot = changed
        .snapshot
        .context("reconnect resync must carry the authoritative SessionSnapshot")?;
    let resynced_pane_ids: Vec<String> = resync_snapshot
        .tabs
        .iter()
        .flat_map(|tab| tab.panes.iter().map(|pane| pane.id.clone()))
        .collect();
    assert!(
        resynced_pane_ids.contains(&pane_id),
        "resync snapshot must expose pane {pane_id}, got {resynced_pane_ids:?}"
    );

    // §15.12 The reattach snapshot from reconnect must expose the pane that
    // existed before; pull a fresh attach to confirm authoritative state.
    let reattach = domain.attach(&session_id, AttachMode::Shared).await?;
    let snapshot = reattach
        .snapshot
        .as_ref()
        .context("reattach snapshot missing")?;
    let pane_ids: Vec<String> = snapshot
        .tabs
        .iter()
        .flat_map(|t| t.panes.iter().map(|p| p.id.clone()))
        .collect();
    assert!(
        pane_ids.contains(&pane_id),
        "pane {} must survive reconnect, got {:?}",
        pane_id,
        pane_ids
    );

    // §15.4 Real RPC against the swapped transport: list_sessions must work
    // and report our session.
    let sessions = domain.list_sessions().await?;
    assert!(
        sessions.iter().any(|s| s.id == session_id),
        "list_sessions must work after reconnect"
    );

    Ok(())
}

/// §15.4 reconnect errors propagate (unknown session id) instead of being
/// silently swallowed by a fallback path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_in_place_propagates_attach_error() -> Result<()> {
    let _socket_env_guard = SOCKET_ENV_LOCK.lock().await;
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    // Attach first so subscribers exist; then reconnect with a bogus session.
    let session_id = domain
        .create_session("err-attach-test", std::path::Path::new("/tmp"))
        .await?;
    domain.attach(&session_id, AttachMode::Shared).await?;
    let _notif_rx = domain.subscribe();

    let bogus = "nonexistent-session-zzz";
    let result = domain
        .reconnect_local_in_place(bogus, AttachMode::Shared)
        .await;
    assert!(
        result.is_err(),
        "reconnect with unknown session must error, got {:?}",
        result
    );

    Ok(())
}
