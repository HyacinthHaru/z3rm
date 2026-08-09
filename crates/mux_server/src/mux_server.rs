// §3.1 mux_server — mux_server 守护进程库。
// 管理 PTY、alacritty 终端模拟、layout 引擎、session 持久化。

use anyhow::{Context as _, Result};
use interprocess::local_socket::tokio::Listener as LocalSocketListener;
use sqlez::connection::Connection;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use tokio::time::Duration;

pub mod connection;
mod server_settings;

pub mod clipboard;
pub mod coalescing;
pub mod dec2026;
pub mod extension_host;
pub mod grid_sync;
pub mod layout;
pub mod pane;
pub mod persistence;
pub mod snapshot;

pub mod session;
#[cfg(test)]
mod tests;

// ============================================================================
// §16.12 日志系统 — 文件日志 + 轮转
// ============================================================================

/// 获取日志目录路径 (§16.12)
pub(crate) fn get_log_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("Library/Logs")
            .join("z3rm")
    } else {
        dirs::data_local_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")))
            .join("z3rm")
            .join("logs")
    }
}

/// §16.12 日志文件路径 (主文件)
static LOG_FILE_PATH: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| get_log_dir().join("mux-server.log"));

/// §16.12 日志轮转路径 (旧文件)
static LOG_FILE_ROTATE: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| get_log_dir().join("mux-server.log.old"));

/// §16.14 初始化文件日志 (zlog) + 轮转配置
///
/// 日志只写文件, 不污染 stderr — daemon 通常被 GUI 客户端 spawn,
/// 继承 stderr 会把日志喷到 GUI 启动终端 (spec §16.14)。
/// 调试时用 `tail -f ~/.local/share/z3rm/logs/mux-server.log`。
/// 显式调试可用 `--verbose` flag 开启 stderr。
pub fn setup_logging() -> Result<()> {
    // §16.14 初始化 zlog 框架
    zlog::init();

    // §16.14 仅当显式请求 verbose 时才输出到 stderr
    if std::env::var("Z3RM_MUX_VERBOSE").as_deref() == Ok("1") {
        zlog::init_output_stderr();
    }

    // §16.14 创建日志目录
    let log_dir = get_log_dir();
    std::fs::create_dir_all(&log_dir)?;

    // §16.14 初始化文件日志输出 + 轮转
    zlog::init_output_file(&LOG_FILE_PATH, Some(&LOG_FILE_ROTATE))?;

    zlog::info!(
        "mux_server logging initialized, log_dir={}",
        log_dir.display()
    );
    Ok(())
}

/// 默认 socket 路径: $XDG_RUNTIME_DIR/z3rm/mux.sock (Unix §16.1)
/// 或 \\.\pipe\z3rm-mux (Windows)
fn default_socket_name() -> Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, GenericNamespaced, prelude::*};
    if let Ok(path) = std::env::var("Z3RM_MUX_SOCKET") {
        return path
            .to_fs_name::<GenericFilePath>()
            .map_err(|error| anyhow::anyhow!("invalid socket path: {error}"));
    }
    #[cfg(unix)]
    {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let path = std::path::PathBuf::from(runtime_dir)
            .join("z3rm")
            .join("mux.sock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory {}", parent.display()))?;
        }
        path.to_string_lossy()
            .to_string()
            .to_fs_name::<GenericFilePath>()
            .map_err(|error| anyhow::anyhow!("invalid socket path: {error}"))
    }
    #[cfg(windows)]
    {
        r"\\.\pipe\z3rm-mux"
            .to_ns_name::<GenericNamespaced>()
            .map_err(|error| anyhow::anyhow!("invalid pipe name: {error}"))
    }
}

/// §16.1 Unix socket 文件系统路径 (绑定后设置 0600 权限用)。
/// 与 default_socket_name 同源: 优先 $Z3RM_MUX_SOCKET, 否则 $XDG_RUNTIME_DIR/z3rm/mux.sock。
#[cfg(unix)]
fn unix_socket_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("Z3RM_MUX_SOCKET") {
        return Some(std::path::PathBuf::from(p));
    }
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    Some(
        std::path::PathBuf::from(runtime_dir)
            .join("z3rm")
            .join("mux.sock"),
    )
}

