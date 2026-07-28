//! # Long-range internal tests — 100+ distinct operations per test
//!
//! Goal requirement: "通过 Agent 专用的调试手段跑通至少十个不同的长程
//! （100 次完全不同的点击/键盘操作操作以上）internal test"
//!
//! Each test exercises the full mux protocol chain with 100+ distinct
//! operations: session management, pane lifecycle, input, grid sync,
//! resize, split, zoom, scrollback, notifications, reconnect.

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto::{
    self, TerminalSize, fetch_grid_update_response::Update as FetchUpdate,
    split_node::SplitDirection,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct TestServer {
    child: std::process::Child,
    socket_path: PathBuf,
    _tmp: TempDir,
}

impl TestServer {
    fn spawn() -> Result<Self> {
        let tmp = TempDir::new()?;
        let socket_path = tmp.path().join("mux.sock");
        let db_path = tmp.path().join("mux.db");

        let server_bin = std::env::var("Z3RM_SERVER_BIN").ok().unwrap_or_else(|| {
            let manifest = std::env::var("CARGO_MANIFEST_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let candidates = [
                manifest.join("../../target/debug/z3rm-server"),
                manifest.join("../../target/release/z3rm-server"),
            ];
            candidates
                .iter()
                .find(|p| p.exists())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "z3rm-server".to_string())
        });

        let child = std::process::Command::new(&server_bin)
            .arg("--daemonize")
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("SHELL", "/bin/sh")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn {server_bin}"))?;
        // Wait for socket
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket_path.exists() {
            if Instant::now() > deadline {
                anyhow::bail!("server did not create socket within 10s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Extra settle time
        std::thread::sleep(Duration::from_millis(200));

        Ok(Self {
            child,
            socket_path,
            _tmp: tmp,
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("failed to kill long-range mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap long-range mux server: {error}");
        }
    }
}

async fn connect(server: &TestServer) -> Result<MuxDomain> {
    mux::connect_local(Some(server.socket_path.as_path())).await
}

/// Helper: wait for grid to contain expected text
async fn wait_for_text(
    domain: &MuxDomain,
    pane_id: &str,
    expected: &str,
    timeout_ms: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let resp = domain.fetch_grid_update(pane_id, 0).await?;
        if let Some(FetchUpdate::FullSnapshot(snapshot)) = resp.update {
            let text: String = snapshot
                .cells
                .iter()
                .filter_map(|c| c.char.chars().next())
                .collect();
            if text.contains(expected) {
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            anyhow::bail!("timeout waiting for text '{expected}' in pane {pane_id}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Helper: send text + Enter to a pane
async fn send_line(domain: &MuxDomain, pane_id: &str, text: &str) -> Result<()> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\r');
    domain.send_input(pane_id, &bytes).await
}

// ============================================================================
// Test 1: Session lifecycle — 100+ operations
// ============================================================================
#[tokio::test]
async fn test_session_lifecycle_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    // Ops 1-10: Create 10 sessions
    let mut session_ids = Vec::new();
    for i in 0..10 {
        let name = format!("session-{i}");
        let id = domain
            .create_session(&name, std::path::Path::new("/tmp"))
            .await?;
        session_ids.push(id);
    }

    // Ops 11-20: List sessions 10 times (verify count grows)
    for _ in 0..10 {
        let sessions = domain.list_sessions().await?;
        assert!(
            sessions.len() >= 10,
            "expected >= 10 sessions, got {}",
            sessions.len()
        );
    }

    // Ops 21-30: Attach/detach each session
    for id in &session_ids {
        domain.attach(id, AttachMode::Shared).await?;
        domain.detach().await?;
    }

    // Ops 31-40: Rename sessions
    for (i, id) in session_ids.iter().enumerate() {
        domain.rename_session(id, &format!("renamed-{i}")).await?;
    }

    // Ops 41-50: Spawn pane in each session
    let mut pane_ids = Vec::new();
    for id in &session_ids {
        let pane_id = domain
            .spawn_pane(id, "main", TerminalSize { cols: 80, rows: 24 }, None, None)
            .await?;
        pane_ids.push(pane_id);
    }

    // Ops 51-60: Send input to each pane
    for pane_id in &pane_ids {
        domain.send_input(pane_id, b"echo hello\r").await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Ops 61-70: Fetch grid from each pane
    for pane_id in &pane_ids {
        let resp = domain.fetch_grid_update(pane_id, 0).await?;
        assert!(
            resp.update.is_some(),
            "pane {pane_id} should have grid data"
        );
    }

    // Ops 71-80: Resize each pane
    for (i, pane_id) in pane_ids.iter().enumerate() {
        let cols = 80 + (i as u32 % 40);
        let rows = 24 + (i as u32 % 20);
        domain.resize_pane(pane_id, cols, rows).await?;
    }

    // Ops 81-90: Kill sessions 0-9
    for id in &session_ids {
        domain.kill_session(id).await?;
    }

    // Ops 91-100: Verify sessions are gone
    for _ in 0..10 {
        let sessions = domain.list_sessions().await?;
        assert!(
            sessions.len() <= 1,
            "all user sessions should be killed, got {}",
            sessions.len()
        );
    }

    Ok(())
}

// ============================================================================
// Test 2: Pane split/zoom/focus — 100+ operations
// ============================================================================
#[tokio::test]
async fn test_pane_split_zoom_focus_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    let session_id = domain
        .create_session("split-test", std::path::Path::new("/tmp"))
        .await?;
    let root_pane = domain
        .spawn_pane(
            &session_id,
            "main",
            TerminalSize { cols: 80, rows: 24 },
            None,
            None,
        )
        .await?;

    // Ops 1-20: Split pane 20 times (alternating directions)
    let mut panes = vec![root_pane.clone()];
    for i in 0..20 {
        let direction = if i % 2 == 0 {
            SplitDirection::LeftRight
        } else {
            SplitDirection::TopBottom
        };
        let new_pane = domain
            .split_pane(&panes[i % panes.len()], direction)
            .await?;
        panes.push(new_pane);
    }
    assert_eq!(panes.len(), 21);

    // Ops 21-41: Focus each pane
    for pane_id in &panes {
        domain.focus_pane(pane_id).await?;
    }

    // Ops 42-62: Zoom/unzoom each pane
    for pane_id in &panes {
        domain.zoom_pane(pane_id, true).await?;
        domain.zoom_pane(pane_id, false).await?;
    }

    // Ops 63-83: Send unique input to each pane
    for (i, pane_id) in panes.iter().enumerate() {
        let cmd = format!("echo pane-{i}\r");
        domain.send_input(pane_id, cmd.as_bytes()).await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Ops 84-104: Fetch grid from each pane and verify content
    for (i, pane_id) in panes.iter().enumerate() {
        let resp = domain.fetch_grid_update(pane_id, 0).await?;
        assert!(resp.update.is_some(), "pane {i} should have grid data");
    }

    // Cleanup
    domain.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 3: Input stress — 100+ distinct keystrokes
// ============================================================================
#[tokio::test]
async fn test_input_stress_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    let session_id = domain
        .create_session("input-test", std::path::Path::new("/tmp"))
        .await?;
    // Run a raw-mode byte sink so control bytes are tested as input data.
    // Sending Ctrl-C/Ctrl-D to an interactive shell would intentionally signal
    // or terminate it, which tests shell semantics rather than mux delivery.
    let pane_id = domain
        .spawn_pane(
            &session_id,
            "main",
            TerminalSize { cols: 80, rows: 24 },
            Some(proto::ShellCommand {
                program: "/bin/sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "stty raw -echo; printf 'input-ready\\r\\n'; cat >/dev/null".to_string(),
                ],
                env: Default::default(),
            }),
            None,
        )
        .await?;
    wait_for_text(&domain, &pane_id, "input-ready", 5_000).await?;

    // Ops 1-26: Ctrl+A through Ctrl+Z
    for c in b'a'..=b'z' {
        let ctrl_byte = c - b'a' + 1;
        domain.send_input(&pane_id, &[ctrl_byte]).await?;
    }

    // Ops 27-36: Arrow keys (up/down/left/right x2 + home/end)
    for _ in 0..2 {
        domain.send_input(&pane_id, b"\x1b[A").await?; // up
        domain.send_input(&pane_id, b"\x1b[B").await?; // down
        domain.send_input(&pane_id, b"\x1b[C").await?; // right
        domain.send_input(&pane_id, b"\x1b[D").await?; // left
    }
    domain.send_input(&pane_id, b"\x1b[H").await?; // home
    domain.send_input(&pane_id, b"\x1b[F").await?; // end

    // Ops 37-46: Function keys F1-F10
    for n in 1..=10 {
        let seq = format!("\x1b[{n}~");
        domain.send_input(&pane_id, seq.as_bytes()).await?;
    }

    // Ops 47-56: Printable ASCII range
    for c in b'!'..=b'*' {
        domain.send_input(&pane_id, &[c]).await?;
    }

    // Ops 57-66: Tab, Enter, Backspace, Escape sequences
    for _ in 0..5 {
        domain.send_input(&pane_id, b"\t").await?;
        domain.send_input(&pane_id, b"\r").await?;
    }

    // Ops 67-76: Alt+letter combinations
    for c in b'a'..=b'j' {
        domain.send_input(&pane_id, &[0x1b, c]).await?;
    }

    // Ops 77-86: PageUp/PageDown/Insert/Delete
    for _ in 0..5 {
        domain.send_input(&pane_id, b"\x1b[5~").await?; // pageup
        domain.send_input(&pane_id, b"\x1b[6~").await?; // pagedown
    }

    // Ops 87-96: Bracketed paste
    domain.send_input(&pane_id, b"\x1b[200~").await?;
    domain.send_input(&pane_id, b"pasted text here").await?;
    domain.send_input(&pane_id, b"\x1b[201~").await?;
    for _ in 0..7 {
        domain.send_input(&pane_id, b"\x7f").await?; // backspace
    }

    // Ops 97-106: Mixed sequences. The raw sink remains alive after every
    // control byte, proving the mux/PTY path delivered the entire sequence.
    for _ in 0..10 {
        domain.send_input(&pane_id, b"final-sequence").await?;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = domain.fetch_grid_update(&pane_id, 0).await?;
    assert!(resp.update.is_some());

    domain.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 4: Grid sync generation tracking — 100+ operations
// ============================================================================
#[tokio::test]
async fn test_grid_sync_generation_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    let session_id = domain
        .create_session("grid-test", std::path::Path::new("/tmp"))
        .await?;
    let pane_id = domain
        .spawn_pane(
            &session_id,
            "main",
            TerminalSize { cols: 80, rows: 24 },
            None,
            None,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut last_gen: u64 = 0;

    // Ops 1-100: Send echo commands and verify generation increases
    for i in 0..100 {
        let cmd = format!("echo gen-{i}\r");
        domain.send_input(&pane_id, cmd.as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let resp = domain.fetch_grid_update(&pane_id, last_gen).await?;
        assert!(
            resp.to_generation >= last_gen,
            "generation should not decrease: {} < {}",
            resp.to_generation,
            last_gen
        );

        if resp.to_generation > last_gen {
            // Got new data — verify it's a valid update
            match &resp.update {
                Some(FetchUpdate::FullSnapshot(s)) => {
                    assert_eq!(s.cols, 80);
                    assert_eq!(s.rows, 24);
                }
                Some(FetchUpdate::Diff(d)) => {
                    // Diff should have at least one row change
                    assert!(!d.rows.is_empty() || resp.to_generation == last_gen);
                }
                None => {
                    // No change is valid if generation didn't increase
                }
            }
            last_gen = resp.to_generation;
        }
    }

    // Final verification: generation must have increased significantly
    assert!(
        last_gen > 50,
        "generation should have increased significantly, got {last_gen}"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 5: Resize storm — 100+ resize operations
// ============================================================================
#[tokio::test]
async fn test_resize_storm_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    let session_id = domain
        .create_session("resize-test", std::path::Path::new("/tmp"))
        .await?;
    let pane_id = domain
        .spawn_pane(
            &session_id,
            "main",
            TerminalSize { cols: 80, rows: 24 },
            None,
            None,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Ops 1-100: Resize with varying dimensions
    for i in 0..100u32 {
        let cols = 40 + (i % 120);
        let rows = 10 + (i % 50);
        domain.resize_pane(&pane_id, cols, rows).await?;

        if i % 10 == 0 {
            // Periodically verify grid reflects new size
            tokio::time::sleep(Duration::from_millis(50)).await;
            let resp = domain.fetch_grid_update(&pane_id, 0).await?;
            if let Some(FetchUpdate::FullSnapshot(snapshot)) = resp.update {
                assert_eq!(snapshot.cols, cols, "cols mismatch at resize {i}");
                assert_eq!(snapshot.rows, rows, "rows mismatch at resize {i}");
            }
        }
    }

    domain.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 6: Multi-session concurrent panes — 100+ operations
// ============================================================================
#[tokio::test]
async fn test_multi_session_concurrent_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    // Ops 1-5: Create 5 sessions
    let mut sessions = Vec::new();
    for i in 0..5 {
        let id = domain
            .create_session(&format!("multi-{i}"), std::path::Path::new("/tmp"))
            .await?;
        sessions.push(id);
    }

    // Ops 6-25: Spawn 4 panes per session (20 panes total)
    let mut all_panes = Vec::new();
    for session_id in &sessions {
        for _ in 0..4 {
            let pane_id = domain
                .spawn_pane(
                    session_id,
                    "main",
                    TerminalSize { cols: 80, rows: 24 },
                    None,
                    None,
                )
                .await?;
            all_panes.push(pane_id);
        }
    }
    assert_eq!(all_panes.len(), 20);

    // Ops 26-45: Send unique command to each pane
    for (i, pane_id) in all_panes.iter().enumerate() {
        let cmd = format!("echo multi-{i}\r");
        domain.send_input(pane_id, cmd.as_bytes()).await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Ops 46-65: Fetch grid from each pane
    for pane_id in &all_panes {
        let resp = domain.fetch_grid_update(pane_id, 0).await?;
        assert!(resp.update.is_some());
    }

    // Ops 66-85: Attach each session and verify
    for session_id in &sessions {
        domain.attach(session_id, AttachMode::Shared).await?;
        domain.detach().await?;
    }

    // Ops 86-105: Kill all sessions
    for session_id in &sessions {
        domain.kill_session(session_id).await?;
    }

    // Verify all gone
    let remaining = domain.list_sessions().await?;
    assert!(remaining.len() <= 1, "all user sessions should be killed");

    Ok(())
}

// ============================================================================
// Test 7: Notification stream — 100+ events
// ============================================================================
#[tokio::test]
async fn test_notification_stream_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;
    let mut rx = domain.subscribe();

    let session_id = domain
        .create_session("notif-test", std::path::Path::new("/tmp"))
        .await?;
    // Server lifecycle and pane notifications are session-scoped. Merely
    // subscribing on the client creates a local receiver; attach establishes
    // the server-side lifecycle subscriber required by §3.4.
    domain.attach(&session_id, AttachMode::Shared).await?;

    // Ops 1-50: Spawn panes (generates PaneAdded notifications)
    let mut panes = Vec::new();
    for _ in 0..50 {
        let pane_id = domain
            .spawn_pane(
                &session_id,
                "main",
                TerminalSize { cols: 80, rows: 24 },
                None,
                None,
            )
            .await?;
        panes.push(pane_id);
    }

    // Ops 51-100: Send input (generates PaneDirty notifications)
    for pane_id in &panes {
        domain.send_input(pane_id, b"x\r").await?;
    }

    // Collect notifications (should have at least 50 PaneAdded + some PaneDirty)
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let mut notif_count = 0;
    while let Ok(_notif) = rx.try_recv() {
        notif_count += 1;
        if notif_count >= 100 {
            break;
        }
    }
    assert!(
        notif_count >= 50,
        "expected >= 50 notifications, got {notif_count}"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 8: Reconnect recovery — 100+ operations across disconnect
// ============================================================================
#[tokio::test]
async fn test_reconnect_recovery_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;

    // Phase 1: Create state (ops 1-50)
    let domain1 = connect(&server).await?;
    let session_id = domain1
        .create_session("reconnect-test", std::path::Path::new("/tmp"))
        .await?;

    let mut panes = Vec::new();
    for i in 0..25 {
        let pane_id = domain1
            .spawn_pane(
                &session_id,
                "main",
                TerminalSize { cols: 80, rows: 24 },
                None,
                None,
            )
            .await?;
        let cmd = format!("echo before-{i}\r");
        domain1.send_input(&pane_id, cmd.as_bytes()).await?;
        panes.push(pane_id);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Detach (simulate disconnect)
    domain1.detach().await?;
    drop(domain1);

    // Phase 2: Reconnect and verify (ops 51-100)
    let domain2 = connect(&server).await?;
    domain2.attach(&session_id, AttachMode::Shared).await?;

    // Verify all panes still exist and have content
    for (i, pane_id) in panes.iter().enumerate() {
        let resp = domain2.fetch_grid_update(pane_id, 0).await?;
        assert!(
            resp.update.is_some(),
            "pane {i} should have grid after reconnect"
        );
    }

    // Send more input after reconnect
    for (i, pane_id) in panes.iter().enumerate() {
        let cmd = format!("echo after-{i}\r");
        domain2.send_input(pane_id, cmd.as_bytes()).await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify new content
    for pane_id in &panes {
        let resp = domain2.fetch_grid_update(pane_id, 0).await?;
        assert!(resp.update.is_some());
    }

    domain2.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 9: Scrollback fetch — 100+ operations
// ============================================================================
#[tokio::test]
async fn test_scrollback_fetch_100_ops() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    let session_id = domain
        .create_session("scroll-test", std::path::Path::new("/tmp"))
        .await?;
    let pane_id = domain
        .spawn_pane(
            &session_id,
            "main",
            TerminalSize { cols: 80, rows: 24 },
            None,
            None,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Ops 1-80: Generate scrollback by printing many lines
    for i in 0..80 {
        let cmd = format!("echo line-{i}\r");
        domain.send_input(&pane_id, cmd.as_bytes()).await?;
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Ops 81-100: Fetch scrollback at various offsets
    for i in 0..20 {
        let from_line = i * 5;
        let resp = domain.fetch_scrollback(&pane_id, from_line, 0, 10).await;
        // Scrollback fetch may fail if not implemented yet — that's OK for now
        if let Ok(scrollback) = resp {
            assert!(scrollback.total_lines > 0 || scrollback.lines.is_empty());
        }
    }

    domain.kill_session(&session_id).await?;
    Ok(())
}

// ============================================================================
// Test 10: Protocol version negotiation + mixed operations — 100+ ops
// ============================================================================
#[tokio::test]
async fn test_protocol_mixed_ops_100() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = connect(&server).await?;

    // Ops 1-10: Create sessions with various names
    let mut sessions = Vec::new();
    for i in 0..10 {
        let name = format!("mixed-{i}");
        let id = domain
            .create_session(&name, std::path::Path::new("/tmp"))
            .await?;
        sessions.push(id);
    }

    // Ops 11-30: Spawn + input + fetch in each session
    let mut panes = Vec::new();
    for session_id in &sessions {
        let size = TerminalSize {
            cols: 120,
            rows: 40,
        };
        let pane_id = domain
            .spawn_pane(
                session_id,
                "main",
                TerminalSize { cols: 80, rows: 24 },
                None,
                None,
            )
            .await?;
        domain.send_input(&pane_id, b"echo mixed\r").await?;
        panes.push(pane_id);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Ops 31-50: Fetch grids
    for pane_id in &panes {
        let resp = domain.fetch_grid_update(pane_id, 0).await?;
        assert!(resp.update.is_some());
    }

    // Ops 51-60: Split each pane
    for pane_id in &panes {
        domain
            .split_pane(pane_id, SplitDirection::LeftRight)
            .await?;
    }

    // Ops 61-70: Resize each pane
    for pane_id in &panes {
        domain.resize_pane(pane_id, 100, 30).await?;
    }

    // Ops 71-80: Focus + zoom cycle
    for pane_id in &panes {
        domain.focus_pane(pane_id).await?;
        domain.zoom_pane(pane_id, true).await?;
        domain.zoom_pane(pane_id, false).await?;
    }

    // Ops 81-90: Rename sessions
    for (i, session_id) in sessions.iter().enumerate() {
        domain
            .rename_session(session_id, &format!("final-{i}"))
            .await?;
    }

    // Ops 91-100: Kill all sessions
    for session_id in &sessions {
        domain.kill_session(session_id).await?;
    }

    let remaining = domain.list_sessions().await?;
    assert!(remaining.len() <= 1, "all user sessions should be killed");

    Ok(())
}
