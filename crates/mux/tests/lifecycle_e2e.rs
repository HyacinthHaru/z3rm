//! # 生命周期 fan-out / steal / detach 端到端测试
//!
//! 覆盖 spec §3.4 at-least-once 生命周期语义与 §3.10 steal/detach:
//! - steal 抢占踢出旧 client (写操作拒绝, 订阅清空)
//! - detach 精确退订 (匿名 client 互不误删)
//! - split/close 的 PaneAdded/PaneRemoved/SessionLayoutChanged fan-out
//! - shell 自然退出触发 PaneRemoved (zombie pane 防线)

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto::split_node::SplitDirection;
use mux_protocol::proto::{ShellCommand, notification::Event as NotifEvent};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// 与 e2e.rs 相同的隔离 server harness (子进程 + 独立 socket)。
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
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn z3rm-server at {}", exe))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if socket_path.exists() {
                break;
            }
            if Instant::now() >= deadline {
                anyhow::bail!("z3rm-server failed to bind socket within 10s");
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 创建 session + 首个 pane, 返回 (session_id, pane_id)。
async fn setup_session_with_pane(domain: &MuxDomain, name: &str) -> Result<(String, String)> {
    let session_id = domain
        .create_session(name, std::path::Path::new("/tmp"))
        .await?;
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach
        .snapshot
        .as_ref()
        .and_then(|s| s.tabs.first().map(|t| t.id.clone()))
        .unwrap_or_else(|| "tab-0".to_string());
    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            mux_protocol::TerminalSize { cols: 80, rows: 24 },
            None,
            None,
        )
        .await?;
    Ok((session_id, pane_id))
}

/// 在通知流上等待满足条件的通知, 带超时。
async fn wait_for_notification(
    rx: &async_channel::Receiver<mux_protocol::Notification>,
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
        let Some(notif) = notif else {
            continue;
        };
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

/// §3.10 Steal: 第二个 client 抢占 attach 后, 旧 client 的写操作必须被拒绝,
/// 新 client 不受影响。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steal_kicks_previous_client() -> Result<()> {
    let server = TestServer::spawn()?;
    let client_a = server.connect().await?;
    let (session_id, pane_id) = setup_session_with_pane(&client_a, "steal-test").await?;

    // client B 以 Steal 模式抢占 session。
    let client_b = server.connect().await?;
    client_b.attach(&session_id, AttachMode::Steal).await?;

    // 旧 client A 的写操作必须被拒绝 (kicked)。
    let input_result = client_a.send_input(&pane_id, b"echo hi\n").await;
    assert!(
        input_result.is_err(),
        "kicked client's send_input must be rejected, got {:?}",
        input_result
    );
    let close_result = client_a.close_pane(&pane_id).await;
    assert!(
        close_result.is_err(),
        "kicked client's close_pane must be rejected, got {:?}",
        close_result
    );

    // 新 client B 的写操作不受影响。
    client_b
        .send_input(&pane_id, b"echo still-alive\n")
        .await
        .context("stealer's send_input must work")?;

    Ok(())
}

/// §3.10 Detach: 匿名 client 各自有唯一 id, 一个 detach 不影响另一个。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_isolates_anonymous_clients() -> Result<()> {
    let server = TestServer::spawn()?;
    let client_a = server.connect().await?;
    let (session_id, _pane_id) = setup_session_with_pane(&client_a, "detach-test").await?;

    // 第二个匿名 client attach 同一 session (此前所有匿名 client 共享
    // server-PID 作 id, 任一 detach 会把双方一起移除)。
    let client_b = server.connect().await?;
    client_b.attach(&session_id, AttachMode::Shared).await?;

    let sessions = client_a.list_sessions().await?;
    let attached = sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.attached_clients)
        .unwrap_or(0);
    assert_eq!(attached, 2, "both clients must be attached");

    // A detach 后, B 必须仍然 attached。
    client_a.detach().await?;
    let sessions = client_b.list_sessions().await?;
    let attached = sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.attached_clients)
        .unwrap_or(0);
    assert_eq!(
        attached, 1,
        "detaching one anonymous client must not remove the other"
    );

    Ok(())
}