async fn bind_socket(name: &interprocess::local_socket::Name<'_>) -> Result<LocalSocketListener> {
    use interprocess::local_socket::tokio::prelude::*;
    let listener = LocalSocketListener::from_options(
        interprocess::local_socket::ListenerOptions::new().name(name.borrow()),
    )?;
    Ok(listener)
}

/// §3.2 Try to bind, falling back to stale socket cleanup.
///
/// On Unix, if  fails and a socket file exists, try connecting to it.
/// If the connection fails (stale socket), remove the socket file and retry.
/// On Windows, named pipes are ephemeral (server disappears = pipe gone),
/// so stale cleanup is unnecessary.
pub async fn bind_or_cleanup(
    name: &interprocess::local_socket::Name<'_>,
) -> Result<LocalSocketListener> {
    match bind_socket(name).await {
        Ok(listener) => Ok(listener),
        Err(e) => {
            #[cfg(unix)]
            if let Some(socket_path) = unix_socket_path() {
                if socket_path.exists() {
                    use std::os::unix::net::UnixStream;
                    match UnixStream::connect(&socket_path) {
                        Ok(_) => {
                            // Active server exists — return original error
                            return Err(e);
                        }
                        Err(_) => {
                            // Stale socket — remove and retry
                            zlog::warn!("stale socket detected, cleaning: {:?}", socket_path);
                            if let Err(e) = std::fs::remove_file(&socket_path) {
                                tracing::warn!(error = %e, "remove stale socket failed");
                            }
                            return bind_socket(name).await;
                        }
                    }
                }
            }
            Err(e)
        }
    }
}

/// §3.6 初始化数据库连接
fn init_database(db_path: &PathBuf) -> Result<Connection> {
    let db = Connection::open_file(db_path.to_str().unwrap_or("file::memory:?mode=memory"));
    // §3.6 初始化持久化表
    persistence::init_tables(&db)?;
    Ok(db)
}

/// 启动守护进程 (§3.1)
pub struct ShutdownState {
    pub requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub ack_request_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub acked: std::sync::Arc<tokio::sync::Notify>,
}

