//! # mux
//!
//! z3rm mux client crate: connects to mux_server via local socket (or SSH),
//! sends RPC requests, receives notifications, and provides grid sync.
//!
//! 协议版本化（§3.10），基于长度前缀的二进制帧（§9），
//! 请求/响应关联通过 request_id（§9）。

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::sync::Arc;

// §9 从 mux_protocol 导入所有 protobuf 类型。
use mux_protocol::{
    attach_request::AttachMode as AttachMode_,
    request::Body as RequestBody, response::Body as ResponseBody,
    split_node::SplitDirection, envelope::Payload as EnvelopePayload,
    frame, check_frame_len, parse_len_prefix, Envelope, Notification, PROTOCOL_VERSION,
    Request, Response, SessionInfo, TerminalSize, FetchGridUpdateResponse,
    FetchScrollbackResponse, AttachResponse, ShellCommand, ShellIntegrationResponse,
    notification::Event as NotifEvent, SessionLayoutChanged,
};

// §16.6 SSH 远程连接模块（Plan 19）。
#[cfg(feature = "ssh")]
mod ssh;
#[cfg(feature = "ssh")]
mod remote_install;
mod sync;

#[cfg(feature = "ssh")]
pub use ssh::{connect_ssh, SshConnectionOptions, SshSession};
#[cfg(feature = "ssh")]
pub use remote_install::{ensure_remote_server, auto_install_server};
pub use sync::sync_extensions_to_remote;

// §9 公共类型导出
pub use mux_protocol::attach_request::AttachMode;
// ============================================================================
// §9 MuxDomain: mux client 核心结构体
// ============================================================================

/// Mux 客户端域：连接到 mux_server，发送 RPC 请求，接收通知。
pub struct MuxDomain {
    inner: Arc<parking_lot::RwLock<DomainInner>>,
    /// §9 窗口 ID (多窗口支持，Plan 32)
    pub window_id: String,
    /// §15.7 Last session successfully attached by this domain. Used by native
    /// KillSession keybindings so the GUI targets the attached session rather
    /// than an arbitrary `list_sessions().first()`.
    last_attached_session_id: parking_lot::RwLock<Option<String>>,
}
/// §9 内部状态：请求 ID 计数器、待处理请求、订阅者列表、写通道。
struct DomainInner {
    next_request_id: AtomicU64,
    pending_requests: HashMap<u64, async_channel::Sender<Response>>,
    /// §9 通知订阅者列表。subscribe() 添加新 sender, 路由器 fan-out 到所有。
    subscribers: Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>>,
    write_tx: std::sync::mpsc::Sender<Vec<u8>>,
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
pub async fn connect_local(socket_path: Option<&Path>) -> Result<MuxDomain> {
    let path = match socket_path {
        Some(p) => p.to_path_buf(),
        None => default_socket_path(),
    };
    // §3.2 / §15.3 跨平台连接: Unix 用 UnixStream (non-blocking), Windows 用 interprocess named pipe。
    let try_connect = || -> Option<Result<MuxDomain>> {
        let p = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("mux-connect".into())
            .spawn(move || {
                #[cfg(unix)]
                let result = {
                    use std::os::unix::net::UnixStream;
                    match UnixStream::connect(&p) {
                        Ok(stream) => {
                            if let Err(e) = stream.set_nonblocking(true) {
                                Err(anyhow::anyhow!(e))
                            } else {
                                MuxDomain::connect_with_blocking_stream(stream)
                            }
                        }
                        Err(e) => Err(anyhow::anyhow!(e)),
                    }
                };
                #[cfg(windows)]
                let result = {
                    use interprocess::local_socket::{prelude::*, GenericNamespaced, Stream as LocalSocketStream};
                    // Windows named pipe: \\.\pipe\z3rm-mux
                    let pipe_name = p.to_string_lossy().to_string();
                    let name = pipe_name
                        .to_ns_name::<GenericNamespaced>()
                        .map_err(|e| anyhow::anyhow!("invalid pipe name: {}", e))?;
                    match LocalSocketStream::connect(name) {
                        Ok(stream) => MuxDomain::connect_with_stream(stream),
                        Err(e) => Err(anyhow::anyhow!(e)),
                    }
                };
                let _ = tx.send(result);
            })
            .ok()?;
        rx.recv().ok().map(|r: Result<MuxDomain>| r.and_then(|d| Ok(d)))
    };
    if let Some(result) = try_connect() {
        match result {
            Ok(domain) => return Ok(domain),
            Err(e) => {
                let msg = format!("{}", e);
                #[cfg(unix)]
                if msg.contains("111") || msg.contains("Connection refused") {
                    tracing::warn!(path = %path.display(), "stale socket (111), cleaning and retrying");
                    if let Err(e) = std::fs::remove_file(&path) { tracing::warn!(error = %e, "remove_file failed"); }
                    if let Some(retry) = try_connect() {
                        return retry;
                    }
                    return Err(anyhow::anyhow!(
                        "connect_local retry failed after cleaning stale socket at {}",
                        path.display()
                    ));
                }
                return Err(anyhow::anyhow!("connect failed: {}", msg));
            }
        }
    }
    anyhow::bail!("connect_local: failed to spawn connection thread to {}", path.display())
}


/// §16.1 默认 socket 路径 (与 mux_server 对齐)。
fn default_socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("Z3RM_MUX_SOCKET") {
        return std::path::PathBuf::from(p);
    }
    #[cfg(unix)]
    {
        let runtime_dir =
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(runtime_dir).join("z3rm").join("mux.sock")
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(r"\\.\pipe\z3rm-mux")
    }
}