/// §3.4 fan-out: split/close 的 PaneAdded/SessionLayoutChanged/PaneRemoved
/// 必须送达所有 attached client (at-least-once), 不只是发起方。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fanout_covers_split_and_close_for_all_clients() -> Result<()> {
    let server = TestServer::spawn()?;
    let client_a = server.connect().await?;
    let (session_id, pane_id) = setup_session_with_pane(&client_a, "fanout-test").await?;

    // client B attach 并订阅通知流。
    let client_b = server.connect().await?;
    client_b.attach(&session_id, AttachMode::Shared).await?;
    let notif_rx = client_b.subscribe();

    // A split → B 必须收到新 pane 的 PaneAdded。
    let new_pane_id = client_a
        .split_pane(&pane_id, SplitDirection::LeftRight)
        .await?;
    let event = wait_for_notification(
        &notif_rx,
        Duration::from_secs(5),
        |event| matches!(event, NotifEvent::PaneAdded(added) if added.pane_id == new_pane_id),
    )
    .await?;
    assert!(
        matches!(event, NotifEvent::PaneAdded(_)),
        "client B must receive PaneAdded for split pane"
    );

    // B 也必须收到 SessionLayoutChanged (split 改变 layout)。
    let event = wait_for_notification(&notif_rx, Duration::from_secs(5), |event| {
        matches!(event, NotifEvent::SessionLayoutChanged(_))
    })
    .await?;
    assert!(matches!(event, NotifEvent::SessionLayoutChanged(_)));

    // A close → B 必须收到 PaneRemoved。
    client_a.close_pane(&new_pane_id).await?;
    let event = wait_for_notification(
        &notif_rx,
        Duration::from_secs(5),
        |event| matches!(event, NotifEvent::PaneRemoved(removed) if removed.pane_id == new_pane_id),
    )
    .await?;
    assert!(
        matches!(event, NotifEvent::PaneRemoved(_)),
        "client B must receive PaneRemoved for closed pane"
    );

    Ok(())
}

/// §3.4 shell 自然退出 → server 必须 fan-out PaneRemoved (zombie pane 防线)。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pane_natural_exit_broadcasts_removed() -> Result<()> {
    let server = TestServer::spawn()?;
    let client = server.connect().await?;
    let session_id = client
        .create_session("exit-test", std::path::Path::new("/tmp"))
        .await?;
    let attach = client.attach(&session_id, AttachMode::Shared).await?;
    let notif_rx = client.subscribe();
    let tab_id = attach
        .snapshot
        .as_ref()
        .and_then(|s| s.tabs.first().map(|t| t.id.clone()))
        .unwrap_or_else(|| "tab-0".to_string());

    // 启动一个立刻退出的命令。
    let pane_id = client
        .spawn_pane(
            &session_id,
            &tab_id,
            mux_protocol::TerminalSize { cols: 80, rows: 24 },
            Some(ShellCommand {
                program: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "exit 0".to_string()],
                env: HashMap::new(),
            }),
            None,
        )
        .await?;

    let event = wait_for_notification(
        &notif_rx,
        Duration::from_secs(10),
        |event| matches!(event, NotifEvent::PaneRemoved(removed) if removed.pane_id == pane_id),
    )
    .await
    .context("server must fan out PaneRemoved when the shell exits")?;
    assert!(matches!(event, NotifEvent::PaneRemoved(_)));

    // pane 移除后 close 应报 not found (不残留 zombie)。
    let close_result = client.close_pane(&pane_id).await;
    assert!(
        close_result.is_err(),
        "closing an exited pane must report not found"
    );

    Ok(())
}
