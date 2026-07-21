// §3.1 mux_server — mux_server 守护进程库。
// 管理 PTY、alacritty 终端模拟、layout 引擎、session 持久化。

use anyhow::Result;
use sqlez::connection::Connection;
use std::path::PathBuf;
use std::time::SystemTime;
use interprocess::local_socket::tokio::Listener as LocalSocketListener;

pub mod connection;
pub mod clipboard;
pub mod grid_sync;
pub mod coalescing;
pub mod dec2026;
pub mod layout;
pub mod pane;
pub mod persistence;

#[cfg(test)]
mod tests;
pub mod session;


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
static LOG_FILE_PATH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
    get_log_dir().join("mux-server.log")
});

/// §16.12 日志轮转路径 (旧文件)
static LOG_FILE_ROTATE: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
    get_log_dir().join("mux-server.log.old")
});

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

    zlog::info!("mux_server logging initialized, log_dir={}", log_dir.display());
    Ok(())
}

/// 默认 socket 路径: $XDG_RUNTIME_DIR/z3rm/mux.sock (Unix §16.1)
/// 或 \\.\pipe\z3rm-mux (Windows)
fn default_socket_name() -> interprocess::local_socket::Name<'static> {
    use interprocess::local_socket::{prelude::*, GenericFilePath, GenericNamespaced};
    if let Ok(p) = std::env::var("Z3RM_MUX_SOCKET") {
        return p
            .to_fs_name::<GenericFilePath>()
            .expect("invalid socket path");
    }
    #[cfg(unix)]
    {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let path = std::path::PathBuf::from(runtime_dir).join("z3rm").join("mux.sock");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        path.to_string_lossy()
            .to_string()
            .to_fs_name::<GenericFilePath>()
            .expect("invalid socket path")
    }
    #[cfg(windows)]
    {
        r"\\.\pipe\z3rm-mux"
            .to_ns_name::<GenericNamespaced>()
            .expect("invalid pipe name")
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
    Some(std::path::PathBuf::from(runtime_dir).join("z3rm").join("mux.sock"))
}

async fn bind_socket(name: &interprocess::local_socket::Name<'_>) -> Result<LocalSocketListener> {
    use interprocess::local_socket::tokio::prelude::*;
    let listener = LocalSocketListener::from_options(
        interprocess::local_socket::ListenerOptions::new().name(name.borrow()),
    )?;
    Ok(listener)
}

/// §3.6 初始化数据库连接
fn init_database(db_path: &PathBuf) -> Result<Connection> {
    let db = Connection::open_file(db_path.to_str().unwrap_or("file::memory:?mode=memory"));
    // §3.6 初始化持久化表
    persistence::init_tables(&db)?;
    Ok(db)
}

/// 启动守护进程 (§3.1)
pub fn run() -> Result<()> {
    // §16.12 初始化日志系统
    setup_logging()?;

    // 创建 Tokio runtime，所有异步操作都在其上下文中执行
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let socket_name = default_socket_name();
        let listener = match bind_socket(&socket_name).await {
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
                let _ = std::fs::create_dir_all(&parent);
                parent.join("mux.db")
            }
            #[cfg(windows)]
            {
                let base = dirs::data_local_dir().unwrap_or_else(|| {
                    PathBuf::from(std::env::var("TEMP").unwrap_or_else(|_| "C:/Temp".to_string()))
                });
                let dir = base.join("z3rm");
                let _ = std::fs::create_dir_all(&dir);
                dir.join("mux.db")
            }
        };
        let db = init_database(&db_path)?;

        let recovered = persistence::recover_sessions(&db)?;
        tracing::info!(count = recovered.len(), "recovered sessions");

        let sessions = std::sync::Arc::new(parking_lot::RwLock::new(recovered));
        let db = std::sync::Arc::new(parking_lot::Mutex::new(db));

        let sessions_clone = sessions.clone();
        let db_clone = db.clone();
        let persist_handle = tokio::spawn(async move {
            persistence::persist_loop(sessions_clone, db_clone).await;
        });

        let clipboard = std::sync::Arc::new(clipboard::ServerClipboard::new());
        let server = Server {
            sessions,
            _db: db,
            _persist_handle: Some(persist_handle),
            clipboard,
            start_time: SystemTime::now(),
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
    // §16.12 启动时间 (用于 status 计算运行时长)
    start_time: SystemTime,
}

impl Server {
    async fn run(self, listener: LocalSocketListener) -> Result<()> {
        use interprocess::local_socket::tokio::prelude::*;
        loop {
            let stream = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    zlog::error!("accept failed: {}", e);
                    continue;
                }
            };
            zlog::info!("client connected");

            let sessions = self.sessions.clone();
            let db = self._db.clone();
            let clipboard = self.clipboard.clone();

            tokio::spawn(async move {
                match connection::handle_connection(stream, sessions, db, clipboard).await {
                    Ok(()) => {
                        zlog::info!("client disconnected");
                    }
                    Err(e) => {
                        zlog::error!("connection error: {}", e);
                    }
                }
            });
        }
    }
}
