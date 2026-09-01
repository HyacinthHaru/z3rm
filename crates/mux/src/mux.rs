//! # mux
//!
//! z3rm mux client crate: connects to mux_server via local socket (or SSH),
//! sends RPC requests, receives notifications, and provides grid sync.
//!
//! 协议版本化（§3.10），基于长度前缀的二进制帧（§9），
//! 请求/响应关联通过 request_id（§9）。

use anyhow::{Context as _, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
const WRITE_QUEUE_CAPACITY: usize = 1024;
/// §3.4 Ordinary notification queue bound per subscriber. A dropped `PaneDirty`
/// is coalesced into a per-pane latch instead of buffered, so the queue never
/// needs to grow for lost dirty signals.
const NOTIFICATION_QUEUE_CAPACITY: usize = 4096;

// §9 从 mux_protocol 导入所有 protobuf 类型。
use mux_protocol::{
    AttachResponse, ClipboardEntry, DeclineFileVersionResponse, Envelope, FetchGridUpdateResponse,
    FetchScrollbackResponse, GetFileVersionResponse, ListChangedFilesResponse,
    ListCommandsResponse, ListDirResponse, ListFileVersionsResponse, Notification,
    PROTOCOL_VERSION, ReadFileResponse, Request, Response, SearchScrollbackResponse, SessionInfo,
    SessionLayoutChanged, ShellCommand, ShellIntegrationResponse, StatFileResponse, TerminalSize,
    attach_request::AttachMode as AttachMode_, check_frame_len,
    envelope::Payload as EnvelopePayload, frame, notification::Event as NotifEvent,
    parse_len_prefix, request::Body as RequestBody, response::Body as ResponseBody,
    split_node::SplitDirection,
};

// §16.6 SSH 远程连接模块（Plan 19）。
#[cfg(all(feature = "ssh", not(target_family = "wasm")))]
mod remote_install;
pub mod command_history;
pub mod scrollback_search;
#[cfg(all(feature = "ssh", not(target_family = "wasm")))]
mod ssh;
mod sync;
#[cfg(target_family = "wasm")]
mod web_io;

#[cfg(all(feature = "ssh", not(target_family = "wasm")))]
pub use remote_install::{auto_install_server, ensure_remote_server};
#[cfg(all(feature = "ssh", not(target_family = "wasm")))]
pub use ssh::{SshConnectionOptions, SshSession, connect_ssh};
pub use sync::sync_extensions_to_remote;

// §9 公共类型导出
pub use mux_protocol::attach_request::AttachMode;
// ============================================================================
// §9 MuxDomain: mux client 核心结构体
// ============================================================================

/// Mux 客户端域：连接到 mux_server，发送 RPC 请求，接收通知。
///
/// §3.3 一个 `MuxDomain` 就是一个 GUI 窗口 (Plan 32): 连接 / 客户端身份 /
/// 窗口三者一一对应, 所以窗口关闭 = socket 关闭 = 服务端精确释放这一个窗口。
pub struct MuxDomain {
    inner: Arc<parking_lot::RwLock<DomainInner>>,
    /// §3.3 本连接代表的窗口 ID (多窗口支持，Plan 32)。
    ///
    /// 连接时先本地铸一个唯一 ID (与服务端同为 `win-{pid}-{nanoid}` 格式), 供
    /// 没有走 `NewWindow` 的对端使用; GUI 打开窗口时会用 `create_window` 换成
    /// 服务端分配的权威 ID。
    window_id: parking_lot::RwLock<String>,
    /// §15.4 The local socket selected for the next in-place reconnect.
    local_socket_path: parking_lot::RwLock<Option<PathBuf>>,
    /// §15.4 Last authoritative snapshot returned by attach/reconnect.
    last_attached_snapshot: parking_lot::RwLock<Option<mux_protocol::SessionSnapshot>>,
    /// §15.7 Last session successfully attached by this domain. Used by native
    /// KillSession keybindings so the GUI targets the attached session rather
    /// than an arbitrary `list_sessions().first()`.
    last_attached_session_id: parking_lot::RwLock<Option<String>>,
    /// wasm single-process transport driver. Native transports run the I/O
    /// worker on a dedicated thread; the wasm build pumps the in-memory
    /// stream from the browser event loop instead.
    #[cfg(target_family = "wasm")]
    wasm_io: parking_lot::Mutex<Option<Arc<web_io::WasmIoShared>>>,
}

impl Drop for MuxDomain {
    /// §3.3 Retiring the transport epoch is what actually closes the socket.
    ///
    /// The I/O thread holds its own `Arc` to `DomainInner`, which owns
    /// `write_tx`, so dropping the last `MuxDomain` handle alone would leave the
    /// channel connected and the thread spinning forever — the daemon would
    /// never see the connection close. Plan 32 relies on that close: it is the
    /// signal that releases the window from the session when a GUI window goes
    /// away without a clean detach.
    fn drop(&mut self) {
        self.inner
            .read()
            .transport_epoch
            .fetch_add(1, Ordering::SeqCst);
    }
}

/// §3.3 铸一个进程内唯一的窗口 ID (Plan 32)。
///
/// 旧实现用 `win-{pid}`, 同一个进程的每个窗口都会撞成同一个 ID, 服务端根本
/// 分不出是哪个窗口在 attach。格式与服务端 `handle_new_window` 保持一致。
#[cfg(not(target_family = "wasm"))]
fn mint_window_id() -> String {
    format!("win-{}-{}", std::process::id(), nanoid::nanoid!())
}
/// wasm has no process id; the browser tab hosts exactly one client process,
/// so a nanoid suffix keeps the window id unique.
#[cfg(target_family = "wasm")]
fn mint_window_id() -> String {
    format!("win-wasm-{}", nanoid::nanoid!())
}
/// §9 内部状态：请求 ID 计数器、待处理请求、订阅者列表、写通道。
pub(crate) struct DomainInner {
    next_request_id: AtomicU64,
    pending_requests: HashMap<u64, PendingRequest>,
    /// §9 通知订阅者列表。subscribe() 添加新记录, 路由器 fan-out 到所有。
    subscribers: Arc<parking_lot::Mutex<Vec<SubscriberSender>>>,
    write_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// Monotonic transport epoch. Each I/O worker records the epoch it was
    /// spawned with; reconnect increments it so a stale worker can no longer
    /// fulfill or cancel requests registered against the new transport.
    transport_epoch: AtomicU64,
}

/// A pending request remembers the transport epoch it was registered against,
/// so the router only fulfills it with a response from the same epoch and a
/// draining stale worker cannot close a sender owned by the new transport.
pub(crate) struct PendingRequest {
    sender: async_channel::Sender<Response>,
    transport_epoch: u64,
}

struct PendingRequestGuard {
    inner: Arc<parking_lot::RwLock<DomainInner>>,
    request_id: u64,
    transport_epoch: u64,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        let mut inner = self.inner.write();
        let belongs_to_guard = inner
            .pending_requests
            .get(&self.request_id)
            .is_some_and(|request| request.transport_epoch == self.transport_epoch);
        if belongs_to_guard {
            inner.pending_requests.remove(&self.request_id);
        }
    }
}

pub(crate) fn take_pending_response_sender(
    inner: &mut DomainInner,
    request_id: u64,
    worker_epoch: u64,
) -> Option<async_channel::Sender<Response>> {
    let belongs_to_worker = inner
        .pending_requests
        .get(&request_id)
        .is_some_and(|request| request.transport_epoch == worker_epoch);
    belongs_to_worker
        .then(|| inner.pending_requests.remove(&request_id))
        .flatten()
        .map(|request| request.sender)
}

pub(crate) fn drain_pending_requests_for_epoch(
    inner: &mut DomainInner,
    worker_epoch: u64,
) -> Vec<async_channel::Sender<Response>> {
    let pending = std::mem::take(&mut inner.pending_requests);
    let mut worker_senders = Vec::new();
    for (request_id, request) in pending {
        if request.transport_epoch == worker_epoch {
            worker_senders.push(request.sender);
        } else {
            inner.pending_requests.insert(request_id, request);
        }
    }
    worker_senders
}
/// Router-side half of one notification subscriber: the bounded ordinary queue
/// sender plus the per-pane dirty latch shared with the subscriber's
/// `NotificationReceiver`. A full-queue `PaneDirty` is recorded in the latch
/// instead of dropped, and the receiver turns each latched pane back into
/// exactly one `PaneDirty` once its queue drains — the latch bounds loss to
/// one entry per pane instead of unbounded buffering.
#[derive(Clone)]
pub struct SubscriberSender {
    /// The bounded ordinary notification queue.
    pub sender: async_channel::Sender<Notification>,
    /// Panes whose `PaneDirty` could not be enqueued because the queue was full.
    pub dirty_latches: Arc<parking_lot::Mutex<HashSet<String>>>,
    /// wasm-only overflow for reliable notifications. The browser is
    /// single-threaded, so the native "block the I/O thread" backpressure
    /// would deadlock; instead reliable events queue here (unbounded but
    /// bounded in practice by pane count) and the receiver drains them in
    /// FIFO order after the channel.
    #[cfg(target_family = "wasm")]
    pub reliable_overflow: Arc<parking_lot::Mutex<VecDeque<Notification>>>,
}

/// Drop subscriber records whose queue sender has been closed, so a dropped
/// receiver stops consuming router work (and its latch stops growing).
pub(crate) fn prune_closed_subscribers(subscribers: &mut Vec<SubscriberSender>) {
    subscribers.retain(|subscriber| !subscriber.sender.is_closed());
}

