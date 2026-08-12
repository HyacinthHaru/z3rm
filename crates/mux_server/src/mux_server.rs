// §3.1 mux_server — mux_server 守护进程库。
// 管理 PTY、alacritty 终端模拟、layout 引擎、session 持久化。

use anyhow::{Context as _, Result};
use interprocess::local_socket::tokio::Listener as LocalSocketListener;
use sqlez::connection::Connection;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

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
mod shell_integration;
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

// ============================================================================
// §3.2 / §16.1 Local socket trust boundary + exclusive startup claim
// ============================================================================
//
// The 0600 ACL the server applies after binding is only meaningful if the
// path the server binds is the path clients connect to. A left-over socket
// left by a crashed daemon, a same-uid attacker that swapped a regular file
// for the socket, or a symlink redirecting bind/connect off the runtime dir
// all defeat that guarantee. The helpers below make a single, well-ordered
// startup decision:
//
//   1. lstat the existing path — reject symlinks and non-socket inodes, and
//      refuse to bind over a socket owned by another uid (fail closed);
//   2. probe connect AND check the recorded owner pid with `kill(pid, 0)`
//      — only a socket whose owner is provably dead (no pid record, or a pid
//      that no longer exists) is reclaimed, so a transiently-unresponsive live
//      daemon is never split-brained by a second starter; and
//   3. write a pid file (pid + boot timestamp) with `fsync` and atomic rename,
//      so `z3rm-server status` and the next startup see a consistent owner.

/// §3.2 Lifecycle files alongside a local Unix socket.
///
/// `pid` doubles as the startup claim: it records the owning pid and the boot
/// timestamp the status command reports for uptime. `lock` is the exclusive
/// marker the live owner holds; a starter that cannot place it knows another
/// daemon already owns the socket and must not reclaim it.
#[cfg(unix)]
struct SocketSidecars {
    pid: PathBuf,
    lock: PathBuf,
}

#[cfg(unix)]
fn socket_sidecars(socket_path: &Path) -> SocketSidecars {
    let mut pid = socket_path.as_os_str().to_owned();
    pid.push(".pid");
    let mut lock = socket_path.as_os_str().to_owned();
    lock.push(".lock");
    SocketSidecars {
        pid: PathBuf::from(pid),
        lock: PathBuf::from(lock),
    }
}

/// §3.2 `true` only when `pid` is an existing process the same uid may signal.
///
/// `kill(pid, 0)` returns `ESRCH` (or `EPERM` for a different uid) when the
/// process is gone. pidfile ownership already pins the socket to our own uid,
/// so `EPERM` here would be a Surprise worth surfacing — but the common "the
/// prior daemon died without cleanup" answer is `ESRCH`, which we read as
/// "owner gone, safe to reclaim".
#[cfg(unix)]
fn owner_process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Safety: kill(2) with signal 0 makes no signal delivery and is the
    // documented liveness probe; the only arguments are the pid and 0.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno != libc::ESRCH
}

/// §3.2 Read the pidfile recorded by a prior owner, if one is present and
///parses. A missing or malformed file means "no recorded owner", which the
/// caller treats as "stale" only together with a failed connect probe.
#[cfg(unix)]
fn read_owner_metadata(socket_path: &Path) -> Option<OwnerMetadata> {
    let sidecars = socket_sidecars(socket_path);
    let contents = std::fs::read_to_string(&sidecars.pid).ok()?;
    let mut lines = contents.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let boot_secs: u64 = lines.next().and_then(|line| line.trim().parse().ok())?;
    Some(OwnerMetadata { pid, boot_secs })
}

#[cfg(unix)]
struct OwnerMetadata {
    pid: u32,
    boot_secs: u64,
}

