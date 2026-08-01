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

use gpui::{App, SharedString};
use ui::{Icon, IconName};

fn mount_status_toast(
    toast: gpui::Entity<notifications::status_toast::StatusToast>,
    cx: &mut gpui::App,
) {
    for window_handle in cx.windows() {
        let mounted = window_handle.update(cx, |_root, window, cx| {
            let Some(Some(multi)) = window.root::<workspace::MultiWorkspace>() else {
                return false;
            };
            let workspace = multi.read(cx).workspace().clone();
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_status_toast(toast.clone(), cx);
            });
            true
        });
        if matches!(mounted, Ok(true)) {
            return;
        }
    }
    tracing::warn!("no workspace available to mount daemon status toast");
}

/// §16.12 显示 "daemon 连接丢失" 通知 (warning toast)
pub fn show_daemon_connection_lost(cx: &mut App) {
    let toast = notifications::status_toast::StatusToast::new(
        "Connection to mux_server lost. Reconnecting...",
        cx,
        |toast, _| {
            toast
                .icon(Icon::new(IconName::Warning).color(ui::Color::Warning))
                .auto_dismiss(true)
                .dismiss_button(true)
        },
    );
    mount_status_toast(toast, cx);
}

/// §16.12 显示 daemon 错误通知 (error toast)
pub fn show_daemon_error(cx: &mut App, error: impl Into<SharedString>) {
    let toast = notifications::status_toast::StatusToast::new(error, cx, |toast, _| {
        toast
            .icon(Icon::new(IconName::XCircle).color(ui::Color::Error))
            .auto_dismiss(false)
            .dismiss_button(true)
    });
    mount_status_toast(toast, cx);
}

