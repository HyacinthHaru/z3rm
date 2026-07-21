//! # 真正的端到端集成测试
//!
//! Spec §13 / Plan 23 要求:daemon spawn → create session → spawn pane →
//! type input → fetch grid update → verify content → split pane → detach →
//! reattach → verify state → close session.
//!
//! 此测试启动真实的 `z3rm-server` 子进程,通过 Unix socket 连接,
//! 驱动完整协议链路。任何协议层 bug 都会被捕获。

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto;
use mux_protocol::proto::split_node::SplitDirection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// §13 测试用的隔离 mux_server 进程。
///
/// 每个 `TestServer` 独占一个 `TempDir`,在该目录下创建 socket 文件,
/// 通过 `Z3RM_MUX_SOCKET` 环境变量告诉 server 把 socket 绑定到那里。
/// Drop 时杀进程;socket 文件随 TempDir 一起清理。
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

        // 找到 z3rm-server 可执行文件:优先 CARGO_BIN_EXE 之类的环境变量,
        // 然后从当前可执行文件目录回溯,最后用 PATH。
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

        // 等待 socket 文件出现 (server 启动 + bind)
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
        // 额外等一下让 listener.enter() 就绪
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

/// 等待 pane grid 出现包含 `needle` 的内容,或超时失败。
///
/// mux_server 处理 PTY 输出 + 推送 PaneDirty + 客户端拉 grid 是异步的,
/// 所以轮询 fetch_grid_update 直到看到目标字符串。
async fn wait_for_grid_contains(
    domain: &MuxDomain,
    pane_id: &str,
    needle: &str,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let resp = domain.fetch_grid_update(pane_id, 0).await?;
        if let Some(update) = &resp.update {
            let text = grid_text(update);
            if text.contains(needle) {
                return Ok(text);
            }
        }
        if Instant::now() >= deadline {
            let last = resp
                .update
                .as_ref()
                .map(grid_text)
                .unwrap_or_default();
            anyhow::bail!(
                "timeout waiting for {:?} in pane {} grid. Last grid:\n{}",
                needle,
                pane_id,
                last
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 把 GridUpdate (Diff 或 FullSnapshot) 渲染成纯文本用于断言。
fn grid_text(update: &proto::fetch_grid_update_response::Update) -> String {
    use proto::fetch_grid_update_response::Update;
    match update {
        Update::Diff(diff) => {
            // 把所有 RowChange 拼起来 (每行按 cell.char 顺序)
            let mut rows: std::collections::BTreeMap<u32, String> = Default::default();
            for rc in &diff.rows {
                let line: String = rc.cells.iter().map(|c| c.char.clone()).collect();
                rows.insert(rc.row, line);
            }
            rows.into_values().collect::<Vec<_>>().join("\n")
        }
        Update::FullSnapshot(snap) => {
            let cols = snap.cols as usize;
            if cols == 0 {
                return String::new();
            }
            let mut out = String::new();
            for (i, cell) in snap.cells.iter().enumerate() {
                out.push_str(&cell.char);
                if (i + 1) % cols == 0 {
                    out.push('\n');
                }
            }
            out
        }
    }
}

// ============================================================================
// §13 端到端测试
// ============================================================================

/// §13 完整会话生命周期:spawn daemon → list → create → attach →
/// spawn pane → type → fetch grid → verify → kill → cleanup。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_full_session_lifecycle() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    // 1. list_sessions: 初始应为空
    let initial = domain.list_sessions().await?;
    assert!(
        initial.is_empty(),
        "fresh daemon should have 0 sessions, got {:?}",
        initial
    );

    // 2. create_session
    let session_id = domain
        .create_session("e2e-lifecycle", &PathBuf::from("/"))
        .await?;
    assert!(!session_id.is_empty(), "session id should not be empty");

    // 3. list_sessions 应反映新会话
    let after_create = domain.list_sessions().await?;
    assert_eq!(after_create.len(), 1);
    assert_eq!(after_create[0].id, session_id);
    assert_eq!(after_create[0].name, "e2e-lifecycle");

    // 4. attach
    let attach = domain
        .attach(&session_id, AttachMode::Shared)
        .await
        .context("attach failed")?;
    let snapshot = attach
        .snapshot
        .as_ref()
        .context("attach returned empty snapshot")?;
    let initial_tab = snapshot
        .tabs
        .first()
        .context("attach snapshot has no tabs")?;
    let initial_tab_id = initial_tab.id.clone();

    // 5. spawn_pane — 用 cat 作为命令,这样我们可以确定地往它写入并看到回显
    let size = proto::TerminalSize { cols: 40, rows: 10 };
    let command = proto::ShellCommand {
        program: "/bin/cat".into(),
        args: vec![],
        env: HashMap::new(),
    };
    let pane_id = domain
        .spawn_pane(
            &session_id,
            &initial_tab_id,
            size,
            Some(command),
            Some(&PathBuf::from("/")),
        )
        .await
        .context("spawn_pane failed")?;
    assert!(!pane_id.is_empty(), "pane id should not be empty");

    // 6. send_input — 给 cat 输入 "hello-z3rm\n",cat 会原样回显
    domain
        .send_input(&pane_id, b"hello-z3rm\n")
        .await
        .context("send_input failed")?;

    // 7. fetch_grid_update 轮询直到看到 "hello-z3rm" (cat 回显两次:输入行 + 输出行)
    let grid =
        wait_for_grid_contains(&domain, &pane_id, "hello-z3rm", Duration::from_secs(5)).await?;
    assert!(
        grid.matches("hello-z3rm").count() >= 2,
        "expected cat to echo 'hello-z3rm' twice (typed + echoed), got:\n{}",
        grid
    );

    // 8. kill_session
    domain.kill_session(&session_id).await?;

    // 9. list_sessions 应恢复为空
    let after_kill = domain.list_sessions().await?;
    assert!(
        after_kill.is_empty(),
        "after kill_session, expected 0 sessions, got {:?}",
        after_kill
    );

    Ok(())
}

/// §3.5 / §15.4 detach + reattach + full snapshot 对账。
///
/// 这条路径是 mux 设计的核心:client 断开后 server 持续运行,
/// 重新 attach 必须从权威快照恢复所有 pane 的状态。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_detach_reattach_preserves_state() -> Result<()> {
    let server = TestServer::spawn()?;

    // 阶段 1:第一个 client 创建 session + pane,输入已知内容
    let client_a = server.connect().await?;
    let session_id = client_a
        .create_session("e2e-reattach", &PathBuf::from("/"))
        .await?;

    let attach_a = client_a.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach_a
        .snapshot
        .as_ref()
        .context("attach_a snapshot")?
        .tabs
        .first()
        .context("no tabs")?
        .id
        .clone();

    let pane_id = client_a
        .spawn_pane(
            &session_id,
            &tab_id,
            proto::TerminalSize { cols: 40, rows: 5 },
            Some(proto::ShellCommand {
                program: "/bin/cat".into(),
                args: vec![],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await?;

    client_a.send_input(&pane_id, b"persisted-marker\n").await?;
    wait_for_grid_contains(&client_a, &pane_id, "persisted-marker", Duration::from_secs(5)).await?;

    // 阶段 2:detach (client_a drop 模拟窗口关闭)
    client_a.detach().await?;
    drop(client_a);

    // 给 server 一点时间检测到 socket EOF
    std::thread::sleep(Duration::from_millis(300));

    // 阶段 3:第二个 client 连上,attach 同一个 session,验证 pane 还在
    let client_b = server.connect().await?;
    let attach_b = client_b.attach(&session_id, AttachMode::Shared).await?;
    let snap_b = attach_b
        .snapshot
        .as_ref()
        .context("attach_b snapshot should be non-empty")?;

    // session 中应该还有至少一个 tab,tab 中应该还有 pane
    assert!(
        !snap_b.tabs.is_empty(),
        "reattach snapshot must contain tabs"
    );
    let tab_b = snap_b
        .tabs
        .iter()
        .find(|t| t.id == tab_id)
        .context("reattach snapshot missing original tab")?;
    assert!(
        tab_b
            .panes
            .iter()
            .any(|p| p.id == pane_id && p.is_alive),
        "reattach snapshot must list original pane as alive"
    );

    // 阶段 4:fetch grid 应该仍然包含 "persisted-marker"
    let grid =
        wait_for_grid_contains(&client_b, &pane_id, "persisted-marker", Duration::from_secs(5))
            .await?;
    assert!(
        grid.contains("persisted-marker"),
        "reattached grid should contain persisted content, got:\n{}",
        grid
    );

    // 清理
    client_b.kill_session(&session_id).await?;
    Ok(())
}

/// §3.3 split_pane 在真链路上必须返回新 pane id 且原 pane 不受影响。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_split_pane_creates_distinct_pane() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    let session_id = domain
        .create_session("e2e-split", &PathBuf::from("/"))
        .await?;
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach.snapshot.as_ref().unwrap().tabs[0].id.clone();

    let pane1 = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            proto::TerminalSize { cols: 40, rows: 5 },
            Some(proto::ShellCommand {
                program: "/bin/cat".into(),
                args: vec![],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await?;

    let pane2 = domain
        .split_pane(&pane1, SplitDirection::LeftRight)
        .await
        .context("split_pane failed")?;

    assert_ne!(pane1, pane2, "split must produce a new pane id");

    // 写入两个 pane 不同的标记,各自应该只在自己的 grid 里出现
    domain.send_input(&pane1, b"pane-one-marker\n").await?;
    domain.send_input(&pane2, b"pane-two-marker\n").await?;

    let grid1 =
        wait_for_grid_contains(&domain, &pane1, "pane-one-marker", Duration::from_secs(5)).await?;
    let grid2 =
        wait_for_grid_contains(&domain, &pane2, "pane-two-marker", Duration::from_secs(5)).await?;

    assert!(
        grid1.contains("pane-one-marker") && !grid1.contains("pane-two-marker"),
        "pane1 grid should contain only its own marker, got:\n{}",
        grid1
    );
    assert!(
        grid2.contains("pane-two-marker") && !grid2.contains("pane-one-marker"),
        "pane2 grid should contain only its own marker, got:\n{}",
        grid2
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §3.3 generation 计数器必须在 PTY 输出后递增。
///
/// 这是 §15.4 / §16.3 的核心不变量:所有渲染相关状态变化都要 bump generation。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_generation_advances_on_pty_output() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    let session_id = domain
        .create_session("e2e-gen", &PathBuf::from("/"))
        .await?;
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach.snapshot.as_ref().unwrap().tabs[0].id.clone();

    let pane = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            proto::TerminalSize { cols: 40, rows: 5 },
            Some(proto::ShellCommand {
                program: "/bin/cat".into(),
                args: vec![],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await?;

    // 等初始 prompt / shell banner
    let first = domain.fetch_grid_update(&pane, 0).await?;
    let baseline_gen = first.to_generation;

    // 输入内容,触发 PTY 输出
    domain.send_input(&pane, b"gen-test\n").await?;
    wait_for_grid_contains(&domain, &pane, "gen-test", Duration::from_secs(5)).await?;

    let after = domain.fetch_grid_update(&pane, 0).await?;
    assert!(
        after.to_generation > baseline_gen,
        "generation must advance after PTY output: baseline={}, after={}",
        baseline_gen,
        after.to_generation
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §15.4 / §16.9 split 出来的 pane 必须出现在 attach 快照的 tab.panes 里。
///
/// 这个测试专门捕获一类 bug:server 把新 pane 加到 `session.panes` 平铺 map,
/// 但忘了同步加到 `session.tabs[*].pane_ids`。结果:fetch_grid_update(pane_id)
/// 能拿到 grid,但 attach snapshot 不含该 pane,status 命令和 GUI 看不到它。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_split_pane_visible_in_attach_snapshot() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    let session_id = domain
        .create_session("e2e-split-snapshot", &PathBuf::from("/"))
        .await?;
    let attach = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attach.snapshot.as_ref().unwrap().tabs[0].id.clone();

    let pane1 = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            proto::TerminalSize { cols: 40, rows: 5 },
            Some(proto::ShellCommand {
                program: "/bin/cat".into(),
                args: vec![],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await?;

    let pane2 = domain
        .split_pane(&pane1, SplitDirection::LeftRight)
        .await?;

    // 重新 attach 拿权威快照
    let reattach = domain.attach(&session_id, AttachMode::ReadOnly).await?;
    let snap = reattach
        .snapshot
        .as_ref()
        .context("reattach snapshot missing")?;

    // 验证两个 pane 都在某个 tab 的 pane_ids 里
    let all_panes: Vec<String> = snap
        .tabs
        .iter()
        .flat_map(|t| t.panes.iter().map(|p| p.id.clone()))
        .collect();
    assert!(
        all_panes.contains(&pane1),
        "pane1 must be in attach snapshot: {:?}",
        all_panes
    );
    assert!(
        all_panes.contains(&pane2),
        "pane2 (split result) must be in attach snapshot: {:?}. \
         If missing, handle_split_pane forgot to update tab.pane_ids.",
        all_panes
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}