/// Fan one notification out to every live subscriber.
///
/// Reliable lifecycle and typed media/action notifications block the native
/// I/O thread rather than drop; on wasm they move to the subscriber's FIFO
/// overflow queue. Supplemental `PaneOutputChunk` drops on a full queue, and
/// `PaneDirty` records a per-pane latch for later synthesis.
pub(crate) fn fan_out_notification(subscribers: &[SubscriberSender], notification: &Notification) {
    let reliable = notification_requires_reliable_delivery(notification);
    for subscriber in subscribers {
        if reliable {
            // §3.1 / §3.4 reliable path: block the dedicated I/O
            // thread instead of dropping lifecycle state or PTY bytes.
            #[cfg(not(target_family = "wasm"))]
            if let Err(error) = subscriber.sender.send_blocking(notification.clone()) {
                tracing::debug!(
                    ?error,
                    "reliable notification subscriber closed before delivery"
                );
            }
            // wasm is single-threaded: blocking the only thread would
            // deadlock. Overflow keeps at-least-once semantics and global
            // FIFO (the receiver drains the channel first, then overflow).
            #[cfg(target_family = "wasm")]
            {
                let mut overflow = subscriber.reliable_overflow.lock();
                if overflow.is_empty() {
                    match subscriber.sender.try_send(notification.clone()) {
                        Ok(()) => continue,
                        Err(async_channel::TrySendError::Full(_)) => {
                            overflow.push_back(notification.clone());
                        }
                        Err(async_channel::TrySendError::Closed(_)) => {
                            tracing::debug!(
                                "reliable notification subscriber closed before delivery"
                            );
                        }
                    }
                } else {
                    overflow.push_back(notification.clone());
                }
            }
        } else if let Err(error) = subscriber.sender.try_send(notification.clone()) {
            match error {
                async_channel::TrySendError::Full(_) => match notification.event.as_ref() {
                    Some(NotifEvent::PaneDirty(dirty)) => {
                        subscriber.dirty_latches.lock().insert(dirty.pane_id.clone());
                        tracing::trace!(
                            pane_id = %dirty.pane_id,
                            "full-queue PaneDirty latched for the subscriber to synthesize"
                        );
                    }
                    _ => {
                        tracing::trace!("lossy notification subscriber queue full; dropping");
                    }
                },
                async_channel::TrySendError::Closed(_) => {
                    tracing::debug!("lossy notification subscriber closed before delivery");
                }
            }
        }
    }
}
/// Notifications that must never be dropped by the router fan-out.
///
/// Lifecycle events (§3.4) and typed media/actions are at-least-once: losing a
/// `PaneRemoved` leaves a zombie pane, while losing a download/copy or media
/// event changes user-visible behavior. Native transports apply backpressure;
/// wasm uses the subscriber's reliable overflow queue because blocking the
/// browser's only thread would deadlock it.
fn notification_requires_reliable_delivery(notification: &Notification) -> bool {
    matches!(
        notification.event,
        Some(NotifEvent::PaneAdded(_))
            | Some(NotifEvent::PaneRemoved(_))
            | Some(NotifEvent::SessionLayoutChanged(_))
            | Some(NotifEvent::PaneZoomed(_))
            | Some(NotifEvent::PaneTitleChanged(_))
            | Some(NotifEvent::PaneBell(_))
            | Some(NotifEvent::PaneMedia(_))
            | Some(NotifEvent::PaneAction(_))
            | Some(NotifEvent::ExtensionChrome(_))
            | Some(NotifEvent::WindowAdded(_))
            | Some(NotifEvent::WindowRemoved(_))
    )
}

// §9 MuxTransport: 传输层枚举
// ============================================================================

/// §3.2 传输层枚举：本地 Unix socket 或 SSH 隧道。
pub enum MuxTransport {
    /// §3.2 本地 Unix socket 连接。
    Local,
    /// §3.2 SSH 隧道连接 (远程 mux_server)。
    #[cfg(feature = "ssh")]
    Ssh(SshSession),
}
// ============================================================================
// §9 connect_local: 建立本地 socket 连接
// ============================================================================
/// §9 连接到本地 mux_server。
/// §15.3 使用 interprocess crate 的 local socket 抽象:
/// Unix → Unix domain socket, Windows → named pipe。
#[cfg(not(target_family = "wasm"))]
async fn run_blocking_operation<T>(
    thread_name: &'static str,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let result = operation();
            if sender.send_blocking(result).is_err() {
                tracing::debug!(
                    thread_name,
                    "blocking operation caller dropped before result"
                );
            }
        })
        .map_err(|error| anyhow::anyhow!("failed to spawn {thread_name} thread: {error}"))?;
    receiver
        .recv()
        .await
        .map_err(|_| anyhow::anyhow!("{thread_name} thread exited without returning a result"))?
}

#[cfg(not(target_family = "wasm"))]
fn connect_local_blocking(path: &Path) -> Result<MuxDomain> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(path)?;
        stream.set_nonblocking(true)?;
        let domain = MuxDomain::connect_with_blocking_stream(stream)?;
        *domain.local_socket_path.write() = Some(path.to_path_buf());
        Ok(domain)
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{
            GenericNamespaced, Stream as LocalSocketStream, prelude::*,
        };
        let pipe_name = path.to_string_lossy().to_string();
        let name = pipe_name
            .to_ns_name::<GenericNamespaced>()
            .map_err(|error| anyhow::anyhow!("invalid pipe name: {error}"))?;
        let stream = LocalSocketStream::connect(name)
            .map_err(|error| anyhow::anyhow!("connect failed: {error}"))?;
        let domain = MuxDomain::connect_with_stream(stream)?;
        *domain.local_socket_path.write() = Some(path.to_path_buf());
        Ok(domain)
    }
}

#[cfg(not(target_family = "wasm"))]
async fn connect_local_once(path: &Path) -> Result<MuxDomain> {
    let path = path.to_path_buf();
    run_blocking_operation("mux-connect", move || connect_local_blocking(&path)).await
}

#[cfg(not(target_family = "wasm"))]
pub async fn connect_local(socket_path: Option<&Path>) -> Result<MuxDomain> {
    let path = socket_path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_socket_path);

    match connect_local_once(&path).await {
        Ok(domain) => Ok(domain),
        Err(error) => {
            let message = error.to_string();
            #[cfg(unix)]
            if message.contains("111") || message.contains("Connection refused") {
                tracing::warn!(path = %path.display(), "stale socket, cleaning and retrying");
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(remove_error) => {
                        return Err(anyhow::anyhow!(
                            "failed to remove stale mux socket at {}: {}",
                            path.display(),
                            remove_error
                        ));
                    }
                }
                return connect_local_once(&path).await.with_context(|| {
                    format!(
                        "connect_local retry failed after cleaning stale socket at {}",
                        path.display()
                    )
                });
            }
            Err(anyhow::anyhow!("connect failed: {message}"))
        }
    }
}
/// Local sockets do not exist in the browser; the wasm client connects to an
/// in-process mux_server over an in-memory stream instead.
#[cfg(target_family = "wasm")]
pub async fn connect_local(_socket_path: Option<&Path>) -> Result<MuxDomain> {
    Err(anyhow::anyhow!(
        "local mux sockets are unavailable on wasm; use MuxDomain::connect_in_memory"
    ))
}

/// Race a response channel against a timeout without tying the caller to a
/// specific executor (GPUI's executor has no Tokio reactor; wasm has no smol).
#[cfg(not(target_family = "wasm"))]
async fn race_recv_with_timeout(
    rx: async_channel::Receiver<Response>,
    timeout: Duration,
) -> Option<std::result::Result<Response, async_channel::RecvError>> {
    smol::future::or(async { Some(rx.recv().await) }, async {
        smol::Timer::after(timeout).await;
        None
    })
    .await
}