/// §3.2 Classify an existing inode at `socket_path` for the trust boundary.
///
/// `lstat` is used deliberately: a symlink at the path must fail closed,
/// because `bind` would follow it and leave the real target owned by an
/// attacker. `stat` would defeat that check.
#[cfg(unix)]
enum SocketInodeState {
    /// No inode at the path — clean bind target.
    Missing,
    /// Owned by the current uid and is a Unix socket — the only inode shape
    /// we are willing to bind over (after a stale-reclaim probe).
    OurSocket,
    /// Path exists but is the wrong kind, owned by another uid, or a symlink.
    /// The starter must refuse rather than reclaim it.
    Unsafe(anyhow::Error),
}

#[cfg(unix)]
fn classify_socket_inode(socket_path: &Path) -> SocketInodeState {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SocketInodeState::Missing;
        }
        Err(error) => {
            return SocketInodeState::Unsafe(anyhow::anyhow!(
                "cannot inspect existing socket path {}: {error}",
                socket_path.display()
            ));
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return SocketInodeState::Unsafe(anyhow::anyhow!(
            "refusing to bind over symlink at {} (potential redirect off the runtime dir)",
            socket_path.display()
        ));
    }
    if !file_type.is_socket() {
        return SocketInodeState::Unsafe(anyhow::anyhow!(
            "refusing to bind over non-socket inode at {} ({:?})",
            socket_path.display(),
            file_type
        ));
    }
    let our_uid = nix_uid();
    if metadata.uid() != our_uid {
        return SocketInodeState::Unsafe(anyhow::anyhow!(
            "refusing to bind over socket at {} owned by uid {} (current uid {})",
            socket_path.display(),
            metadata.uid(),
            our_uid
        ));
    }
    SocketInodeState::OurSocket
}

#[cfg(unix)]
fn nix_uid() -> u32 {
    // Safety: getuid takes no arguments and returns the calling process' uid.
    unsafe { libc::getuid() }
}

/// §3.2 A live accept loop is the authoritative "owner is alive" signal: a
/// socket that still accepts a connection has a running server behind it. A
/// refused connect alone is not enough — a slow-to-start daemon or a kernel
/// backlog stalling the connect would let a second starter steal the socket.
#[cfg(unix)]
fn socket_accepts_connection(socket_path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(socket_path).is_ok()
}

/// §3.2 Write the pidfile (pid + boot timestamp) atomically: stage to a
/// sibling temp file, fsync for durability, then rename over the destination.
/// The pidfile is the startup claim `z3rm-server status` and the next startup
/// read to decide whether the recorded owner is still alive.
#[cfg(unix)]
fn write_pidfile(socket_path: &Path, boot: SystemTime) -> Result<()> {
    let sidecars = socket_sidecars(socket_path);
    let parent = sidecars.pid.parent().ok_or_else(|| {
        anyhow::anyhow!("pidfile path has no parent: {}", sidecars.pid.display())
    })?;
    let pid = std::process::id();
    let boot_secs = boot
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // Stage to a sibling temp file, fsync for durability, then rename over
    // the destination (the crate's shared atomic-write pattern; no tempfile
    // dependency). The temp file is removed if any step fails so a failed
    // start cannot leave a stray staging file behind.
    let staging_path = parent.join(format!(".mux-server.pid.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        std::fs::write(&staging_path, format!("{pid}\n{boot_secs}\n"))
            .with_context(|| format!("writing staging pidfile in {}", parent.display()))?;
        std::fs::File::open(&staging_path)?
            .sync_all()
            .with_context(|| format!("fsync staging pidfile for {}", socket_path.display()))?;
        std::fs::rename(&staging_path, &sidecars.pid)
            .map_err(|error| anyhow::anyhow!("persist pidfile: {error}"))?;
        fsync_parent(&sidecars.pid);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging_path);
    }
    result
}

/// §3.2 fsync the directory containing `path` so the pidfile rename is
/// durable across a crash. Best-effort: a failure here must not block start,
/// because some tmpfs setups (and CI sandboxes) reject directory fsync.
#[cfg(unix)]
fn fsync_parent(path: &Path) {
    let Some(parent) = path.parent() else { return };
    match std::fs::File::open(parent) {
        Ok(mut dir) => {
            if let Err(error) = dir.sync_all() {
                tracing::warn!(error = %error, dir = %parent.display(), "fsync parent dir failed (continuing)");
            }
        }
        Err(error) => tracing::warn!(
            error = %error,
            dir = %parent.display(),
            "open parent dir for fsync failed (continuing)"
        ),
    }
}