trait ReadWrite: std::io::Read + std::io::Write {}
impl<T: std::io::Read + std::io::Write> ReadWrite for T {}

/// §15.4 Open a fresh local socket and return the live byte stream, without
/// spawning the I/O thread. Used by `MuxDomain::reconnect_local_in_place` so
/// the new I/O thread can be bound to an existing `Arc<RwLock<DomainInner>>`
/// rather than a freshly-created one. Mirrors the stale-socket retry that
/// `connect_local` performs.
fn connect_local_stream(
    socket_path: Option<&Path>,
) -> Result<Box<dyn ReadWrite + Send>> {
    let path = match socket_path {
        Some(p) => p.to_path_buf(),
        None => default_socket_path(),
    };

    #[cfg(unix)]
    fn open(path: &std::path::Path) -> Result<Box<dyn ReadWrite + Send>> {
        use std::os::unix::net::UnixStream;
        let connect = || -> Result<UnixStream> {
            let stream = UnixStream::connect(path)
                .map_err(|e| anyhow::anyhow!("connect failed: {}", e))?;
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
                    if let Err(e) = std::fs::remove_file(path) { tracing::warn!(error = %e, "remove_file failed"); }
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
        use interprocess::local_socket::{prelude::*, GenericNamespaced, Stream as LocalSocketStream};
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
    pub fn connect_with_stream(
        stream: interprocess::local_socket::Stream,
    ) -> Result<Self> {
        let (write_tx, write_rx) = std::sync::mpsc::channel();

        let subscribers: Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        let inner = Arc::new(parking_lot::RwLock::new(DomainInner {
            next_request_id: AtomicU64::new(1),
            pending_requests: HashMap::new(),
            subscribers: subscribers.clone(),
            write_tx,
        }));

        let io_inner = inner.clone();
        let io_subscribers = subscribers.clone();
        std::thread::Builder::new()
            .name("mux-io".into())
            .spawn(move || {
                Self::io_and_router_loop(stream, write_rx, io_inner, io_subscribers);
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn mux I/O thread: {}", e))?;

        let window_id = format!("win-{}", std::process::id());
        Ok(MuxDomain {
            inner,
            window_id,
            last_attached_session_id: parking_lot::RwLock::new(None),
        })
    }

    /// Connect using any blocking Read+Write stream (e.g., UnixStream with non-blocking set).
    pub fn connect_with_blocking_stream<S: std::io::Read + std::io::Write + Send + 'static>(
        stream: S,
    ) -> Result<Self> {
        let (write_tx, write_rx) = std::sync::mpsc::channel();
        let subscribers: Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let inner = Arc::new(parking_lot::RwLock::new(DomainInner {
            next_request_id: AtomicU64::new(1),
            pending_requests: HashMap::new(),
            subscribers: subscribers.clone(),
            write_tx,
        }));
        let io_inner = inner.clone();
        let io_subscribers = subscribers.clone();
        std::thread::Builder::new()
            .name("mux-io".into())
            .spawn(move || {
                Self::io_and_router_loop(stream, write_rx, io_inner, io_subscribers);
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn mux I/O thread: {}", e))?;
        let window_id = format!("win-{}", std::process::id());
        Ok(MuxDomain {
            inner,
            window_id,
            last_attached_session_id: parking_lot::RwLock::new(None),
        })
    }

    pub async fn connect(_transport: MuxTransport) -> Result<Self> {
        Err(anyhow::anyhow!(
            "connect() with MuxTransport not yet supported; use connect_local()"
        ))
    }

    fn io_and_router_loop<S: std::io::Read + std::io::Write + Send + 'static>(
        mut stream: S,
        write_rx: std::sync::mpsc::Receiver<Vec<u8>>,
        inner: Arc<parking_lot::RwLock<DomainInner>>,
        subscribers: Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>>,
    ) {
        let mut buf = Vec::new();

        let mut exit = false;
        'outer: loop {
            // §9 轮询写通道（非阻塞）
            loop {
                match write_rx.try_recv() {
                    Ok(framed) => {
                        if stream.write_all(&framed).is_err() {
                            tracing::error!("socket write error");
                            exit = true;
                            break 'outer;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        exit = true;
                        break 'outer;
                    }
                }
            }

            // §9 读取下一帧
            match Self::read_next_frame_generic(&mut stream, &mut buf) {
                Ok(Some(framed)) => {
                    let envelope = match mux_protocol::unframe(&framed) {
                        Ok((env, _)) => env,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to decode envelope");
                            exit = true;
                            break 'outer;
                        }
                    };

                    match envelope.payload {
                        Some(EnvelopePayload::Response(resp)) => {
                            let sender = inner.write().pending_requests.remove(&resp.request_id);
                            if let Some(tx) = sender {
                                let _ = tx.try_send(resp);
                            }
                        }
                        Some(EnvelopePayload::Notification(notif)) => {
                            let mut subs = subscribers.lock();
                            subs.retain(|tx| !tx.is_closed());
                            let is_lifecycle = matches!(
                                notif.event,
                                Some(NotifEvent::PaneAdded(_))
                                    | Some(NotifEvent::PaneRemoved(_))
                                    | Some(NotifEvent::SessionLayoutChanged(_))
                                    | Some(NotifEvent::PaneZoomed(_))
                                    | Some(NotifEvent::PaneTitleChanged(_))
                                    | Some(NotifEvent::PaneBell(_))
                            );
                            for tx in subs.iter() {
                                if is_lifecycle {
                                    // §3.4 at-least-once: block rather than drop lifecycle.
                                    // The I/O thread is dedicated; a slow GUI may stall
                                    // briefly but must not lose PaneRemoved.
                                    if tx.send_blocking(notif.clone()).is_err() {
                                        // closed
                                    }
                                } else if tx.try_send(notif.clone()).is_err() {
                                    // PaneDirty is at-most-once / lossy under pressure.
                                }
                            }
                        }
                        Some(EnvelopePayload::Request(_)) => {
                            tracing::trace!("unexpected request from server");
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
                    exit = true;
                    break 'outer;
                }
            }
        }
        let _ = exit;
        // §9 传输断开: 清空所有 pending_requests 的 sender, 让等待中的
        // send_request 调用 rx.recv() 立即收到 Closed -> 快速返回
        // "connection closed" 而非干等 15s 超时。这是 steal / 断连后写
        let pending: Vec<async_channel::Sender<Response>> = {
            let mut inner = inner.write();
            inner.pending_requests.drain().map(|(_, tx)| tx).collect()
        };
        for tx in pending {
            // 关闭 sender; 对端 recv() 返回 RecvError::Closed。
            drop(tx);
        }
    }


    /// Generic frame reader for any Read+Write stream.
    fn read_next_frame_generic<S: std::io::Read + std::io::Write>(
        stream: &mut S,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let (frame_len, header_len) = loop {
            if let Some((len, header_len)) = Self::try_parse_frame_header(buf)? {
                break (len, header_len);
            }

            let mut read_buf = [0u8; 256];
            match stream.read(&mut read_buf) {
                Ok(0) if buf.is_empty() => return Ok(None),
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed while reading frame header",
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
        self.inner.read().next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// §9 发送请求并等待响应（§16.6 公开供扩展安装使用）。
    pub async fn send_request(&self, body: RequestBody) -> Result<Response> {
        let request_id = self.next_request_id();
        let (tx, rx) = async_channel::bounded(1);

        {
            let mut inner = self.inner.write();
            inner.pending_requests.insert(request_id, tx);
        }

        let request = Request {
            request_id,
            body: Some(body),
        };
        let envelope = Envelope {
            version: Some(PROTOCOL_VERSION),
            payload: Some(EnvelopePayload::Request(request)),
        };
        let framed = frame(&envelope)?;

        self.inner
            .read()
            .write_tx
            .send(framed)
            .map_err(|e| anyhow::anyhow!("write channel error: {}", e))?;

        // §9 等待响应: 异步 await, 不阻塞 executor worker 线程。
        // 15s 上限: 足够慢 SSH 隧道往返, 又能在真正 hang 时快速失败。
        // I/O 线程退出时会清空 pending_requests, 此时 recv() 立刻返回
        // Closed -> 调用方无需等待 15s 即可拿到 "connection closed" 错误。
        match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
            Ok(Ok(resp)) => {
                if let Some(ResponseBody::Error(err)) = &resp.body {
                    if !err.is_empty() {
                        self.inner.write().pending_requests.remove(&request_id);
                        return Err(anyhow::anyhow!("mux server error: {}", err));
                    }
                }
                Ok(resp)
            }
            Ok(Err(_)) => {
                self.inner.write().pending_requests.remove(&request_id);
                Err(anyhow::anyhow!("connection closed"))
            }
            Err(_) => {
                self.inner.write().pending_requests.remove(&request_id);
                Err(anyhow::anyhow!("request timeout"))
            }
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
            _ => Err(anyhow::anyhow!("unexpected response type for list_sessions")),
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
            _ => Err(anyhow::anyhow!("unexpected response type for create_session")),
        }
    }

    /// §3.10 结束指定会话。
    pub async fn kill_session(&self, id: &str) -> Result<()> {
        let req = RequestBody::KillSession(mux_protocol::KillSessionRequest {
            id: id.to_string(),
        });
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    /// §3.5 Request an explicit mux_server process shutdown.
    pub async fn shutdown(&self) -> Result<()> {
        self.send_request(RequestBody::Shutdown(
            mux_protocol::ShutdownRequest {},
        ))
        .await?;
        Ok(())
    }

    /// §3.10 重命名会话。
    pub async fn rename_session(&self, id: &str, name: &str) -> Result<()> {
        let req = RequestBody::RenameSession(mux_protocol::RenameSessionRequest {
            id: id.to_string(),
            name: name.to_string(),
        });
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    // ========================================================================
    // §3.3 窗口管理方法 (多窗口支持，Plan 32)
    // ========================================================================

    /// §3.3 连接到会话并注册窗口 ID。
    pub async fn attach_with_window(&self, session_id: &str) -> Result<AttachResponse> {
        let req = RequestBody::Attach(mux_protocol::AttachRequest {
            session_id: session_id.to_string(),
            mode: AttachMode::Shared as i32,
            window_id: self.window_id.clone(),
            identity: None,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Attach(r)) => Ok(r),
            _ => Err(anyhow::anyhow!("unexpected response type for attach")),
        }
    }

    /// §3.3 在指定会话中创建新窗口，返回窗口 ID。
    pub async fn create_window(&self, session_id: &str) -> Result<String> {
        let req = RequestBody::NewWindow(mux_protocol::NewWindowRequest {
            session_id: session_id.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::NewWindow(r)) => Ok(r.window_id),
            _ => Err(anyhow::anyhow!("unexpected response type for create_window")),
        }
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
        let req = RequestBody::SpawnPane(mux_protocol::SpawnPaneRequest {
            session_id: session.to_string(),
            tab_id: tab.to_string(),
            size: Some(size),
            command,
            cwd: cwd.map(|p| p.to_string_lossy().to_string()),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::PaneId(id)) => Ok(id),
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
        let req = RequestBody::SplitPane(mux_protocol::SplitPaneRequest {
            pane_id: pane.to_string(),
            direction: direction as i32,
            command,
            cwd: None,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::PaneId(id)) => Ok(id),
            _ => Err(anyhow::anyhow!("unexpected response type for split_pane")),
        }
    }

    /// §3.10 关闭 Pane。
        pub async fn close_pane(&self, pane: &str) -> Result<()> {
        let req = RequestBody::ClosePane(mux_protocol::ClosePaneRequest {
            pane_id: pane.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Error(msg)) if !msg.is_empty() => {
                Err(anyhow::anyhow!(msg))
            }
            _ => Ok(()),
        }
    }

    /// §3.10 聚焦 Pane。
    pub async fn focus_pane(&self, pane: &str) -> Result<()> {
        let req = RequestBody::FocusPane(mux_protocol::FocusPaneRequest {
            pane_id: pane.to_string(),
        });
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    /// §3.10 调整 Pane 尺寸。
    pub async fn resize_pane(&self, pane: &str, cols: u32, rows: u32) -> Result<()> {
        let req = RequestBody::ResizePane(mux_protocol::ResizePaneRequest {
            pane_id: pane.to_string(),
            cols,
            rows,
        });
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    /// §3.10 设置 Pane 标题。
    pub async fn set_pane_title(&self, pane: &str, title: &str) -> Result<()> {
        let req = RequestBody::SetPaneTitle(mux_protocol::SetPaneTitleRequest {
            pane_id: pane.to_string(),
            title: title.to_string(),
        });
        let _resp = self.send_request(req).await?;
        Ok(())
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
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Error(msg)) if !msg.is_empty() => {
                Err(anyhow::anyhow!(msg))
            }
            _ => Ok(()),
        }
    }

    /// §3.10 向 Pane 粘贴文本。
    pub async fn paste(&self, pane: &str, text: &str) -> Result<()> {
        let req = RequestBody::Paste(mux_protocol::PasteRequest {
            pane_id: pane.to_string(),
            text: text.to_string(),
        });
        let _resp = self.send_request(req).await?;
        Ok(())
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
            _ => Err(anyhow::anyhow!("unexpected response type for fetch_grid_update")),
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
            _ => Err(anyhow::anyhow!("unexpected response type for fetch_scrollback")),
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
            window_id: self.window_id.clone(),
            identity: None,
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::Attach(resp)) => {
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

    /// §3.10 断开连接。
    pub async fn detach(&self) -> Result<()> {
        let req = RequestBody::Detach(mux_protocol::DetachRequest {});
        let _resp = self.send_request(req).await?;
        Ok(())
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
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    /// §3.3 查询 Pane 的 shell integration 状态 (cwd + prompt marker)。
    pub async fn get_shell_integration(&self, pane: &str) -> Result<ShellIntegrationResponse> {
        let req = RequestBody::ShellIntegration(mux_protocol::ShellIntegrationRequest {
            pane_id: pane.to_string(),
        });
        let resp = self.send_request(req).await?;
        match resp.body {
            Some(ResponseBody::ShellIntegration(si)) => Ok(si),
            _ => Err(anyhow::anyhow!("unexpected response type for get_shell_integration")),
        }
    }

    /// §3.1 In-place render-path: 订阅 Pane 的 PTY 输出字节流。
    /// 返回空响应确认订阅成功；实际字节通过 subscribe() 通知通道以 PaneOutputChunk 推送。
    pub async fn subscribe_pane_output(&self, pane: &str) -> Result<()> {
        let req = RequestBody::SubscribePaneOutput(mux_protocol::SubscribePaneOutputRequest {
            pane_id: pane.to_string(),
        });
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    // ========================================================================
    // §9 订阅通知（§9）
    // ========================================================================

    pub fn subscribe(&self) -> async_channel::Receiver<Notification> {
        let (tx, rx) = async_channel::bounded(4096);
        self.inner.read().subscribers.lock().push(tx);
        rx
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
    pub fn take_subscribers(
        &self,
    ) -> Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>> {
        let inner = self.inner.write();
        let mut subs_guard = inner.subscribers.lock();
        let taken = std::mem::take(&mut *subs_guard);
        Arc::new(parking_lot::Mutex::new(taken))
    }

    /// Install a previously extracted subscriber list into this domain.
    /// Any pre-existing subscribers are replaced.
    pub fn install_subscribers(
        &self,
        subs: Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>>,
    ) {
        let mut inner = self.inner.write();
        inner.subscribers = subs;
    }

    /// Broadcast a synthetic notification to every subscriber (at-least-once).
    /// Used after reconnect to deliver a SessionLayoutChanged without waiting
    /// for the server to push one.
    pub fn broadcast_notification(&self, notif: Notification) {
        let inner = self.inner.read();
        let mut subs = inner.subscribers.lock();
        subs.retain(|tx| !tx.is_closed());
        for tx in subs.iter() {
            if tx.try_send(notif.clone()).is_err() {
                tracing::debug!("notification subscriber dropped before delivery");
            }
        }
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
    pub async fn reconnect_local_in_place(
        &self,
        session_id: &str,
        attach_mode: AttachMode,
    ) -> Result<()> {
        let preserved = self.take_subscribers();
        let new_write_tx = self.spawn_local_io()?;
        self.reinsert_subscribers(&preserved);
        {
            let mut inner = self.inner.write();
            inner.pending_requests.clear();
            inner.next_request_id = AtomicU64::new(1);
            inner.write_tx = new_write_tx;
        }
        let attach_resp = self.attach(session_id, attach_mode).await?;
        if let Some(snapshot) = attach_resp.snapshot.as_ref() {
            if let Some(layout) = snapshot.layout.as_ref() {
                self.broadcast_notification(Notification {
                    event: Some(NotifEvent::SessionLayoutChanged(
                        SessionLayoutChanged {
                            layout: Some(layout.clone()),
                        },
                    )),
                });
            }
        }
        Ok(())
     }

    /// Open a fresh local socket and spawn a new I/O thread bound to the
    /// existing `self.inner` `Arc<RwLock<DomainInner>>`. Returns the write
    /// channel the I/O thread drains so the caller can install it into the
    /// `DomainInner` as the live transport.
    fn spawn_local_io(&self) -> Result<std::sync::mpsc::Sender<Vec<u8>>> {
        let stream = connect_local_stream(None)?;
        let (write_tx, write_rx) = std::sync::mpsc::channel();
        let io_inner = self.inner.clone();
        let io_subscribers = {
            let guard = self.inner.read();
            guard.subscribers.clone()
        };
        std::thread::Builder::new()
            .name("mux-io".into())
            .spawn(move || {
                Self::io_and_router_loop(stream, write_rx, io_inner, io_subscribers);
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn mux I/O thread: {}", e))?;
        Ok(write_tx)
    }

    /// Reinsert sender handles previously removed by `take_subscribers` back
    /// into `self.inner`'s subscribers `Mutex` so the live I/O thread fans
    /// out to them. Restores continuity for pre-reconnect subscribers
    /// without replacing the subscribers `Arc` (which the I/O thread has
    /// already captured).
    fn reinsert_subscribers(
        &self,
        preserved: &Arc<parking_lot::Mutex<Vec<async_channel::Sender<Notification>>>>,
    ) {
        let mut taken = preserved.lock();
        if taken.is_empty() {
            return;
        }
        let guard = self.inner.read();
        let mut into = guard.subscribers.lock();
        into.append(&mut taken);
    }
}

// ============================================================================
// §9 MuxNotification: 公共通知类型别名
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