pub fn run() -> Result<()> {
    // §16.12 初始化日志系统
    setup_logging()?;

    // 创建 Tokio runtime，所有异步操作都在其上下文中执行
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let socket_name = default_socket_name()?;
        let listener = match bind_or_cleanup(&socket_name).await {
            Ok(l) => l,
            Err(e) => {
                zlog::error!("socket bind failed: error={}", e);
                return Err(e);
            }
        };

        // §16.1 socket 权限 0600: 仅同 UID 可连接 —— §9 fail-open 角色模型的安全前提。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(socket_path) = unix_socket_path() {
                std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        zlog::info!("mux_server listening");

        // §16.1 DB 路径: Unix 下复用 socket 父目录; Windows 下用 %LOCALAPPDATA%/z3rm
        let db_path = if let Ok(p) = std::env::var("Z3RM_MUX_DB") {
            PathBuf::from(p)
        } else {
            #[cfg(unix)]
            {
                let runtime_dir =
                    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
                let parent = PathBuf::from(runtime_dir).join("z3rm");
                if let Err(e) = std::fs::create_dir_all(&parent) {
                    tracing::warn!(error = %e, "create_dir_all failed");
                }
                parent.join("mux.db")
            }
            #[cfg(windows)]
            {
                let base = dirs::data_local_dir().unwrap_or_else(|| {
                    PathBuf::from(std::env::var("TEMP").unwrap_or_else(|_| "C:/Temp".to_string()))
                });
                let dir = base.join("z3rm");
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    tracing::warn!(error = %e, "create_dir_all failed");
                }
                dir.join("mux.db")
            }
        };
        let db = init_database(&db_path)?;

        // Persisted rows are recovery candidates, not live sessions. Recreating
        // shells is destructive and must be explicitly confirmed; publishing
        // empty Session objects here made attach falsely report successful
        // recovery and caused the GUI to spawn unrelated replacement shells.
        let recovery_scan = persistence::recovery_candidates(&db)?;
        for error in &recovery_scan.rejected {
            tracing::warn!(%error, "rejected invalid mux recovery candidate");
        }
        tracing::info!(
            count = recovery_scan.candidates.len(),
            rejected = recovery_scan.rejected.len(),
            "loaded mux recovery candidates pending confirmation"
        );
        let sessions = std::sync::Arc::new(parking_lot::RwLock::new(Vec::new()));
        let db = std::sync::Arc::new(parking_lot::Mutex::new(db));

        // §16.8 Server-side QuickJS extension host: dedicated OS thread;
        // discovery/load failures log and never stop the daemon (§15.7).
        let extension_host = extension_host::ServerExtensionHost::start(
            sessions.clone(),
            extension_host::default_user_extensions_dir(),
        );

        let sessions_clone = sessions.clone();
        let db_clone = db.clone();
        let persist_handle = tokio::spawn(async move {
            persistence::persist_loop(sessions_clone, db_clone).await;
        });

        let clipboard = std::sync::Arc::new(clipboard::ServerClipboard::new());

        // §3.5 / §16.11 keep_alive + scrollback from ServerSettings (env + server.json).
        // keep_alive_seconds is read live from the AtomicU64 on each idle re-arm
        // so hot-reload of server.json takes effect on the next idle cycle.
        let server_settings = crate::server_settings::ServerSettings::load();
        let keep_alive_seconds = server_settings.keep_alive_seconds();

        if keep_alive_seconds > 0 {
            zlog::info!(
                "keep_alive enabled: idle_timeout={}s (hot-reloadable)",
                keep_alive_seconds
            );
        }
        let server = Server {
            sessions,
            _db: db,
            _persist_handle: Some(persist_handle),
            clipboard,
            extension_host,
            start_time: SystemTime::now(),
            server_settings: server_settings.clone(),
            // §3.5 active connection counter — drives the idle-shutdown timer.
            active_connections: std::sync::Arc::new(AtomicUsize::new(0)),
        };

        server.run(listener).await
    })
}