/// §16.12 / §15.12 daemon 连接监视器 — 后台检测连接状态并在丢失时做权威
/// 重连 (§15.4 in-place swap)。会话 ID 由调用方持有, 重连期间原 `Arc<MuxDomain>`
/// 被原地换上新传输/inner, 保留 `window_id` 与所有通知订阅者, 并主动广播
/// 一条 `SessionLayoutChanged` 触发下游快照重对账。
pub fn watch_daemon_connection(
    domain: std::sync::Arc<MuxDomain>,
    session_id: String,
    cx: &mut App,
) -> gpui::Task<()> {
    cx.spawn(async move |cx| {
        const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        let mut backoff = INITIAL_BACKOFF;
        loop {
            cx.background_executor().timer(backoff).await;

            // §15.4 Probe the live connection (issues a real RPC), not just
            // socket presence — a stale socket file can outlive a dead daemon.
            if domain.check_connection().await {
                backoff = INITIAL_BACKOFF;
                continue;
            }

            // §16.12 Connection lost: surface it and escalate. Spawn-then-
            // reconnect is a fallback for the case where the daemon process
            // itself died. Successful reconnect uses the same exponential
            // back-off envelope below.
            cx.update(|cx| show_daemon_connection_lost(cx));
            if let Err(spawn_err) = ensure_daemon_running().await {
                tracing::warn!(error = %spawn_err, "ensure_daemon_running failed before reconnect");
            }

            match domain
                .reconnect_local_in_place(&session_id, mux::AttachMode::Shared)
                .await
            {
                Ok(_) => {
                    tracing::info!(session_id = %session_id, "reconnected to daemon in place");
                    cx.update(|cx| show_daemon_error(cx, "Mux reconnected"));
                    backoff = INITIAL_BACKOFF;
                }
                Err(reconnect_err) => {
                    let msg = format!("Failed to reconnect to mux: {reconnect_err}");
                    tracing::warn!(error = %reconnect_err, "reconnect attempt failed");
                    cx.update(|cx| show_daemon_error(cx, msg));
                    backoff = (backoff * 2).min(MAX_BACKOFF);
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

/// §16.1 环境变量覆盖 (测试 / 排障用), 优先于 settings.json。
const CONNECT_TIMEOUT_ENV: &str = "Z3RM_MUX_CONNECT_TIMEOUT_MS";
const DAEMON_STARTUP_TIMEOUT_ENV: &str = "Z3RM_MUX_DAEMON_STARTUP_TIMEOUT_MS";

/// §16.1 spawn 之后等待 daemon 就绪的最小预算。
const MIN_DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// §16.1 单次连接尝试的超时预算 (`mux.connect_timeout_ms`, 默认 500ms)。
///
/// Read from settings.json rather than `SettingsStore`, because the connection
/// path runs without an `App` handle: `ensure_daemon_running` is called from a
/// detached startup task and again from the background reconnect watcher.
pub fn connect_timeout() -> Duration {
    if let Some(timeout) = env_duration(CONNECT_TIMEOUT_ENV) {
        return timeout;
    }
    configured_mux_settings()
        .unwrap_or_default()
        .connect_timeout()
}

/// §16.1 读取 settings.json 里的 `mux` 配置块 (用户设置优先, 其次全局设置)。
///
/// Whichever file declares a `mux` block wins as a whole; per-field merging is
/// `SettingsStore`'s job and is unavailable this early.
fn configured_mux_settings() -> Option<settings::MuxSettingsContent> {
    [paths::settings_file(), paths::global_settings_file()]
        .into_iter()
        .find_map(|path| {
            let contents = std::fs::read_to_string(path).ok()?;
            mux_settings_from_json(&contents)
        })
}

/// §16.1 从 settings.json 文本中解析 `mux` 配置块。
///
/// Settings files are JSON with comments, and a syntax error elsewhere in the
/// file must not make the daemon unreachable — every failure degrades to the
/// documented defaults.
fn mux_settings_from_json(contents: &str) -> Option<settings::MuxSettingsContent> {
    let root: serde_json::Value = settings::parse_json_with_comments(contents).ok()?;
    serde_json::from_value(root.get("mux")?.clone()).ok()
}

/// §16.1 spawn 之后等待 daemon 接受连接的上限。
///
/// Derived from the connect timeout so both move together when a user raises
/// it for a slow machine, with a floor that preserves today's 5s budget at the
/// 500ms default.
fn daemon_startup_timeout(connect_timeout: Duration) -> Duration {
    if let Some(timeout) = env_duration(DAEMON_STARTUP_TIMEOUT_ENV) {
        return timeout;
    }
    (connect_timeout * 10).max(MIN_DAEMON_STARTUP_TIMEOUT)
}

fn env_duration(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_millis)
}

/// §16.1 带超时的连接尝试。
///
/// `mux::connect_local` has no timeout of its own, and §16.1 defines "connect
/// timeout" as the signal to spawn a daemon — so the bound has to be enforced
/// here rather than waiting indefinitely on an unresponsive socket.
async fn connect_with_timeout(socket_path: Option<&Path>, timeout: Duration) -> Result<MuxDomain> {
    let connect = std::pin::pin!(mux::connect_local(socket_path));
    let deadline = std::pin::pin!(smol::Timer::after(timeout));
    match futures::future::select(connect, deadline).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => Err(anyhow::anyhow!(
            "timed out connecting to mux socket after {:?}",
            timeout
        )),
    }
}

pub async fn ensure_daemon_running() -> Result<MuxDomain> {
    // §3.2 先尝试连接默认路径; 失败则 spawn daemon 再重试。
    let connect_timeout = connect_timeout();
    eprintln!("[z3rm] Attempting connect to default socket");
    match connect_with_timeout(None, connect_timeout).await {
        Ok(domain) => {
            eprintln!("[z3rm] Connected to existing daemon");
            return Ok(domain);
        }
        Err(e) => {
            eprintln!("[z3rm] Connection failed: {}", e);
        }
    }
    eprintln!("[z3rm] Spawning daemon...");
    tracing::info!("daemon not running, spawning...");
    spawn_daemon()?;

    // §16.1 Poll until the daemon accepts a real protocol connection. The
    // successful domain is returned directly, avoiding a throwaway I/O worker.
    let domain = wait_for_socket(
        &default_socket_path(),
        connect_timeout,
        daemon_startup_timeout(connect_timeout),
    )
    .await?;
    eprintln!("[z3rm] Connected to daemon after spawn");
    Ok(domain)
}

/// 启动 z3rm-server daemon 进程 (§16.1)
///
/// §16.1 The socket is deliberately left in place: a connect timeout does not
/// prove the old daemon is dead, and deleting a socket it is still listening on
/// would let two daemons own the same path. Reclaiming a genuinely dead
/// socket — and refusing to start when a live one is found — is the new
/// daemon's job (`mux_server::bind_or_cleanup`).
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
        .envs(std::env::vars()) // inherit parent environment including $SHELL
        .spawn();
    match result {
        Ok(_) => {
            tracing::info!(binary = %binary_name, "spawned daemon");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "z3rm-server not found. Build it with `cargo build -p mux_server` \
                 and ensure it is in PATH or next to the z3rm executable"
        )),
        Err(e) => Err(anyhow::anyhow!("failed to spawn daemon: {e}")),
    }
}
/// Poll until the daemon socket accepts a real protocol connection (§16.1).
/// Connection attempts and delays are executor-neutral, so daemon cold start
/// does not block GPUI's foreground executor.
///
/// Each attempt is bounded by `connect_timeout` so one unresponsive socket
/// cannot swallow the whole `startup_timeout` budget.
async fn wait_for_socket(
    socket_path: &Path,
    connect_timeout: Duration,
    startup_timeout: Duration,
) -> Result<MuxDomain> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(100);

    loop {
        if start.elapsed() > startup_timeout {
            return Err(anyhow::anyhow!(
                "timed out waiting for daemon socket at {} ({:?})",
                socket_path.display(),
                startup_timeout
            ));
        }

        match connect_with_timeout(Some(socket_path), connect_timeout).await {
            Ok(domain) => {
                tracing::info!(
                    path = %socket_path.display(),
                    elapsed = ?start.elapsed(),
                    "daemon socket ready and accepting connections"
                );
                return Ok(domain);
            }
            Err(error) => {
                tracing::trace!(
                    path = %socket_path.display(),
                    %error,
                    "daemon socket not ready yet"
                );
            }
        }

        smol::Timer::after(poll_interval).await;
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

/// 解析 GUI 启动附带的 target session 并返回 session ID。
///
/// §3.10 `z3rm attach [-t target]` 启动 GUI 时携带 target：
/// - `Some(name_or_id)` -> 按 id 或 name 查找现有 session；找不到则报错。
/// - `None` -> 退回 `ensure_default_session` 的语义（创建或复用默认 session）。
///
/// 错误必须传播（不静默丢弃 `list_sessions` / 解析失败）。
pub async fn ensure_target_session(domain: &MuxDomain, target: Option<&str>) -> Result<String> {
    match target {
        None => ensure_default_session(domain).await,
        Some(raw) => {
            if raw.is_empty() {
                anyhow::bail!("attach target must not be empty");
            }
            let filtered: String = raw.chars().filter(|ch| !ch.is_whitespace()).collect();
            if filtered.is_empty() {
                anyhow::bail!("attach target must not be empty");
            }
            let sessions = domain.list_sessions().await?;
            let session = sessions
                .iter()
                .find(|session| session.id == filtered || session.name == filtered)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "session '{}' not found (existing: {:?})",
                        filtered,
                        sessions.iter().map(|s| &s.name).collect::<Vec<_>>()
                    )
                })?;
            Ok(session.id.clone())
        }
    }
}
/// 如果 session 没有 pane，创建默认终端 (§16.1)
pub async fn ensure_pane_in_session(domain: &MuxDomain, session_id: &str) -> Result<()> {
    // Attach to get snapshot (Shared mode to allow reading)
    let attach_resp = domain.attach(session_id, mux::AttachMode::Shared).await?;
    let snapshot = attach_resp
        .snapshot
        .context("no snapshot in attach response")?;

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
    let size = TerminalSize {
        cols: 120,
        rows: 40,
    };
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
    let pane_id = snapshot
        .tabs
        .iter()
        .flat_map(|t| &t.panes)
        .map(|p| &p.id)
        .next()
        .cloned();

    // Detach since we just needed to read
    if let Err(e) = domain.detach().await {
        tracing::error!(error = %e, "detach failed during get_first_pane_id");
    }

    Ok(pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §16.1 `mux.connect_timeout_ms` must survive the comments Zed-style
    /// settings files are allowed to contain.
    #[test]
    fn connect_timeout_is_read_from_settings_json() {
        let contents = r#"{
            // user settings
            "theme": "One Dark",
            "mux": {
                "connect_timeout_ms": 1200,
                "keep_alive": true
            }
        }"#;
        let mux = mux_settings_from_json(contents).expect("mux block should parse");
        assert_eq!(mux.connect_timeout(), Duration::from_millis(1200));
    }

    /// §16.1 A `mux` block without the key falls back to the documented default.
    #[test]
    fn missing_connect_timeout_falls_back_to_default() {
        let mux = mux_settings_from_json(r#"{"mux": {"keep_alive": true}}"#)
            .expect("mux block should parse");
        assert_eq!(mux.connect_timeout(), Duration::from_millis(500));
    }

    /// Malformed or mux-less settings must not make the daemon unreachable.
    #[test]
    fn unusable_settings_yield_no_override() {
        assert!(mux_settings_from_json(r#"{"theme": "One Dark"}"#).is_none());
        assert!(mux_settings_from_json("{ this is not json").is_none());
        assert_eq!(
            mux_settings_from_json(r#"{"mux": {}}"#)
                .expect("empty mux block should parse")
                .connect_timeout(),
            Duration::from_millis(500)
        );
    }

    /// §16.1 The spawn-and-wait budget scales with the configured connect
    /// timeout, and never drops below the 5s floor the default produces.
    #[test]
    fn startup_timeout_tracks_connect_timeout() {
        assert_eq!(
            daemon_startup_timeout(Duration::from_millis(500)),
            Duration::from_secs(5)
        );
        assert_eq!(
            daemon_startup_timeout(Duration::from_millis(100)),
            Duration::from_secs(5)
        );
        assert_eq!(
            daemon_startup_timeout(Duration::from_millis(2000)),
            Duration::from_secs(20)
        );
    }

    /// §16.1 A connect attempt against a path nobody listens on must fail
    /// within the configured budget instead of hanging.
    ///
    /// This drives a real socket connect, so it runs on a real executor rather
    /// than GPUI's deterministic one, which rejects the background I/O thread.
    #[test]
    fn connect_gives_up_after_the_timeout() {
        let socket_path = std::env::temp_dir().join(format!(
            "z3rm-absent-{}-connect-timeout.sock",
            std::process::id()
        ));
        let started = std::time::Instant::now();
        let result = smol::block_on(connect_with_timeout(
            Some(&socket_path),
            Duration::from_millis(200),
        ));
        assert!(result.is_err(), "connecting to a missing socket must fail");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "connect must respect the timeout budget, took {:?}",
            started.elapsed()
        );
    }
}