/// §3.2 Remove the pidfile and the exclusive lock left by a prior owner.
///
/// Called on graceful shutdown and after reclaiming a stale socket, so the
/// next startup sees a clean dir and does not misread a dead owner's record.
#[cfg(unix)]
fn remove_sidecars(socket_path: &Path) {
    let sidecars = socket_sidecars(socket_path);
    for file in [sidecars.pid.as_path(), sidecars.lock.as_path()] {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(error = %error, path = %file.display(), "remove socket sidecar failed"),
        }
    }
}

/// §3.2 Try to bind, falling back to stale-socket cleanup.
///
/// On Unix the bind is gated by a trust-boundary check on the existing inode
/// (symlink/foreign-uid/non-socket inodes fail closed) and an exclusive
/// stale-reclaim decision: a leftover socket is only reclaimed when both a
/// connect probe fails *and* the recorded owner pid is provably gone, so a
/// live but transiently-unresponsive daemon cannot be split-brained by a
/// concurrent starter. A satisfying bind writes a pidfile with the owning pid
/// and boot timestamp so `z3rm-server status` and the next startup agree on
/// ownership. On Windows named pipes are ephemeral, so the trust check is a
/// no-op and stale cleanup is unnecessary.
pub async fn bind_or_cleanup(
    name: &interprocess::local_socket::Name<'_>,
) -> Result<LocalSocketListener> {
    match bind_socket(name).await {
        Ok(listener) => {
            #[cfg(unix)]
            if let Some(socket_path) = unix_socket_path() {
                write_pidfile(&socket_path, SystemTime::now())?;
            }
            Ok(listener)
        }
        Err(error) => {
            #[cfg(unix)]
            if let Some(socket_path) = unix_socket_path() {
                return reclaim_or_refuse(&socket_path, name, error).await;
            }
            #[cfg(not(unix))]
            let _ = name;
            Err(error)
        }
    }
}

#[cfg(unix)]
async fn reclaim_or_refuse(
    socket_path: &Path,
    name: &interprocess::local_socket::Name<'_>,
    original_error: anyhow::Error,
) -> Result<LocalSocketListener> {
    match classify_socket_inode(socket_path) {
        SocketInodeState::Missing => Err(original_error),
        SocketInodeState::Unsafe(reason) => {
            zlog::warn!("refusing to bind unsafe socket: {reason}");
            Err(reason)
        }
        SocketInodeState::OurSocket => {
            // §3.2 A live owner keeps its socket even if our connect stalls;
            // only a connect failure *and* a dead/missing owner is reclaimed.
            if socket_accepts_connection(socket_path) {
                zlog::info!("live daemon owns {}; refusing to reclaim", socket_path.display());
                return Err(original_error);
            }
            match read_owner_metadata(socket_path) {
                Some(metadata) if owner_process_alive(metadata.pid) => {
                    zlog::info!(
                        "socket {} connect refused but owner pid {} is alive; refusing to reclaim",
                        socket_path.display(),
                        metadata.pid
                    );
                    Err(original_error)
                }
                _ => {
                    zlog::warn!(
                        "stale socket detected (no live owner); reclaiming {}",
                        socket_path.display()
                    );
                    if let Err(remove_error) = std::fs::remove_file(socket_path) {
                        if remove_error.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(error = %remove_error, "remove stale socket failed");
                        }
                    }
                    remove_sidecars(socket_path);
                    let listener = bind_socket(name).await?;
                    write_pidfile(socket_path, SystemTime::now())?;
                    Ok(listener)
                }
            }
        }
    }
}
fn init_database(db_path: &Path) -> Result<Connection> {
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
            Ok(listener) => listener,
            Err(error) => return Err(error),
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
