//! §16.1 daemon 自动启动与连接管理

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use mux::MuxDomain;
use mux_protocol::TerminalSize;

// ============================================================================
// §16.12 GPUI 通知 — daemon 连接丢失/错误提示
// ============================================================================

use gpui::{App, SharedString, Task};
use ui::{Icon, IconName};

/// §16.12 显示 "daemon 连接丢失" 通知 (warning toast)
pub fn show_daemon_connection_lost(cx: &mut App) {
    notifications::status_toast::StatusToast::new(
        "Connection to mux_server lost. Reconnecting...",
        cx,
        |toast, _| {
            toast
                .icon(Icon::new(IconName::Warning).color(ui::Color::Warning))
                .auto_dismiss(true)
                .dismiss_button(true)
        },
    );
}

/// §16.12 显示 daemon 错误通知 (error toast)
pub fn show_daemon_error(cx: &mut App, error: impl Into<SharedString>) {
    notifications::status_toast::StatusToast::new(error, cx, |toast, _| {
        toast
            .icon(Icon::new(IconName::XCircle).color(ui::Color::Error))
            .auto_dismiss(false)
            .dismiss_button(true)
    });
}

/// §16.12 daemon 连接监视器: 后台检测连接状态并自动重连。
/// 当 MuxDomain 连接丢失时自动重连并显示 toast 通知。
pub fn watch_daemon_connection(
    _domain: std::sync::Arc<MuxDomain>,
    cx: &mut App,
) -> gpui::Task<()> {
    cx.spawn(async move |cx| {
        let socket_path = default_socket_path();
        loop {
            smol::Timer::after(Duration::from_secs(30)).await;

            if !socket_path.exists() {
                cx.update(|cx| show_daemon_connection_lost(cx));

                match ensure_daemon_running().await {
                    Ok(_) => {
                        tracing::info!("reconnected to daemon");
                    }
                    Err(reconnect_err) => {
                        let msg = format!("Failed to reconnect to daemon: {reconnect_err}");
                        cx.update(|cx| show_daemon_error(cx, msg));
                    }
                }
            }
        }
    })
}

/// 默认 socket 路径: $XDG_RUNTIME_DIR/z3rm/mux.sock 或 /tmp/z3rm/mux.sock (§16.1)
/// 测试与多实例场景可用 Z3RM_MUX_SOCKET 环境变量覆盖。
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("Z3RM_MUX_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir)
    } else {
        PathBuf::from("/tmp")
    }
    .join("z3rm")
    .join("mux.sock")
}

pub async fn ensure_daemon_running() -> Result<MuxDomain> {
    let socket_path = default_socket_path();
    eprintln!("[z3rm] Attempting connect to {}", socket_path.display());
    match mux::connect_local(&socket_path).await {
        Ok(domain) => {
            eprintln!("[z3rm] Connected to existing daemon");
            tracing::info!("connected to existing daemon");
            return Ok(domain);
        }
        Err(e) => {
            eprintln!("[z3rm] Connection failed: {}", e);
        }
    }
    eprintln!("[z3rm] Spawning daemon...");

    tracing::info!("daemon not running, spawning...");
    spawn_daemon()?;

    wait_for_socket(&socket_path, Duration::from_secs(5)).await?;
    eprintln!("[z3rm] Socket ready, connecting...");

    let domain = mux::connect_local(&socket_path)
        .await
        .context("failed to connect to daemon after spawn")?;
    eprintln!("[z3rm] Connected to daemon after spawn");
    tracing::info!("connected to daemon after spawn");
    Ok(domain)
}

