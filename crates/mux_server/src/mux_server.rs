// §3.1 mux_server — mux_server 守护进程库。
// 管理 PTY、alacritty 终端模拟、layout 引擎、session 持久化。

use anyhow::Result;
use sqlez::connection::Connection;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::net::UnixListener;

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

/// 默认 socket 路径: $XDG_RUNTIME_DIR/z3rm/mux.sock (§16.1)
///
/// 测试与多实例场景可用 Z3RM_MUX_SOCKET 环境变量覆盖。
fn default_socket_path() -> PathBuf {
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

/// 绑定本地 socket (§9)
async fn bind_socket(path: &PathBuf) -> Result<UnixListener> {
    // 确保父目录存在
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 删除可能存在的旧 socket
    let _ = std::fs::remove_file(path);

    // 创建 socket 文件
    let listener = UnixListener::bind(path)?;

    // 设置 0600 权限 (§9)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }

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
        let socket_path = default_socket_path();
        let listener = match bind_socket(&socket_path).await {
            Ok(l) => l,
            Err(e) => {
                zlog::error!("socket bind failed: path={} error={}", socket_path.display(), e);
                return Err(e);
            }
        };
        let addr = listener.local_addr()?;
        zlog::info!("mux_server listening: socket={:?}", addr);

        let db_path = if let Ok(p) = std::env::var("Z3RM_MUX_DB") {
            PathBuf::from(p)
        } else {
            let dir = dirs::runtime_dir()
                .or_else(|| Some(std::env::temp_dir().join("z3rm")))
                .unwrap_or_else(|| PathBuf::from("/tmp/z3rm"));
            std::fs::create_dir_all(&dir)?;
            dir.join("z3rm.db")
        };
        let db = init_database(&db_path)?;

        // §3.6 启动时恢复 session
        let recovered = persistence::recover_sessions(&db)?;
        tracing::info!(count = recovered.len(), "recovered sessions");

        let sessions = std::sync::Arc::new(parking_lot::RwLock::new(recovered));
        let db = std::sync::Arc::new(parking_lot::Mutex::new(db));

        // §3.6 启动持久化后台任务 (每 10s 快照)
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
    /// §3.5 keep_alive=true: 守护进程保持运行直到显式关闭
    /// §9 监听连接并处理请求
    async fn run(self, listener: UnixListener) -> Result<()> {
        loop {
            let (stream, addr) = listener.accept().await?;
            // §16.12 记录客户端连接事件
            zlog::info!("client connected");

            let sessions = self.sessions.clone();
            let db = self._db.clone();
            let clipboard = self.clipboard.clone();

            tokio::spawn(async move {
                match connection::handle_connection(stream, sessions, db, clipboard).await {
                    Ok(()) => {
                        // §16.12 记录客户端断开
                        zlog::info!("client disconnected");
                    }
                    Err(e) => {
                        // §16.12 记录连接错误
                        zlog::error!("connection error: {}", e);
                    }
                }
            });
        }
    }
}