/// Race a response channel against a timeout without tying the caller to a
/// specific executor (GPUI's executor has no Tokio reactor; wasm has no smol).
#[cfg(target_family = "wasm")]
async fn race_recv_with_timeout(
    rx: async_channel::Receiver<Response>,
    timeout: Duration,
) -> Option<std::result::Result<Response, async_channel::RecvError>> {
    use std::future::poll_fn;
    use std::task::Poll;
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let mut receive = std::pin::pin!(rx.recv());
    let mut timer = std::pin::pin!(gloo_timers::future::TimeoutFuture::new(millis));
    poll_fn(|cx| {
        if let Poll::Ready(value) = receive.as_mut().poll(cx) {
            return Poll::Ready(Some(value));
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

/// §16.1 默认 socket 路径 (与 mux_server 对齐)。
#[cfg(not(target_family = "wasm"))]
fn default_socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("Z3RM_MUX_SOCKET") {
        return std::path::PathBuf::from(p);
    }
    #[cfg(unix)]
    {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(runtime_dir)
            .join("z3rm")
            .join("mux.sock")
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(r"\\.\pipe\z3rm-mux")
    }
}

trait ReadWrite: std::io::Read + std::io::Write {}
impl<T: std::io::Read + std::io::Write> ReadWrite for T {}

/// §15.4 Open a fresh local socket and return the live byte stream, without
/// spawning the I/O thread. Used by the in-place reconnect methods so the new
/// I/O thread can be bound to an existing `Arc<RwLock<DomainInner>>` rather
/// than a freshly-created one. Mirrors the stale-socket retry that
/// `connect_local` performs.
#[cfg(not(target_family = "wasm"))]
fn connect_local_stream(socket_path: Option<&Path>) -> Result<Box<dyn ReadWrite + Send>> {
    let path = match socket_path {
        Some(p) => p.to_path_buf(),
        None => default_socket_path(),
    };

    #[cfg(unix)]
    fn open(path: &std::path::Path) -> Result<Box<dyn ReadWrite + Send>> {
        use std::os::unix::net::UnixStream;
        let connect = || -> Result<UnixStream> {
            let stream =
                UnixStream::connect(path).map_err(|e| anyhow::anyhow!("connect failed: {}", e))?;
            stream
                .set_nonblocking(true)
                .map_err(|e| anyhow::anyhow!("set_nonblocking failed: {}", e))?;
            Ok(stream)
        };
        match connect() {
            Ok(stream) => Ok(Box::new(stream)),
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("111") || msg.contains("Connection refused") {
                    tracing::warn!(path = %path.display(), "stale socket (111), cleaning and retrying");
                    if let Err(e) = std::fs::remove_file(path) {
                        tracing::warn!(error = %e, "remove_file failed");
                    }
                    let stream = connect()?;
                    Ok(Box::new(stream))
                } else {
                    Err(e)
                }
            }
        }
    }
    #[cfg(not(unix))]
    fn open(path: &std::path::Path) -> Result<Box<dyn ReadWrite + Send>> {
        use interprocess::local_socket::{
            GenericNamespaced, Stream as LocalSocketStream, prelude::*,
        };
        let pipe_name = path.to_string_lossy().to_string();
        let name = pipe_name
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| anyhow::anyhow!("invalid pipe name: {}", e))?;
        let stream = LocalSocketStream::connect(name)
            .map_err(|e| anyhow::anyhow!("connect failed: {}", e))?;
        Ok(Box::new(stream))
    }

    open(&path)
}

// ============================================================================
// §9 MuxDomain 实现
// ============================================================================

impl MuxDomain {
    #[cfg(not(target_family = "wasm"))]
    pub fn connect_with_stream(stream: interprocess::local_socket::Stream) -> Result<Self> {
        let (write_tx, write_rx) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);

        let subscribers: Arc<parking_lot::Mutex<Vec<SubscriberSender>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        let inner = Arc::new(parking_lot::RwLock::new(DomainInner {
            next_request_id: AtomicU64::new(1),
            pending_requests: HashMap::new(),
            subscribers: subscribers.clone(),
            write_tx,
            transport_epoch: AtomicU64::new(0),
        }));

        let io_inner = inner.clone();
        let io_subscribers = subscribers.clone();
        let io_epoch = 0;
        std::thread::Builder::new()
            .name("mux-io".into())
            .spawn(move || {
                Self::io_and_router_loop(stream, write_rx, io_inner, io_subscribers, io_epoch);
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn mux I/O thread: {}", e))?;

        Ok(MuxDomain {
            inner,
            window_id: parking_lot::RwLock::new(mint_window_id()),
            local_socket_path: parking_lot::RwLock::new(None),
            last_attached_snapshot: parking_lot::RwLock::new(None),
            last_attached_session_id: parking_lot::RwLock::new(None),
        })
    }

    /// Connect using any blocking Read+Write stream (e.g., UnixStream with non-blocking set).
    #[cfg(not(target_family = "wasm"))]
    pub fn connect_with_blocking_stream<S: std::io::Read + std::io::Write + Send + 'static>(
        stream: S,
    ) -> Result<Self> {
        let (write_tx, write_rx) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let subscribers: Arc<parking_lot::Mutex<Vec<SubscriberSender>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let inner = Arc::new(parking_lot::RwLock::new(DomainInner {
            next_request_id: AtomicU64::new(1),
            pending_requests: HashMap::new(),
            subscribers: subscribers.clone(),
            write_tx,
            transport_epoch: AtomicU64::new(0),
        }));
        let io_inner = inner.clone();
        let io_subscribers = subscribers.clone();
        let io_epoch = 0;
        std::thread::Builder::new()
            .name("mux-io".into())
            .spawn(move || {
                Self::io_and_router_loop(stream, write_rx, io_inner, io_subscribers, io_epoch);
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn mux I/O thread: {}", e))?;
        Ok(MuxDomain {
            inner,
            window_id: parking_lot::RwLock::new(mint_window_id()),
            local_socket_path: parking_lot::RwLock::new(None),
            last_attached_snapshot: parking_lot::RwLock::new(None),
            last_attached_session_id: parking_lot::RwLock::new(None),
        })
    }

    pub async fn connect(transport: MuxTransport) -> Result<Self> {
        match transport {
            MuxTransport::Local => connect_local(None).await,
            #[cfg(feature = "ssh")]
            MuxTransport::Ssh(_) => Err(anyhow::anyhow!(
                "SSH transport requires connect_ssh() to manage the SshSession lifecycle"
            )),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn io_and_router_loop<S: std::io::Read + std::io::Write + Send + 'static>(
        mut stream: S,
        write_rx: std::sync::mpsc::Receiver<Vec<u8>>,
        inner: Arc<parking_lot::RwLock<DomainInner>>,
        subscribers: Arc<parking_lot::Mutex<Vec<SubscriberSender>>>,
        worker_epoch: u64,
    ) {
        let mut buf = Vec::new();
        let mut pending_writes: VecDeque<(Vec<u8>, usize)> = VecDeque::new();

        'outer: loop {
            if inner.read().transport_epoch.load(Ordering::SeqCst) != worker_epoch {
                break;
            }
            // Drain the request channel into a partial-write queue. Local sockets
            // are nonblocking; `write_all` would treat a full kernel buffer as a
            // fatal disconnect and silently lose the frame.
            while let Ok(framed) = write_rx.try_recv() {
                pending_writes.push_back((framed, 0));
            }

            loop {
                let finished = match pending_writes.front_mut() {
                    None => break,
                    Some((frame, offset)) if *offset == frame.len() => true,
                    Some((frame, offset)) => match stream.write(&frame[*offset..]) {
                        Ok(0) => {
                            tracing::error!("socket write returned zero bytes");
                            break 'outer;
                        }
                        Ok(written) => {
                            *offset += written;
                            *offset == frame.len()
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => {
                            tracing::error!(error = %error, "socket write error");
                            break 'outer;
                        }
                    },
                };
                if finished {
                    pending_writes.pop_front();
                }
            }

            // §9 读取下一帧
            match Self::read_next_frame_generic(&mut stream, &mut buf) {
                Ok(Some(framed)) => {
                    let envelope = match mux_protocol::unframe(&framed) {
                        Ok((env, _)) => env,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to decode envelope");
                            break 'outer;
                        }
                    };
                    if inner.read().transport_epoch.load(Ordering::SeqCst) != worker_epoch {
                        break 'outer;
                    }

                    match envelope.payload {
                        Some(payload) => {
                            Self::route_envelope_payload(payload, &inner, &subscribers, worker_epoch)
                        }
                        None => {
                            tracing::warn!("envelope with no payload");
                        }
                    }
                }
                Ok(None) => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => {
                    tracing::error!(error = %e, "socket read error");
                    break 'outer;
                }
            }
        }
        // Close only requests registered on this transport. A stale worker may
        // exit after reconnect has already installed requests for a new epoch.
        let pending = drain_pending_requests_for_epoch(&mut inner.write(), worker_epoch);
        drop(pending);
    }

    /// Generic frame reader for any Read+Write stream.
    /// Route a decoded envelope: responses resolve their pending request,
    /// notifications fan out to subscribers. Shared by the native I/O thread
    /// and the wasm in-memory pump so both keep identical semantics.
    pub(crate) fn route_envelope_payload(
        payload: EnvelopePayload,
        inner: &Arc<parking_lot::RwLock<DomainInner>>,
        subscribers: &Arc<parking_lot::Mutex<Vec<SubscriberSender>>>,
        worker_epoch: u64,
    ) {
        match payload {
            EnvelopePayload::Response(resp) => {
                let request_id = resp.request_id;
                let sender =
                    take_pending_response_sender(&mut inner.write(), resp.request_id, worker_epoch);
                if let Some(sender) = sender
                    && sender.try_send(resp).is_err()
                {
                    tracing::debug!(
                        request_id,
                        "request future dropped before response delivery"
                    );
                }
            }
            EnvelopePayload::Notification(notif) => {
                // Clone the live records so a blocking reliable
                // delivery does not hold the subscriber lock.
                let senders = {
                    let mut subscribers = subscribers.lock();
                    prune_closed_subscribers(&mut subscribers);
                    subscribers.iter().cloned().collect::<Vec<_>>()
                };
                fan_out_notification(&senders, &notif);
            }
            EnvelopePayload::Request(_) => {
                tracing::trace!("unexpected request from server");
            }
        }
    }

    pub(crate) fn read_next_frame_generic<S: std::io::Read + std::io::Write>(
        stream: &mut S,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let (frame_len, header_len) = loop {
            if let Some((len, header_len)) = Self::try_parse_frame_header(buf)? {
                break (len, header_len);
            }

            let mut read_buf = [0u8; 256];
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed before frame header",
                    ));
                }
                Ok(n) => buf.extend_from_slice(&read_buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        };

        let total_len = header_len.checked_add(frame_len).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "frame length overflow")
        })?;
        while buf.len() < total_len {
            let mut read_buf = [0u8; 256];
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed while reading frame payload",
                    ));
                }
                Ok(n) => buf.extend_from_slice(&read_buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        }

        let frame = buf.drain(0..total_len).collect();
        Ok(Some(frame))
    }

    /// §9 尝试从缓冲区解析帧头（varint 长度前缀）。
    fn try_parse_frame_header(buf: &[u8]) -> std::io::Result<Option<(usize, usize)>> {
        let Some((len, header_len)) = parse_len_prefix(buf)? else {
            return Ok(None);
        };
        let len = check_frame_len(len)?;
        Ok(Some((len, header_len)))
    }

    /// §9 分配新的 request_id（§16.6 公开供扩展安装使用）。
    pub fn next_request_id(&self) -> u64 {
        self.inner
            .read()
            .next_request_id
            .fetch_add(1, Ordering::SeqCst)
    }

    /// Send a request and wait for its response.
    pub async fn send_request(&self, body: RequestBody) -> Result<Response> {
        self.send_request_with_timeout(body, Duration::from_secs(15))
            .await
    }

    async fn send_request_with_timeout(
        &self,
        body: RequestBody,
        timeout: Duration,
    ) -> Result<Response> {
        let request_id = self.next_request_id();
        let request = Request {
            request_id,
            body: Some(body),
        };
        let envelope = Envelope {
            version: Some(PROTOCOL_VERSION),
            payload: Some(EnvelopePayload::Request(request)),
        };
        let framed = frame(&envelope)?;

        let (tx, rx) = async_channel::bounded(1);
        let (write_tx, transport_epoch) = {
            let mut inner = self.inner.write();
            let transport_epoch = inner.transport_epoch.load(Ordering::SeqCst);
            inner.pending_requests.insert(
                request_id,
                PendingRequest {
                    sender: tx,
                    transport_epoch,
                },
            );
            (inner.write_tx.clone(), transport_epoch)
        };
        let _pending_request = PendingRequestGuard {
            inner: self.inner.clone(),
            request_id,
            transport_epoch,
        };

        match write_tx.try_send(framed) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                return Err(anyhow::anyhow!(
                    "mux write queue is full; request rejected before transport"
                ));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                return Err(anyhow::anyhow!("mux write channel disconnected"));
            }
        }
        // The wasm in-memory transport has no I/O thread to wake; pump it
        // directly so the queued frame is written and any synchronous reply
        // is routed before we start awaiting.
        #[cfg(target_family = "wasm")]
        if let Some(io) = self.wasm_io.lock().as_ref() {
            io.pump();
        }
        // This future is also polled by GPUI's executor, where no Tokio reactor
        // exists, so the timeout must be executor-neutral.
        let response = race_recv_with_timeout(rx, timeout).await;
        match response {
            Some(Ok(resp)) => {
                if let Some(ResponseBody::Error(err)) = &resp.body
                    && !err.is_empty()
                {
                    return Err(anyhow::anyhow!("mux server error: {}", err));
                }
                Ok(resp)
            }
            Some(Err(_)) => Err(anyhow::anyhow!("connection closed")),
            None => Err(anyhow::anyhow!("request timeout")),
        }
    }

    // ========================================================================
    // §9 Session 生命周期方法（§3.10）
    // ========================================================================

    /// §3.10 列出所有会话。
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let req = RequestBody::ListSessions(mux_protocol::ListSessionsRequest {});
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Sessions(list)) => Ok(list.sessions),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for list_sessions"
            )),
        }
    }

    /// §3.6 List validated persisted sessions awaiting explicit recovery.
    ///
    /// The response also carries the rows the scan rejected. They are not
    /// recoverable, but dropping them here would make an unreadable session
    /// indistinguishable from one that never existed.
    pub async fn list_recovery_candidates(
        &self,
    ) -> Result<mux_protocol::ListRecoveryCandidatesResponse> {
        let response = self
            .send_request(RequestBody::ListRecoveryCandidates(
                mux_protocol::ListRecoveryCandidatesRequest {},
            ))
            .await?;
        match response.body {
            Some(ResponseBody::RecoveryCandidates(list)) => Ok(list),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for list_recovery_candidates"
            )),
        }
    }

    /// §3.6 Explicitly recreate a persisted session using fresh default shells.
    pub async fn confirm_recovery(
        &self,
        session_id: &str,
    ) -> Result<mux_protocol::ConfirmRecoveryResponse> {
        let response = self
            .send_request(RequestBody::ConfirmRecovery(
                mux_protocol::ConfirmRecoveryRequest {
                    session_id: session_id.to_string(),
                },
            ))
            .await?;
        match response.body {
            Some(ResponseBody::RecoveryConfirmed(recovered)) => Ok(recovered),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for confirm_recovery"
            )),
        }
    }

    /// §3.10 创建新会话，返回会话 ID。
    pub async fn create_session(&self, name: &str, cwd: &Path) -> Result<String> {
        let req = RequestBody::CreateSession(mux_protocol::CreateSessionRequest {
            name: name.to_string(),
            cwd: cwd.to_string_lossy().to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Session(info)) => Ok(info.id),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for create_session"
            )),
        }
    }

    fn empty_or_error_response(response: Response) -> Result<()> {
        match response.body {
            Some(ResponseBody::Error(message)) if !message.is_empty() => {
                Err(anyhow::anyhow!(message))
            }
            _ => Ok(()),
        }
    }

    /// §3.10 结束指定会话。
    pub async fn kill_session(&self, id: &str) -> Result<()> {
        let req = RequestBody::KillSession(mux_protocol::KillSessionRequest { id: id.to_string() });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §3.5 Request an explicit mux_server process shutdown.
    pub async fn shutdown(&self) -> Result<()> {
        Self::empty_or_error_response(
            self.send_request(RequestBody::Shutdown(mux_protocol::ShutdownRequest {}))
                .await?,
        )
    }

    /// §3.10 重命名会话。
    pub async fn rename_session(&self, id: &str, name: &str) -> Result<()> {
        let req = RequestBody::RenameSession(mux_protocol::RenameSessionRequest {
            id: id.to_string(),
            name: name.to_string(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    // ========================================================================
    // §3.3 窗口管理方法 (多窗口支持，Plan 32)
    // ========================================================================

    /// §3.3 本连接代表的窗口 ID。
    pub fn window_id(&self) -> String {
        self.window_id.read().clone()
    }

    /// §3.3 换用服务端分配的权威窗口 ID。必须在 `attach` 之前调用, 否则服务端
    /// 记录的是本地铸的那个 ID。
    pub fn set_window_id(&self, window_id: String) {
        *self.window_id.write() = window_id;
    }

    /// §3.3 在指定会话中申请一个新窗口 ID，由服务端分配。
    ///
    /// 只分配 ID: 窗口真正加入会话是在随后的 `attach` 里完成的, 因为只有 attach
    /// 携带连接身份, 服务端才能在断连时精确释放这个窗口。
    pub async fn create_window(&self, session_id: &str) -> Result<String> {
        let req = RequestBody::NewWindow(mux_protocol::NewWindowRequest {
            session_id: session_id.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::NewWindow(r)) => Ok(r.window_id),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for create_window"
            )),
        }
    }

    /// §3.3 为这个域申请一个服务端分配的窗口 ID 并用它 attach (Plan 32)。
    ///
    /// GUI 每开一个窗口就建一个 `MuxDomain` 并走这条路径, 因此服务端看到的
    /// 每个窗口都对应一条独立连接。`NewWindow` 失败不阻断 attach: 窗口 ID 只是
    /// 成员资格标签, 退回本地铸的 ID 仍然全局唯一, 不值得让整个窗口打不开。
    pub async fn create_and_attach_window(&self, session_id: &str) -> Result<AttachResponse> {
        match self.create_window(session_id).await {
            Ok(window_id) => self.set_window_id(window_id),
            Err(error) => tracing::warn!(
                session_id,
                %error,
                "NewWindow failed; attaching with the locally minted window id"
            ),
        }
        self.attach(session_id, AttachMode::Shared).await
    }
    // ========================================================================
    // §9 Pane 生命周期方法（§3.10）
    // ========================================================================

    /// §3.10 在会话/标签页中创建新 Pane，返回 Pane ID。
    pub async fn spawn_pane(
        &self,
        session: &str,
        tab: &str,
        size: TerminalSize,
        command: Option<ShellCommand>,
        cwd: Option<&Path>,
    ) -> Result<String> {
        self.spawn_pane_idempotent(session, tab, size, command, cwd, None)
            .await
    }

    /// §3.10 As `spawn_pane`, but a caller that may retry supplies a key: the
    /// server answers a repeat with the pane the first request created rather
    /// than spawning a second shell.
    pub async fn spawn_pane_idempotent(
        &self,
        session: &str,
        tab: &str,
        size: TerminalSize,
        command: Option<ShellCommand>,
        cwd: Option<&Path>,
        idempotency_key: Option<String>,
    ) -> Result<String> {
        let req = RequestBody::SpawnPane(mux_protocol::SpawnPaneRequest {
            session_id: session.to_string(),
            tab_id: tab.to_string(),
            size: Some(size),
            command,
            cwd: cwd.map(|p| p.to_string_lossy().to_string()),
            idempotency_key,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::PaneId(id)) => Ok(id),
            Some(ResponseBody::Error(message)) => Err(anyhow::anyhow!(message)),
            _ => Err(anyhow::anyhow!("unexpected response type for spawn_pane")),
        }
    }

    /// §3.10 拆分已有 Pane，返回新 Pane ID。
    pub async fn split_pane(&self, pane: &str, direction: SplitDirection) -> Result<String> {
        self.split_pane_with_command(pane, direction, None).await
    }

    /// §3.10 Split an existing pane and optionally run a command in it.
    pub async fn split_pane_with_command(
        &self,
        pane: &str,
        direction: SplitDirection,
        command: Option<ShellCommand>,
    ) -> Result<String> {
        self.split_pane_idempotent(pane, direction, command, None)
            .await
    }

    /// §3.10 As `split_pane_with_command`, with the retry key described on
    /// [`MuxDomain::spawn_pane_idempotent`].
    pub async fn split_pane_idempotent(
        &self,
        pane: &str,
        direction: SplitDirection,
        command: Option<ShellCommand>,
        idempotency_key: Option<String>,
    ) -> Result<String> {
        let req = RequestBody::SplitPane(mux_protocol::SplitPaneRequest {
            pane_id: pane.to_string(),
            direction: direction as i32,
            command,
            cwd: None,
            idempotency_key,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::PaneId(id)) => Ok(id),
            Some(ResponseBody::Error(message)) => Err(anyhow::anyhow!(message)),
            _ => Err(anyhow::anyhow!("unexpected response type for split_pane")),
        }
    }

    /// §3.10 关闭 Pane。
    pub async fn close_pane(&self, pane: &str) -> Result<()> {
        let req = RequestBody::ClosePane(mux_protocol::ClosePaneRequest {
            pane_id: pane.to_string(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §3.10 聚焦 Pane。
    pub async fn focus_pane(&self, pane: &str) -> Result<()> {
        let req = RequestBody::FocusPane(mux_protocol::FocusPaneRequest {
            pane_id: pane.to_string(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §3.10 调整 Pane 尺寸。
    pub async fn resize_pane(&self, pane: &str, cols: u32, rows: u32) -> Result<()> {
        let req = RequestBody::ResizePane(mux_protocol::ResizePaneRequest {
            pane_id: pane.to_string(),
            cols,
            rows,
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §16.9 Adjust the server-authoritative layout ratio of a pane.
    pub async fn resize_layout(
        &self,
        pane: &str,
        direction: SplitDirection,
        delta: f32,
    ) -> Result<()> {
        let req = RequestBody::ResizeLayout(mux_protocol::ResizeLayoutRequest {
            pane_id: pane.to_string(),
            direction: direction as i32,
            delta,
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §16.9 Set one split node's ratios outright.
    ///
    /// What a divider drag reports: the ratios the client settled on, not the
    /// nudges that got it there. Absolute, so a repeat is a no-op.
    pub async fn set_layout_ratios(&self, node: &str, ratios: Vec<f32>) -> Result<()> {
        let req = RequestBody::SetLayoutRatios(mux_protocol::SetLayoutRatiosRequest {
            node_id: node.to_string(),
            ratios,
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §16.9 Move a pane beside another one — what a tab drag means to the
    /// server. Moving a pane to where it already sits is a no-op.
    pub async fn move_pane(
        &self,
        pane: &str,
        target: &str,
        direction: SplitDirection,
        before: bool,
    ) -> Result<()> {
        let req = RequestBody::MovePane(mux_protocol::MovePaneRequest {
            pane_id: pane.to_string(),
            target_pane_id: target.to_string(),
            direction: direction as i32,
            before,
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §3.10 设置 Pane 标题。
    pub async fn set_pane_title(&self, pane: &str, title: &str) -> Result<()> {
        let req = RequestBody::SetPaneTitle(mux_protocol::SetPaneTitleRequest {
            pane_id: pane.to_string(),
            title: title.to_string(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    // ========================================================================
    // §9 输入方法（§3.10）
    // ========================================================================

    /// §3.10 向 Pane 发送原始输入字节。
    pub async fn send_input(&self, pane: &str, bytes: &[u8]) -> Result<()> {
        let req = RequestBody::SendInput(mux_protocol::SendInputRequest {
            pane_id: pane.to_string(),
            data: bytes.to_vec(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §3.10 向 Pane 粘贴文本。
    pub async fn paste(&self, pane: &str, text: &str) -> Result<()> {
        let req = RequestBody::Paste(mux_protocol::PasteRequest {
            pane_id: pane.to_string(),
            text: text.to_string(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    // ========================================================================
    // §9 Grid Sync 方法（§3.3）
    // ========================================================================

    /// §3.3 拉取自指定 generation 以来的网格变更。
    pub async fn fetch_grid_update(
        &self,
        pane: &str,
        since: u64,
    ) -> Result<FetchGridUpdateResponse> {
        let req = RequestBody::FetchGridUpdate(mux_protocol::FetchGridUpdateRequest {
            pane_id: pane.to_string(),
            since_generation: since,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::GridUpdate(update)) => Ok(update),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for fetch_grid_update"
            )),
        }
    }

    /// §3.3 拉取历史滚动缓冲区。
    pub async fn fetch_scrollback(
        &self,
        pane: &str,
        from: u32,
        direction: u32,
        count: u32,
    ) -> Result<FetchScrollbackResponse> {
        let req = RequestBody::FetchScrollback(mux_protocol::FetchScrollbackRequest {
            pane_id: pane.to_string(),
            from_line: from,
            direction,
            count,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Scrollback(scrollback)) => Ok(scrollback),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for fetch_scrollback"
            )),
        }
    }

    /// §12 在 pane 的 scrollback 历史里做正则搜索。
    ///
    /// `from_line` 是历史行下标 (0 = 最旧)，`direction` 0 = 向更旧、1 = 向更新。
    /// 只搜历史，可见区不在范围内。
    pub async fn search_scrollback(
        &self,
        pane: &str,
        regex: &str,
        from_line: u32,
        direction: u32,
        max_results: u32,
    ) -> Result<SearchScrollbackResponse> {
        let req = RequestBody::SearchScrollback(mux_protocol::SearchScrollbackRequest {
            pane_id: pane.to_string(),
            regex: regex.to_string(),
            from_line,
            direction,
            max_results,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::SearchScrollback(matches)) => Ok(matches),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for search_scrollback"
            )),
        }
    }

    /// §3.10 列出 pane 里由 OSC 133 marker 划出的命令。
    ///
    /// `max_results` 为 0 表示"全部仍被保留的", 否则只取最近的 N 条。行号是
    /// tmux 模型 (可见区首行 0, 负数进历史), 缺省表示那一行已不可寻址 —— 退出
    /// 码不受影响, 仍然准确。
    pub async fn list_commands(
        &self,
        pane: &str,
        max_results: u32,
    ) -> Result<ListCommandsResponse> {
        let request = RequestBody::ListCommands(mux_protocol::ListCommandsRequest {
            pane_id: pane.to_string(),
            max_results,
        });
        let response = self.send_request(request).await?;
        match response.body {
            Some(ResponseBody::Commands(commands)) => Ok(commands),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for list_commands"
            )),
        }
    }

    // ========================================================================
    // §9 Attach / Detach（§3.10）
    // ========================================================================

    /// §3.10 连接会话，返回完整快照。
    pub async fn attach(&self, session: &str, mode: AttachMode_) -> Result<AttachResponse> {
        let req = RequestBody::Attach(mux_protocol::AttachRequest {
            session_id: session.to_string(),
            mode: mode as i32,
            window_id: self.window_id(),
            identity: None,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Attach(resp)) => {
                *self.last_attached_snapshot.write() = resp.snapshot.clone();
                *self.last_attached_session_id.write() = Some(session.to_string());
                Ok(resp)
            }
            _ => Err(anyhow::anyhow!("unexpected response type for attach")),
        }
    }

    /// §15.7 Session this domain last attached to, if any.
    pub fn last_attached_session_id(&self) -> Option<String> {
        self.last_attached_session_id.read().clone()
    }
    /// §15.4 Most recent authoritative snapshot for extension/UI hydration.
    pub fn last_attached_snapshot(&self) -> Option<mux_protocol::SessionSnapshot> {
        self.last_attached_snapshot.read().clone()
    }

    /// §3.10 断开连接。
    pub async fn detach(&self) -> Result<()> {
        let req = RequestBody::Detach(mux_protocol::DetachRequest {});
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    // ========================================================================
    // §3.3 Pane Zoom / Shell Integration
    // ========================================================================

    /// §3.3 设置 Pane zoom 状态。
    pub async fn zoom_pane(&self, pane: &str, zoom: bool) -> Result<()> {
        let req = RequestBody::ZoomPane(mux_protocol::ZoomPaneRequest {
            pane_id: pane.to_string(),
            zoom,
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    /// §3.3 查询 Pane 的 shell integration 状态 (cwd + prompt marker)。
    pub async fn get_shell_integration(&self, pane: &str) -> Result<ShellIntegrationResponse> {
        let req = RequestBody::ShellIntegration(mux_protocol::ShellIntegrationRequest {
            pane_id: pane.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::ShellIntegration(si)) => Ok(si),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for get_shell_integration"
            )),
        }
    }

    /// §3.1 In-place render-path: 订阅 Pane 的 PTY 输出字节流。
    /// 返回空响应确认订阅成功；实际字节通过 subscribe() 通知通道以 PaneOutputChunk 推送。
    pub async fn subscribe_pane_output(&self, pane: &str) -> Result<()> {
        let req = RequestBody::SubscribePaneOutput(mux_protocol::SubscribePaneOutputRequest {
            pane_id: pane.to_string(),
        });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    // ========================================================================
    // §4 Shadow File Versions（crash-safe 文件系统版本控制）
    // ========================================================================

    /// §4 列出指定会话内所有留有 shadow 版本的文件，按最新 SeqNo 从新到旧。
    pub async fn list_changed_files(&self, session_id: &str) -> Result<ListChangedFilesResponse> {
        let req = RequestBody::ListChangedFiles(mux_protocol::ListChangedFilesRequest {
            session_id: session_id.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::ChangedFiles(changed)) => Ok(changed),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for list_changed_files"
            )),
        }
    }

    /// §4 列出指定会话内某路径的全部 shadow 版本。
    pub async fn list_file_versions(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<ListFileVersionsResponse> {
        let req = RequestBody::ListFileVersions(mux_protocol::ListFileVersionsRequest {
            session_id: session_id.to_string(),
            path: path.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::FileVersions(versions)) => Ok(versions),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for list_file_versions"
            )),
        }
    }

    /// §4 获取指定版本的文件字节内容。
    pub async fn get_file_version(
        &self,
        session_id: &str,
        path: &str,
        version_id: u64,
    ) -> Result<GetFileVersionResponse> {
        let req = RequestBody::GetFileVersion(mux_protocol::GetFileVersionRequest {
            session_id: session_id.to_string(),
            path: path.to_string(),
            version_id,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::FileVersionContent(content)) => Ok(content),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for get_file_version"
            )),
        }
    }

    /// §4.8 把文件还原成 `version_id` 那一版的内容，撤销此后的改动。
    /// 传的是要还原到的版本，不是要丢弃的那一版。
    pub async fn decline_file_version(
        &self,
        session_id: &str,
        path: &str,
        version_id: u64,
    ) -> Result<DeclineFileVersionResponse> {
        let req = RequestBody::DeclineFileVersion(mux_protocol::DeclineFileVersionRequest {
            session_id: session_id.to_string(),
            path: path.to_string(),
            version_id,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::DeclineFileVersion(resp)) => Ok(resp),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for decline_file_version"
            )),
        }
    }

    // ========================================================================
    // §16.6 服务端剪贴板中继
    // ========================================================================

    /// §16.6 读取服务端剪贴板。
    ///
    /// 剪贴板从来没被设置过时服务端回的是一条空 TEXT 条目而不是 `None`，所以
    /// "没设置过"和"设置成了空文本"在协议上分不开; 调用方只能按空内容处理。
    pub async fn get_clipboard(&self) -> Result<ClipboardEntry> {
        let req = RequestBody::GetClipboard(mux_protocol::GetClipboardRequest {});
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Clipboard(clipboard)) => clipboard
                .entry
                .context("clipboard response carried no entry"),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!(
                "unexpected response type for get_clipboard"
            )),
        }
    }

    /// §16.6 设置服务端剪贴板，并让服务端向所有 attach 的客户端 fan-out
    /// `ClipboardChanged`。
    ///
    /// 服务端成功时回的是**空** `Error` 体 (proto: `non-empty = error`)，因此这里
    /// 必须走 `empty_or_error_response`; 直接 `Some(Error(e)) => Err(e)` 会把每一次
    /// 成功都变成一条错误信息为空的失败。
    pub async fn set_clipboard(&self, entry: ClipboardEntry) -> Result<()> {
        let req =
            RequestBody::SetClipboard(mux_protocol::SetClipboardRequest { entry: Some(entry) });
        Self::empty_or_error_response(self.send_request(req).await?)
    }

    // ========================================================================
    // §16.6 会话 worktree 内的文件访问
    // ========================================================================

    /// §16.6 读取本连接已 attach 的会话 worktree 内的一个文件。
    ///
    /// 路径相对该会话的 cwd 解析，绝对路径必须落在 cwd 内，`..` 一律拒绝。没有
    /// attach 过任何会话的连接没有 worktree 范围，服务端会直接拒绝而不是退化成
    /// 整个文件系统。
    ///
    /// The compatibility API still returns one aggregate buffer, but obtains it
    /// through bounded byte pages so no individual response contains a large file.
    pub async fn read_file(&self, path: &str) -> Result<ReadFileResponse> {
        let mut offset_bytes = 0;
        let mut result = ReadFileResponse::default();
        let mut first_page = true;
        loop {
            let page = self
                .read_file_page(
                    path,
                    offset_bytes,
                    mux_protocol::DEFAULT_READ_FILE_PAGE_BYTES,
                )
                .await?;
            if first_page {
                result.is_binary = page.is_binary;
                result.encoding = page.encoding.clone();
                result.total_lines = page.total_lines;
                result.total_bytes = page.total_bytes;
                first_page = false;
            } else {
                anyhow::ensure!(
                    page.is_binary == result.is_binary && page.encoding == result.encoding,
                    "read_file metadata changed while paging {path}"
                );
                anyhow::ensure!(
                    page.total_bytes == result.total_bytes,
                    "read_file size changed while paging {path}: {} became {} bytes",
                    result.total_bytes,
                    page.total_bytes
                );
            }
            result.content.extend_from_slice(&page.content);

            let Some(next_offset_bytes) = page.next_offset_bytes else {
                return Ok(result);
            };
            offset_bytes = next_offset_bytes;
        }
    }

    /// Read one bounded byte page from a file.
    pub async fn read_file_page(
        &self,
        path: &str,
        offset_bytes: u64,
        max_bytes: u32,
    ) -> Result<ReadFileResponse> {
        anyhow::ensure!(
            (1..=mux_protocol::MAX_READ_FILE_PAGE_BYTES).contains(&max_bytes),
            "max_bytes must be between 1 and {}",
            mux_protocol::MAX_READ_FILE_PAGE_BYTES
        );
        let page = self
            .send_read_file_request(mux_protocol::ReadFileRequest {
                path: path.to_string(),
                offset_line: None,
                max_lines: None,
                offset_bytes: Some(offset_bytes),
                max_bytes: Some(max_bytes),
            })
            .await?;
        validate_read_file_byte_page(path, &page, offset_bytes, max_bytes)?;
        Ok(page)
    }

    /// Read one page delimited by logical LF-terminated lines.
    pub async fn read_file_lines(
        &self,
        path: &str,
        offset_line: u32,
        max_lines: u32,
    ) -> Result<ReadFileResponse> {
        self.send_read_file_request(mux_protocol::ReadFileRequest {
            path: path.to_string(),
            offset_line: Some(offset_line),
            max_lines: Some(max_lines),
            offset_bytes: None,
            max_bytes: None,
        })
        .await
    }

    async fn send_read_file_request(
        &self,
        request: mux_protocol::ReadFileRequest,
    ) -> Result<ReadFileResponse> {
        let req = RequestBody::ReadFile(request);
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::FileContent(content)) => Ok(content),
            // 越界路径 / 读不到的文件服务端回的是 Error 体; 落进兜底分支会把原因
            // 换成一句无信息量的 "unexpected"。
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!("unexpected response type for read_file")),
        }
    }

    /// §16.6 列出已 attach 会话 worktree 内某个目录的条目，目录在前、其余按名称。
    ///
    /// `is_modified` 的含义是"本 session 的影子快照给它记过版本"; 会话没有 armed
    /// 的 watcher 时它恒为 false，那是"未知"而不是"未改过"。
    pub async fn list_dir(&self, path: &str) -> Result<ListDirResponse> {
        let req = RequestBody::ListDir(mux_protocol::ListDirRequest {
            path: path.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::DirListing(listing)) => Ok(listing),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!("unexpected response type for list_dir")),
        }
    }

    /// §16.6 取已 attach 会话 worktree 内某个路径的元数据。
    ///
    /// 路径不存在不是错误：服务端回 `exists=false` 的类型化响应。只有路径越界或
    /// stat 本身失败 (权限等) 才是 Error 体。
    pub async fn stat_file(&self, path: &str) -> Result<StatFileResponse> {
        let req = RequestBody::StatFile(mux_protocol::StatFileRequest {
            path: path.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::FileStat(stat)) => Ok(stat),
            Some(ResponseBody::Error(error)) => Err(anyhow::anyhow!(error)),
            _ => Err(anyhow::anyhow!("unexpected response type for stat_file")),
        }
    }

    // ========================================================================
    // §9 订阅通知（§9）
    // ========================================================================

    pub fn subscribe(&self) -> NotificationReceiver {
        let (tx, rx) = async_channel::bounded(NOTIFICATION_QUEUE_CAPACITY);
        let dirty_latches = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        #[cfg(target_family = "wasm")]
        let reliable_overflow = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        self.inner.read().subscribers.lock().push(SubscriberSender {
            sender: tx,
            dirty_latches: dirty_latches.clone(),
            #[cfg(target_family = "wasm")]
            reliable_overflow: reliable_overflow.clone(),
        });
        NotificationReceiver {
            queue: rx,
            dirty_latches,
            #[cfg(target_family = "wasm")]
            reliable_overflow,
        }
    }

    // ========================================================================
    // §15.4 Reconnect helpers: subscriber transfer + synthetic notification
    // ========================================================================

    /// Probe whether the connection is alive by issuing a lightweight
    /// list-sessions request. Returns `true` if the io thread is still
    /// active and the server responded.
    pub async fn check_connection(&self) -> bool {
        self.list_sessions().await.is_ok()
    }

    /// Extract the subscriber list, leaving an empty list in its place.
    /// Used during reconnect to transfer subscribers from the old (dead)
    /// domain into the freshly connected domain.
    pub fn take_subscribers(&self) -> Arc<parking_lot::Mutex<Vec<SubscriberSender>>> {
        let inner = self.inner.write();
        let mut subs_guard = inner.subscribers.lock();
        let taken = std::mem::take(&mut *subs_guard);
        Arc::new(parking_lot::Mutex::new(taken))
    }

    /// Install a previously extracted subscriber list into this domain.
    /// Any pre-existing subscribers are replaced.
    pub fn install_subscribers(&self, subs: Arc<parking_lot::Mutex<Vec<SubscriberSender>>>) {
        let mut inner = self.inner.write();
        inner.subscribers = subs;
    }

    /// Broadcast a synthetic notification to every subscriber (at-least-once).
    /// Used after reconnect to deliver a SessionLayoutChanged without waiting
    /// for the server to push one. Same delivery rules as the router fan-out:
    /// lifecycle events block, a full-queue `PaneDirty` latches for later
    /// synthesis instead of stalling the reconnect.
    pub fn broadcast_notification(&self, notif: Notification) {
        let senders = {
            let inner = self.inner.read();
            let mut subscribers = inner.subscribers.lock();
            prune_closed_subscribers(&mut subscribers);
            subscribers.iter().cloned().collect::<Vec<_>>()
        };
        fan_out_notification(&senders, &notif);
    }

    /// §15.4 / §15.12 Authoritative in-place reconnect.
    ///
    /// Opens a fresh local socket and spawns a new I/O thread bound to
    /// `self.inner`'s existing `Arc<RwLock<DomainInner>>`, then swaps the
    /// transport-bound fields of that `DomainInner` in place. Because the
    /// new I/O thread and `self` share the *same* `Arc`, request/response
    /// routing and notification fan-out keep working for every existing
    /// `Arc<MuxDomain>` and every already-registered subscriber — no GUI
    /// re-wiring required.
    ///
    /// `self.window_id` is preserved (the server sees the same logical
    /// window across reconnect). Subscriber senders registered before the
    /// reconnect remain wired to `self.inner`'s subscribers `Mutex`, which
    /// is exactly the `Mutex` the new I/O thread fans out into, so they
    /// keep receiving server-pushed notifications.
    ///
    /// After the swap, re-attaches the supplied active `session_id` and
    /// broadcasts a synthetic `SessionLayoutChanged` derived from the full
    /// authoritative snapshot returned by the server — observers reconcile
    /// from the snapshot rather than racing the at-least-once push path.
    #[cfg(not(target_family = "wasm"))]
    pub async fn reconnect_at_path_in_place(
        &self,
        path: &Path,
        session_id: &str,
        attach_mode: AttachMode,
    ) -> Result<()> {
        let path = path.to_path_buf();
        let stream = connect_local_stream(Some(&path))?;
        let (new_write_tx, new_write_rx) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let io_inner = self.inner.clone();

        let old_pending = {
            let mut inner = self.inner.write();
            let old_epoch = inner.transport_epoch.load(Ordering::SeqCst);
            let new_epoch = old_epoch
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("mux transport epoch exhausted"))?;
            let io_subscribers = inner.subscribers.clone();

            let old_pending = drain_pending_requests_for_epoch(&mut inner, old_epoch);
            inner.write_tx = new_write_tx;
            inner.transport_epoch.store(new_epoch, Ordering::SeqCst);

            if let Err(error) = std::thread::Builder::new()
                .name("mux-io".into())
                .spawn(move || {
                    Self::io_and_router_loop(
                        stream,
                        new_write_rx,
                        io_inner,
                        io_subscribers,
                        new_epoch,
                    );
                })
            {
                return Err(anyhow::anyhow!("failed to spawn mux I/O thread: {}", error));
            }

            old_pending
        };
        *self.local_socket_path.write() = Some(path);
        drop(old_pending);

        let attach_resp = self.attach(session_id, attach_mode).await?;
        if let Some(snapshot) = attach_resp.snapshot.as_ref() {
            if let Some(layout) = snapshot.layout.as_ref() {
                self.broadcast_notification(Notification {
                    event: Some(NotifEvent::SessionLayoutChanged(SessionLayoutChanged {
                        layout: Some(layout.clone()),
                        snapshot: Some(snapshot.clone()),
                    })),
                });
            }

            for tab in &snapshot.tabs {
                for pane in &tab.panes {
                    self.broadcast_notification(Notification {
                        event: Some(NotifEvent::PaneDirty(mux_protocol::PaneDirty {
                            pane_id: pane.id.clone(),
                        })),
                    });
                }
            }
        }
        Ok(())
    }

    /// Reconnect at the socket selected by the original local connection or the
    /// last successful path-selectable reconnect.
    #[cfg(not(target_family = "wasm"))]
    pub async fn reconnect_local_in_place(
        &self,
        session_id: &str,
        attach_mode: AttachMode,
    ) -> Result<()> {
        let path = self
            .local_socket_path
            .read()
            .clone()
            .unwrap_or_else(default_socket_path);
        self.reconnect_at_path_in_place(&path, session_id, attach_mode)
            .await
    }
    /// The in-memory wasm transport never drops, so reconnect is meaningless.
    #[cfg(target_family = "wasm")]
    pub async fn reconnect_at_path_in_place(
        &self,
        _path: &Path,
        _session_id: &str,
        _attach_mode: AttachMode,
    ) -> Result<()> {
        Err(anyhow::anyhow!(
            "in-place reconnect is unavailable on the in-memory wasm transport"
        ))
    }

    /// The in-memory wasm transport never drops, so reconnect is meaningless.
    #[cfg(target_family = "wasm")]
    pub async fn reconnect_local_in_place(
        &self,
        _session_id: &str,
        _attach_mode: AttachMode,
    ) -> Result<()> {
        Err(anyhow::anyhow!(
            "in-place reconnect is unavailable on the in-memory wasm transport"
        ))
    }
}

fn validate_read_file_byte_page(
    path: &str,
    page: &ReadFileResponse,
    requested_offset: u64,
    max_bytes: u32,
) -> Result<()> {
    anyhow::ensure!(
        page.content.len() <= max_bytes as usize,
        "read_file returned {} bytes for {path}, exceeding the requested page size of {max_bytes}",
        page.content.len()
    );
    anyhow::ensure!(
        page.offset_bytes == requested_offset,
        "read_file returned byte offset {} while {requested_offset} was requested for {path}",
        page.offset_bytes
    );
    anyhow::ensure!(
        page.offset_bytes <= page.total_bytes,
        "read_file returned offset {} beyond the {} byte file {path}",
        page.offset_bytes,
        page.total_bytes
    );
    let page_end = page
        .offset_bytes
        .checked_add(page.content.len() as u64)
        .with_context(|| format!("read_file byte offset overflow for {path}"))?;
    anyhow::ensure!(
        page_end <= page.total_bytes,
        "read_file page for {path} ends at {page_end}, beyond {} bytes",
        page.total_bytes
    );
    match page.next_offset_bytes {
        Some(next_offset) => {
            anyhow::ensure!(
                next_offset == page_end,
                "read_file returned a discontinuous next offset {next_offset} for {path}; expected {page_end}"
            );
            anyhow::ensure!(
                next_offset > page.offset_bytes && next_offset < page.total_bytes,
                "read_file returned an invalid continuation offset {next_offset} for {path}"
            );
        }
        None => anyhow::ensure!(
            page_end == page.total_bytes,
            "read_file ended early at byte {page_end} of {} for {path}",
            page.total_bytes
        ),
    }
    Ok(())
}

/// Subscriber-side handle to a `MuxDomain` notification stream.
///
/// Wraps the bounded ordinary queue with a per-pane coalesced dirty latch:
/// when the router cannot enqueue a lossy `PaneDirty` because the queue is
/// full, the pane is recorded in the latch instead of dropped, and the
/// receiver returns exactly one synthesized `PaneDirty` per latched pane
/// after the ordinary queue has been drained. The ordinary queue stays
/// bounded — a flood of dropped dirty events coalesces into at most one
/// latch entry per pane rather than buffering per event.
pub struct NotificationReceiver {
    queue: async_channel::Receiver<Notification>,
    dirty_latches: Arc<parking_lot::Mutex<HashSet<String>>>,
    #[cfg(target_family = "wasm")]
    reliable_overflow: Arc<parking_lot::Mutex<VecDeque<Notification>>>,
}

impl NotificationReceiver {
    /// Receive the next notification, blocking until one is available.
    ///
    /// Queued notifications always come first; once the ordinary queue is
    /// empty, one synthesized `PaneDirty` per latched pane is returned before
    /// blocking for new traffic.
    pub async fn recv(&self) -> Result<Notification, async_channel::RecvError> {
        match self.queue.try_recv() {
            Ok(notification) => Ok(notification),
            Err(async_channel::TryRecvError::Empty) => {
                #[cfg(target_family = "wasm")]
                if let Some(notification) = self.take_reliable_overflow() {
                    return Ok(notification);
                }
                if let Some(notification) = self.take_latched_dirty() {
                    Ok(notification)
                } else {
                    self.queue.recv().await
                }
            }
            Err(async_channel::TryRecvError::Closed) => Err(async_channel::RecvError),
        }
    }

    /// Receive the next notification without blocking.
    ///
    /// Returns a queued notification if one is pending; once the ordinary
    /// queue is empty, returns one synthesized `PaneDirty` per latched pane.
    pub fn try_recv(&self) -> Result<Notification, async_channel::TryRecvError> {
        match self.queue.try_recv() {
            Ok(notification) => Ok(notification),
            Err(async_channel::TryRecvError::Empty) => {
                #[cfg(target_family = "wasm")]
                if let Some(notification) = self.take_reliable_overflow() {
                    return Ok(notification);
                }
                self.take_latched_dirty()
                    .ok_or(async_channel::TryRecvError::Empty)
            }
            Err(error @ async_channel::TryRecvError::Closed) => Err(error),
        }
    }

    /// Synthesize one `PaneDirty` for a latched pane, if any remains.
    /// Pop the oldest overflowed reliable notification (wasm only). The
    /// ordinary channel is already empty when this runs, so FIFO is kept.
    #[cfg(target_family = "wasm")]
    fn take_reliable_overflow(&self) -> Option<Notification> {
        self.reliable_overflow.lock().pop_front()
    }

    ///
    /// The pane is removed from the latch, so a burst of dropped dirty events
    /// for one pane still produces exactly one synthesized notification.
    fn take_latched_dirty(&self) -> Option<Notification> {
        let mut latches = self.dirty_latches.lock();
        let pane_id = latches.iter().next()?.clone();
        latches.remove(&pane_id);
        Some(Notification {
            event: Some(NotifEvent::PaneDirty(mux_protocol::PaneDirty { pane_id })),
        })
    }
}

// ============================================================================
// §9 MuxNotification: 公共通知类型别名
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn read_file_byte_page(
        offset_bytes: u64,
        content: &[u8],
        next_offset_bytes: Option<u64>,
        total_bytes: u64,
    ) -> ReadFileResponse {
        ReadFileResponse {
            content: content.to_vec(),
            is_binary: true,
            encoding: "binary".to_string(),
            offset_line: 0,
            next_offset_line: None,
            total_lines: 0,
            offset_bytes,
            next_offset_bytes,
            total_bytes,
        }
    }

    #[test]
    fn read_file_byte_page_validation_accepts_contiguous_pages() {
        let first = read_file_byte_page(0, b"abcd", Some(4), 6);
        validate_read_file_byte_page("file", &first, 0, 4).expect("first page");

        let last = read_file_byte_page(4, b"ef", None, 6);
        validate_read_file_byte_page("file", &last, 4, 4).expect("last page");
    }

    #[test]
    fn read_file_byte_page_validation_rejects_gaps_and_truncation() {
        let skipped = read_file_byte_page(0, b"ab", Some(3), 4);
        assert!(validate_read_file_byte_page("file", &skipped, 0, 2).is_err());

        let truncated = read_file_byte_page(0, b"ab", None, 4);
        assert!(validate_read_file_byte_page("file", &truncated, 0, 2).is_err());

        let empty_continuation = read_file_byte_page(0, b"", Some(0), 4);
        assert!(validate_read_file_byte_page("file", &empty_continuation, 0, 2).is_err());
    }

    #[test]
    fn read_file_byte_page_validation_rejects_oversized_or_out_of_range_pages() {
        let oversized = read_file_byte_page(0, b"abc", Some(3), 4);
        assert!(validate_read_file_byte_page("file", &oversized, 0, 2).is_err());

        let wrong_offset = read_file_byte_page(2, b"ab", None, 4);
        assert!(validate_read_file_byte_page("file", &wrong_offset, 0, 2).is_err());

        let past_end = read_file_byte_page(5, b"", None, 4);
        assert!(validate_read_file_byte_page("file", &past_end, 5, 2).is_err());

        let content_past_end = read_file_byte_page(3, b"ab", None, 4);
        assert!(validate_read_file_byte_page("file", &content_past_end, 3, 2).is_err());
    }

    #[test]
    fn lifecycle_blocks_while_byte_stream_drops_and_dirty_latches() {
        let output = Notification {
            event: Some(NotifEvent::PaneOutput(mux_protocol::PaneOutputChunk {
                pane_id: "pane-1".to_string(),
                data: b"\x1b[2;3Htext".to_vec(),
                output_sequence: 7,
            })),
        };
        let dirty = Notification {
            event: Some(NotifEvent::PaneDirty(mux_protocol::PaneDirty {
                pane_id: "pane-1".to_string(),
            })),
        };
        let removed = Notification {
            event: Some(NotifEvent::PaneRemoved(mux_protocol::PaneRemoved {
                pane_id: "pane-1".to_string(),
                exit_code: 0,
            })),
        };

        // The byte stream and PaneDirty are supplemental dirty signals, never
        // reliable: dropping either only defers an authoritative grid pull.
        assert!(!notification_requires_reliable_delivery(&output));
        assert!(!notification_requires_reliable_delivery(&dirty));
        // Lifecycle events remain at-least-once and block the router.
        assert!(notification_requires_reliable_delivery(&removed));
        let media = Notification {
            event: Some(NotifEvent::PaneMedia(mux_protocol::PaneMedia::default())),
        };
        let action = Notification {
            event: Some(NotifEvent::PaneAction(mux_protocol::PaneAction::default())),
        };
        assert!(notification_requires_reliable_delivery(&media));
        assert!(notification_requires_reliable_delivery(&action));
    }

    /// §3.4 Deterministic regression: a subscriber saturated with lossy
    /// notifications must not lose a dropped `PaneDirty` — the router records
    /// the pane in the subscriber's latch, and the receiver synthesizes exactly
    /// one `PaneDirty` for it after draining the bounded ordinary queue,
    /// without unbounded buffering.
    #[test]
    fn saturated_subscriber_synthesizes_one_dirty_per_latched_pane_after_drain() {
        let (tx, rx) = async_channel::bounded(2);
        let dirty_latches = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        let subscriber = SubscriberSender {
            sender: tx,
            dirty_latches: dirty_latches.clone(),
        };
        let receiver = NotificationReceiver {
            queue: rx,
            dirty_latches,
        };

        let output = Notification {
            event: Some(NotifEvent::PaneOutput(mux_protocol::PaneOutputChunk {
                pane_id: "pane-1".to_string(),
                data: b"x".to_vec(),
                output_sequence: 1,
            })),
        };
        let dirty = |pane_id: &str| Notification {
            event: Some(NotifEvent::PaneDirty(mux_protocol::PaneDirty {
                pane_id: pane_id.to_string(),
            })),
        };

        // Saturate the ordinary queue with lossy output chunks (capacity 2);
        // the overflow chunks drop, exactly like a slow renderer under a burst.
        for _ in 0..4 {
            fan_out_notification(std::slice::from_ref(&subscriber), &output);
        }
        assert_eq!(receiver.queue.len(), 2, "ordinary queue must stay bounded");

        // A full-queue PaneDirty for the target pane is latched, not dropped,
        // and a second one for the same pane coalesces into the same entry.
        fan_out_notification(std::slice::from_ref(&subscriber), &dirty("target-pane"));
        fan_out_notification(std::slice::from_ref(&subscriber), &dirty("target-pane"));
        assert_eq!(receiver.dirty_latches.lock().len(), 1);

        // Drain: queued notifications come first, then exactly one synthesized
        // PaneDirty for the target pane, then the stream goes quiet.
        let mut queued_outputs = 0;
        let mut synthesized = 0;
        loop {
            match receiver.try_recv() {
                Ok(notification) => match notification.event {
                    Some(NotifEvent::PaneOutput(_)) => queued_outputs += 1,
                    Some(NotifEvent::PaneDirty(dirty)) => {
                        assert_eq!(dirty.pane_id, "target-pane");
                        synthesized += 1;
                    }
                    other => panic!("unexpected notification {other:?}"),
                },
                Err(async_channel::TryRecvError::Empty) => break,
                Err(error) => panic!("unexpected try_recv error {error:?}"),
            }
        }
        assert_eq!(
            queued_outputs, 2,
            "queued notifications must drain before synthesized ones"
        );
        assert_eq!(
            synthesized, 1,
            "exactly one synthesized PaneDirty per latched pane"
        );
        assert!(receiver.dirty_latches.lock().is_empty());
        assert_eq!(receiver.queue.capacity(), Some(2), "ordinary queue must stay bounded");
        assert_eq!(receiver.queue.len(), 0);

        // Re-saturate: a pane latched again synthesizes exactly one more event
        // on the next drain, never a flood of buffered notifications.
        for _ in 0..4 {
            fan_out_notification(std::slice::from_ref(&subscriber), &output);
        }
        fan_out_notification(std::slice::from_ref(&subscriber), &dirty("target-pane"));
        let queued = receiver.try_recv().expect("queued output must drain first");
        assert!(matches!(queued.event, Some(NotifEvent::PaneOutput(_))));
        let queued = receiver.try_recv().expect("queued output must drain first");
        assert!(matches!(queued.event, Some(NotifEvent::PaneOutput(_))));
        let notification = receiver.try_recv().expect("latched dirty must synthesize");
        let Some(NotifEvent::PaneDirty(dirty)) = notification.event else {
            panic!("expected synthesized PaneDirty, got {notification:?}");
        };
        assert_eq!(dirty.pane_id, "target-pane");
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    /// §3.4 The latch is per pane: distinct panes dropped while the queue is
    /// full each get exactly one synthesized `PaneDirty`, and `recv` returns a
    /// latched dirty immediately instead of blocking on the empty queue.
    #[test]
    fn saturated_subscriber_keeps_one_latch_entry_per_pane_and_recv_does_not_block() {
        let (tx, rx) = async_channel::bounded(1);
        let dirty_latches = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        let subscriber = SubscriberSender {
            sender: tx,
            dirty_latches: dirty_latches.clone(),
        };
        let receiver = NotificationReceiver {
            queue: rx,
            dirty_latches,
        };

        let dirty = |pane_id: &str| Notification {
            event: Some(NotifEvent::PaneDirty(mux_protocol::PaneDirty {
                pane_id: pane_id.to_string(),
            })),
        };

        // Fill the queue so every dirty below is latched.
        fan_out_notification(
            std::slice::from_ref(&subscriber),
            &Notification {
                event: Some(NotifEvent::PaneOutput(mux_protocol::PaneOutputChunk {
                    pane_id: "filler".to_string(),
                    data: b"x".to_vec(),
                    output_sequence: 1,
                })),
            },
        );
        fan_out_notification(std::slice::from_ref(&subscriber), &dirty("pane-a"));
        fan_out_notification(std::slice::from_ref(&subscriber), &dirty("pane-b"));
        fan_out_notification(std::slice::from_ref(&subscriber), &dirty("pane-a"));
        assert_eq!(
            receiver.dirty_latches.lock().len(),
            2,
            "one latch entry per distinct pane, coalescing repeats"
        );

        // Drain the queued filler first.
        assert!(matches!(
            receiver.try_recv(),
            Ok(Notification {
                event: Some(NotifEvent::PaneOutput(_)),
                ..
            })
        ));

        // With the queue empty, recv must return a latched dirty without
        // blocking on new traffic.
        let delivered = smol::block_on(smol::future::or(
            async { receiver.recv().await.ok() },
            async {
                smol::Timer::after(Duration::from_millis(200)).await;
                None
            },
        ))
        .expect("recv must not block while a latched dirty is pending");
        let Some(NotifEvent::PaneDirty(first)) = delivered.event else {
            panic!("expected synthesized PaneDirty, got {delivered:?}");
        };

        let second = receiver.try_recv().expect("second latched pane must synthesize");
        let Some(NotifEvent::PaneDirty(second)) = second.event else {
            panic!("expected synthesized PaneDirty, got {second:?}");
        };

        let mut latched = [first.pane_id, second.pane_id];
        latched.sort();
        assert_eq!(latched, ["pane-a", "pane-b"]);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(receiver.dirty_latches.lock().is_empty());
    }

    /// §3.4 Closed subscriber records are pruned, so a dropped receiver stops
    /// consuming router work (and its latch stops growing).
    #[test]
    fn closed_subscriber_records_are_pruned() {
        let (live_tx, _live_rx) = async_channel::bounded(1);
        let (closed_tx, closed_rx) = async_channel::bounded(1);
        drop(closed_rx);
        let mut subscribers = vec![
            SubscriberSender {
                sender: live_tx,
                dirty_latches: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            },
            SubscriberSender {
                sender: closed_tx,
                dirty_latches: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            },
        ];

        prune_closed_subscribers(&mut subscribers);

        assert_eq!(subscribers.len(), 1);
        assert!(!subscribers[0].sender.is_closed());
    }
    #[test]
    fn server_extension_chrome_is_reliable() {
        let notification = Notification {
            event: Some(NotifEvent::ExtensionChrome(
                mux_protocol::ExtensionChromeUpdate {
                    extension_id: "server-ext".to_string(),
                    view_id: "status".to_string(),
                    vdom_payload: br#"{"type":"span"}"#.to_vec(),
                },
            )),
        };

        assert!(notification_requires_reliable_delivery(&notification));
    }

    fn unresponsive_domain() -> (MuxDomain, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (write_tx, write_rx) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let subscribers = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let domain = MuxDomain {
            inner: Arc::new(parking_lot::RwLock::new(DomainInner {
                next_request_id: AtomicU64::new(1),
                pending_requests: HashMap::new(),
                subscribers,
                write_tx,
                transport_epoch: AtomicU64::new(0),
            })),
            window_id: parking_lot::RwLock::new("test-window".to_string()),
            local_socket_path: parking_lot::RwLock::new(None),
            last_attached_snapshot: parking_lot::RwLock::new(None),
            last_attached_session_id: parking_lot::RwLock::new(None),
        };
        (domain, write_rx)
    }

    #[test]
    fn request_timeout_does_not_require_tokio_runtime() {
        let (domain, write_rx) = unresponsive_domain();

        let error = smol::block_on(domain.send_request_with_timeout(
            RequestBody::ListSessions(mux_protocol::ListSessionsRequest {}),
            Duration::from_millis(10),
        ))
        .expect_err("an unresponsive server must time out");

        assert!(error.to_string().contains("request timeout"));
        assert!(write_rx.try_recv().is_ok(), "request must be written");
        assert!(domain.inner.read().pending_requests.is_empty());
    }

    #[test]
    fn blocking_operation_does_not_stall_async_executor() {
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let operation_release = release.clone();

        let operation_completed_first = smol::block_on(smol::future::or(
            async move {
                run_blocking_operation("mux-test-blocking", move || {
                    while !operation_release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(())
                })
                .await
                .is_ok()
            },
            async move {
                smol::Timer::after(Duration::from_millis(10)).await;
                release.store(true, Ordering::SeqCst);
                false
            },
        ));

        assert!(
            !operation_completed_first,
            "executor timer must advance while blocking operation runs on its OS thread"
        );
    }

    #[test]
    fn cancelling_request_removes_pending_entry() {
        let (domain, write_rx) = unresponsive_domain();

        let completed = smol::block_on(smol::future::or(
            async {
                domain
                    .send_request_with_timeout(
                        RequestBody::ListSessions(mux_protocol::ListSessionsRequest {}),
                        Duration::from_secs(60),
                    )
                    .await
                    .is_ok()
            },
            async {
                smol::Timer::after(Duration::from_millis(10)).await;
                false
            },
        ));

        assert!(!completed);
        assert!(write_rx.try_recv().is_ok(), "request must be written");
        assert!(domain.inner.read().pending_requests.is_empty());
    }

    #[test]
    fn stale_transport_cannot_fulfill_or_cancel_new_request() {
        let (domain, _write_rx) = unresponsive_domain();
        let (old_sender, _old_receiver) = async_channel::bounded(1);
        let (new_sender, new_receiver) = async_channel::bounded(1);
        {
            let mut inner = domain.inner.write();
            inner.pending_requests.insert(
                1,
                PendingRequest {
                    sender: old_sender,
                    transport_epoch: 0,
                },
            );
            inner.pending_requests.insert(
                2,
                PendingRequest {
                    sender: new_sender,
                    transport_epoch: 1,
                },
            );
        }

        assert!(take_pending_response_sender(&mut domain.inner.write(), 2, 0).is_none());
        assert!(domain.inner.read().pending_requests.contains_key(&2));

        let stale_requests = drain_pending_requests_for_epoch(&mut domain.inner.write(), 0);
        drop(stale_requests);
        assert!(!domain.inner.read().pending_requests.contains_key(&1));
        assert!(domain.inner.read().pending_requests.contains_key(&2));
        assert!(!new_receiver.is_closed());
    }
    use std::io::Cursor;

    /// A loopback transport that answers every request with one fixed response
    /// body, so client-side response handling can be exercised without a live
    /// daemon.
    struct ScriptedServer {
        inbound: Vec<u8>,
        outbound: std::collections::VecDeque<u8>,
        reply: ResponseBody,
    }

    impl ScriptedServer {
        fn new(reply: ResponseBody) -> Self {
            Self {
                inbound: Vec::new(),
                outbound: std::collections::VecDeque::new(),
                reply,
            }
        }

        fn invalid_data(error: impl std::fmt::Display) -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        }
    }

    impl std::io::Write for ScriptedServer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inbound.extend_from_slice(buf);
            while let Some((length, header_len)) =
                parse_len_prefix(&self.inbound).map_err(Self::invalid_data)?
            {
                let payload_len = check_frame_len(length).map_err(Self::invalid_data)?;
                let total_len = header_len
                    .checked_add(payload_len)
                    .ok_or_else(|| Self::invalid_data("frame length overflow"))?;
                if self.inbound.len() < total_len {
                    break;
                }
                let request_frame: Vec<u8> = self.inbound.drain(0..total_len).collect();
                let (envelope, _) =
                    mux_protocol::unframe(&request_frame).map_err(Self::invalid_data)?;
                let Some(EnvelopePayload::Request(request)) = envelope.payload else {
                    continue;
                };
                let response = Envelope {
                    version: Some(PROTOCOL_VERSION),
                    payload: Some(EnvelopePayload::Response(Response {
                        request_id: request.request_id,
                        body: Some(self.reply.clone()),
                    })),
                };
                self.outbound
                    .extend(frame(&response).map_err(Self::invalid_data)?);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Read for ScriptedServer {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.outbound.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "no reply queued",
                ));
            }
            let mut written = 0;
            for slot in buf.iter_mut() {
                match self.outbound.pop_front() {
                    Some(byte) => {
                        *slot = byte;
                        written += 1;
                    }
                    None => break,
                }
            }
            Ok(written)
        }
    }

    fn scripted_domain(reply: ResponseBody) -> MuxDomain {
        match MuxDomain::connect_with_blocking_stream(ScriptedServer::new(reply)) {
            Ok(domain) => domain,
            Err(error) => panic!("scripted domain: {error}"),
        }
    }

    /// §15.4 A dead transport has to be observable, and the notification
    /// stream cannot do it: the subscriber channel is owned by `DomainInner`,
    /// so it stays open after the I/O thread exits and `recv()` simply blocks
    /// forever. `check_connection` is the probe that does report the outage —
    /// if it ever stopped doing so, a remote window would freeze in silence.
    #[test]
    fn dead_transport_is_observable_only_through_check_connection() {
        // An empty cursor returns Ok(0) on read, which the frame reader turns
        // into UnexpectedEof — the same path a dropped SSH tunnel takes.
        let domain = match MuxDomain::connect_with_blocking_stream(std::io::Cursor::new(Vec::new()))
        {
            Ok(domain) => domain,
            Err(error) => panic!("dead-stream domain: {error}"),
        };
        let notifications = domain.subscribe();
        assert!(
            !smol::block_on(domain.check_connection()),
            "check_connection must report a dead transport"
        );
        assert_eq!(
            notifications.try_recv().err(),
            Some(async_channel::TryRecvError::Empty),
            "the notification stream must not be the disconnect signal"
        );
    }

    /// Requests whose response body was previously discarded must still fail
    /// when the server refuses them — a `ResponseBody::Error` is not a success.
    #[test]
    fn empty_response_requests_surface_server_errors() {
        for description in ["detach", "zoom_pane", "subscribe_pane_output", "shutdown"] {
            let domain = scripted_domain(ResponseBody::Error(
                "permission denied: read-write required".to_string(),
            ));
            let result = smol::block_on(async {
                match description {
                    "detach" => domain.detach().await,
                    "zoom_pane" => domain.zoom_pane("pane-1", true).await,
                    "subscribe_pane_output" => domain.subscribe_pane_output("pane-1").await,
                    _ => domain.shutdown().await,
                }
            });
            let error = match result {
                Ok(()) => panic!("{description} must not report success on a server error"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("permission denied"),
                "{description} lost the server error: {error}"
            );
        }
    }

    #[test]
    fn empty_response_requests_accept_success_bodies() {
        let domain = scripted_domain(ResponseBody::Error(String::new()));
        assert!(smol::block_on(domain.detach()).is_ok());

        let domain = scripted_domain(ResponseBody::ZoomPane(mux_protocol::ZoomPaneResponse {}));
        assert!(smol::block_on(domain.zoom_pane("pane-1", true)).is_ok());

        let domain = scripted_domain(ResponseBody::SubscribePaneOutput(
            mux_protocol::SubscribePaneOutputResponse {},
        ));
        assert!(smol::block_on(domain.subscribe_pane_output("pane-1")).is_ok());
    }

    #[test]
    fn frame_reader_rejects_oversized_prefix_before_payload_read() {
        let mut prefix = Vec::new();
        let mut length = (mux_protocol::MAX_FRAME_PAYLOAD as u64) + 1;
        while length >= 0x80 {
            prefix.push((length as u8 & 0x7f) | 0x80);
            length >>= 7;
        }
        prefix.push(length as u8);

        let mut stream = Cursor::new(Vec::new());
        let error = MuxDomain::read_next_frame_generic(&mut stream, &mut prefix)
            .expect_err("oversized frame prefix must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_reader_rejects_overlong_prefix() {
        let mut buffer = vec![0x80; mux_protocol::MAX_VARINT_LEN];
        let mut stream = Cursor::new(Vec::new());

        let error = MuxDomain::read_next_frame_generic(&mut stream, &mut buffer)
            .expect_err("overlong frame prefix must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_reader_reports_eof_before_header() {
        let mut buffer = Vec::new();
        let mut stream = Cursor::new(Vec::new());

        let error = MuxDomain::read_next_frame_generic(&mut stream, &mut buffer)
            .expect_err("peer eof before a frame must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frame_reader_reports_eof_during_payload() {
        let mut buffer = vec![3, b'a'];
        let mut stream = Cursor::new(Vec::new());

        let error = MuxDomain::read_next_frame_generic(&mut stream, &mut buffer)
            .expect_err("mid-payload eof must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frame_reader_drains_complete_frame() {
        let mut buffer = vec![2, b'a', b'b', 1, b'c'];
        let mut stream = Cursor::new(Vec::new());

        let frame = MuxDomain::read_next_frame_generic(&mut stream, &mut buffer)
            .expect("read frame")
            .expect("complete frame");
        assert_eq!(frame, vec![2, b'a', b'b']);
        assert_eq!(buffer, vec![1, b'c']);
    }
}

/// §9 通知类型别名。
pub type MuxNotification = Notification;