/// 服务器主结构 (§3.1)
pub struct Server {
    // §3.2 session 注册表
    sessions: std::sync::Arc<parking_lot::RwLock<Vec<session::Session>>>,
    // §3.6 SQLite 持久化连接
    _db: std::sync::Arc<parking_lot::Mutex<Connection>>,
    // §3.6 持久化后台任务句柄
    _persist_handle: Option<tokio::task::JoinHandle<()>>,
    // §16.6 服务器剪贴板
    clipboard: std::sync::Arc<clipboard::ServerClipboard>,
    // §16.8 服务端 QuickJS 扩展宿主 (专用线程)。
    extension_host: std::sync::Arc<extension_host::ServerExtensionHost>,
    start_time: SystemTime,
    // §16.11 Shared server settings (env + server.json); hot-reloaded.
    // keep_alive_seconds is read live via AtomicU64 — not snapshotted at boot.
    server_settings: std::sync::Arc<crate::server_settings::ServerSettings>,
    // §3.5 active connection counter — drives the idle-shutdown timer
    active_connections: std::sync::Arc<AtomicUsize>,
}
impl Server {
    async fn run(self, listener: LocalSocketListener) -> Result<()> {
        // §16.11 Hot-reload server.json → scrollback capacity on live panes.
        crate::server_settings::spawn_hot_reload(
            self.server_settings.clone(),
            self.sessions.clone(),
        );

        use interprocess::local_socket::tokio::prelude::*;

        // §3.5 keep_alive: when active_connections is 0 and keep_alive_seconds > 0,
        // the daemon exits after keep_alive_seconds of idleness. A mpsc channel lets
        // spawned connection tasks notify the accept loop when they finish, so the
        // loop can re-evaluate the idle timer without polling.
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let shutdown_state = std::sync::Arc::new(ShutdownState {
            requested: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ack_request_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            acked: std::sync::Arc::new(tokio::sync::Notify::new()),
        });

        // The idle deadline, if a timer is currently armed. `None` means no timer
        // is running (either because a connection is active or keep_alive is disabled).
        let mut idle_deadline: Option<tokio::time::Instant> = None;

        loop {
            // (Re-)arm the idle timer when the connection count transitions to zero.
            // Read keep_alive live so a hot-reloaded server.json value applies.
            let keep_alive_seconds = self.server_settings.keep_alive_seconds();
            let current = self.active_connections.load(Ordering::SeqCst);
            if current == 0 && keep_alive_seconds > 0 && idle_deadline.is_none() {
                idle_deadline =
                    Some(tokio::time::Instant::now() + Duration::from_secs(keep_alive_seconds));
            }
            // Cancel the timer when connections become non-zero.
            if current > 0 {
                idle_deadline = None;
            }

            tokio::select! {
                accept_result = listener.accept() => {
                    let stream = match accept_result {
                        Ok(s) => s,
                        Err(e) => {
                            zlog::error!("accept failed: {}", e);
                            // Keep the loop alive on transient accept errors.
                            continue;
                        }
                    };
                    // New connection cancels the idle timer.
                    idle_deadline = None;
                    let prev = self.active_connections.fetch_add(1, Ordering::SeqCst);
                    zlog::info!("client connected (active={})", prev + 1);

                    let sessions = self.sessions.clone();
                    let db = self._db.clone();
                    let clipboard = self.clipboard.clone();
                    // §16.11 thread the live ServerSettings handle so new panes
                    // honor env + server.json scrollback (hot-reloaded) at spawn.
                    let server_settings = self.server_settings.clone();
                    let extension_host = self.extension_host.clone();
                    let counter = self.active_connections.clone();
                    let done_tx = done_tx.clone();
                    let shutdown_state = shutdown_state.clone();

                    tokio::spawn(async move {
                        match connection::handle_connection(stream, sessions, db, clipboard, server_settings, shutdown_state, extension_host).await {
                            Ok(()) => {
                                zlog::info!("client disconnected");
                            }
                            Err(e) => {
                                zlog::error!("connection error: {}", e);
                            }
                        }
                        // §3.5 decrement counter and notify the accept loop so it
                        // can re-evaluate the idle timer.
                        let remaining = counter.fetch_sub(1, Ordering::SeqCst) - 1;
                        let _ = done_tx.send(());
                        zlog::info!("active connections={}", remaining);
                    });
                }

                // Notify that a connection task finished — re-evaluate the idle timer.
                _ = done_rx.recv() => {
                    // Loop will re-arm idle_deadline if connections dropped to 0.
                }

                // §3.5 Explicit Shutdown RPC acknowledged and flushed by a connection.
                _ = shutdown_state.acked.notified() => {
                    tracing::info!("explicit mux shutdown acknowledged, terminating");
                    return Ok(());
                }

                // Idle timer expired — graceful shutdown.
                _ = idle_sleep(idle_deadline) => {
                    let keep_alive_seconds = self.server_settings.keep_alive_seconds();
                    tracing::info!(
                        idle_seconds = keep_alive_seconds,
                        "daemon idle for {}s, shutting down",
                        keep_alive_seconds
                    );
                    return Ok(());
                }
            }
        }
    }
}

/// §3.5 Returns a boxed future that resolves at the idle deadline.
///
/// If no deadline is armed (`None`), returns a future that never resolves,
/// so it stays dormant inside `tokio::select!` until a connection event
/// wins the race. This avoids the need for conditional branching inside
/// the select! — the branch is always present but inert when disabled.
fn idle_sleep(deadline: Option<tokio::time::Instant>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    match deadline {
        // Never resolves — far-future deadline keeps the select! branch inert.
        None => Box::pin(tokio::time::sleep_until(
            tokio::time::Instant::now() + Duration::from_secs(86400 * 365 * 10),
        )),
        Some(d) => Box::pin(async move {
            tokio::time::sleep_until(d).await;
        }),
    }
}