/// 启动 z3rm-server daemon 进程 (§16.1)
fn spawn_daemon() -> Result<()> {
    // 从可执行文件同目录查找 z3rm-server (dev build 支持)
    let server_in_same_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("z3rm-server")));

    let binary_name = if let Some(ref path) = server_in_same_dir {
        if path.exists() {
            path.to_string_lossy().into_owned()
        } else {
            "z3rm-server".to_string()
        }
    } else {
        "z3rm-server".to_string()
    };
    let result = Command::new(&binary_name)
        .arg("--daemonize")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(_) => {
            tracing::info!(binary = %binary_name, "spawned daemon");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(anyhow::anyhow!(
                "z3rm-server not found. Build it with `cargo build -p mux_server` \
                 and ensure it is in PATH or next to the z3rm executable"
            ))
        }
        Err(e) => {
            Err(anyhow::anyhow!("failed to spawn daemon: {e}"))
        }
    }
}
/// 轮询等待 socket 就绪（验证 daemon 实际可连接）§16.1
async fn wait_for_socket(socket_path: &Path, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(100);
    let path = socket_path.to_path_buf();

    loop {
        if start.elapsed() > timeout {
            return Err(anyhow::anyhow!(
                "timed out waiting for daemon socket at {} ({:?})",
                socket_path.display(),
                timeout
            ));
        }

        // 尝试实际连接来验证 daemon 正在监听
        let p = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("mux-sock-check".into())
            .spawn(move || {
                let result = std::os::unix::net::UnixStream::connect(&p);
                let _ = tx.send(result);
            })
            .ok();
        let result = rx.recv()?;
        if result.is_ok() {
            tracing::info!(
                "daemon socket ready and connected at {} after {:?}",
                socket_path.display(),
                start.elapsed()
            );
            return Ok(());
        }

        // Sleep without blocking GPUI (use thread sleep in async context)
        std::thread::sleep(poll_interval);
    }
}

/// 首次启动时创建默认 session (§16.1)
pub async fn ensure_default_session(domain: &MuxDomain) -> Result<String> {
    let sessions = domain.list_sessions().await?;

    if sessions.is_empty() {
        // 创建默认 session，工作目录为 home 目录
        let cwd = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let session_id = domain.create_session("default", &cwd).await?;
        tracing::info!(session_id = %session_id, "created default session");
        Ok(session_id)
    } else {
        // 已有 session，使用第一个
        let session_id = sessions[0].id.clone();
        tracing::info!(session_id = %session_id, "using existing session");
        Ok(session_id)
    }
}
/// 如果 session 没有 pane，创建默认终端 (§16.1)
pub async fn ensure_pane_in_session(domain: &MuxDomain, session_id: &str) -> Result<()> {
    // Attach to get snapshot (Shared mode to allow reading)
    let attach_resp = domain.attach(session_id, mux::AttachMode::Shared).await?;
    let snapshot = attach_resp.snapshot.context("no snapshot in attach response")?;

    // Check if any tab has panes
    let has_panes = snapshot.tabs.iter().any(|tab| !tab.panes.is_empty());
    if has_panes {
        // Detach since we just needed to check
        domain.detach().await?;
        return Ok(());
    }
    // No panes — spawn a terminal in the first tab
    let tab_id = snapshot.tabs.first().map(|t| &t.id);
    let fallback = String::from("default");
    let tab_id = tab_id.unwrap_or(&fallback);

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let size = TerminalSize { cols: 120, rows: 40 };
    let pane_id = domain
        .spawn_pane(session_id, tab_id, size, None, Some(&home))
        .await?;
    tracing::info!(pane_id = %pane_id, "spawned default terminal pane");
    Ok(())
}

/// 获取 session 的第一个 pane id (用于创建 MuxPaneView)。
pub async fn get_first_pane_id(domain: &MuxDomain) -> Result<Option<String>> {
    let sessions = domain.list_sessions().await?;
    let session = sessions.first().context("no sessions")?;
    let session_id = &session.id;
    
    // Attach to get snapshot
    let attach_resp = domain.attach(session_id, mux::AttachMode::Shared).await?;
    let snapshot = attach_resp.snapshot.context("no snapshot")?;
    
    // Find first pane
    let pane_id = snapshot.tabs.iter()
        .flat_map(|t| &t.panes)
        .map(|p| &p.id)
        .next()
        .cloned();
    
    // Detach since we just needed to read
    let _ = domain.detach().await;
    
    Ok(pane_id)
}
