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
use std::sync::Arc;

// §9 从 mux_protocol 导入所有 protobuf 类型。
use mux_protocol::{
    attach_request::AttachMode as AttachMode_,
    request::Body as RequestBody, response::Body as ResponseBody,
    split_node::SplitDirection, envelope::Payload as EnvelopePayload,
    frame, Envelope, Notification, PROTOCOL_VERSION,
    Request, Response, SessionInfo, TerminalSize, FetchGridUpdateResponse,
    FetchScrollbackResponse, AttachResponse, ShellCommand,
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

/// §9 传输层：本地 Unix socket。
pub enum MuxTransport {
    /// §9 本地 Unix socket 连接。
    Local,
}
// ============================================================================
// §9 connect_local: 建立本地 socket 连接
// ============================================================================
/// §9 连接到本地 mux_server。
/// Unix: 通过 std Unix domain socket (非阻塞 I/O)。
/// Windows: TODO named pipe transport (§3.2 spec; not yet wired)。
pub async fn connect_local(socket_path: Option<&Path>) -> Result<MuxDomain> {
    let path = match socket_path {
        Some(p) => p.to_path_buf(),
        None => default_socket_path(),
    };
    // §3.2 连接失败时检查是否 stale socket（os error 111），如是则清理并重试一次。
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
                #[cfg(not(unix))]
                let result: Result<MuxDomain> = Err(anyhow::anyhow!(
                    "Windows named-pipe transport not yet implemented (spec §3.2)"
                ));
                let _ = tx.send(result);
            })
            .ok()?;
        rx.recv().ok().map(|r| r.and_then(|d: MuxDomain| Ok(d)))
    };
    if let Some(result) = try_connect() {
        match result {
            Ok(domain) => return Ok(domain),
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("111") || msg.contains("Connection refused") {
                    tracing::warn!(path = %path.display(), "stale socket (111), cleaning and retrying");
                    let _ = std::fs::remove_file(&path);
                    if let Some(retry) = try_connect() {
                        return retry;
                    }
                    return Err(anyhow::anyhow!(
                        "connect_local retry failed after cleaning stale socket at {}",
                        path.display()
                    ));
                }
                return Err(anyhow::anyhow!("stale connect failed: {}", msg));
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

        loop {
            // §9 轮询写通道（非阻塞）
            loop {
                match write_rx.try_recv() {
                    Ok(framed) => {
                        if stream.write_all(&framed).is_err() {
                            tracing::error!("socket write error");
                            return;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }

            // §9 读取下一帧
            match Self::read_next_frame_generic(&mut stream, &mut buf) {
                Ok(Some(framed)) => {
                    let envelope = match mux_protocol::unframe(&framed) {
                        Ok((env, _)) => env,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to decode envelope");
                            break;
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
                            for tx in subs.iter() {
                                let _ = tx.try_send(notif.clone());
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
                    break;
                }
            }
        }
    }


    /// Generic frame reader for any Read+Write stream
    fn read_next_frame_generic<S: std::io::Read + std::io::Write>(
        stream: &mut S,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let (frame_len, header_len) = match Self::try_parse_frame_header(buf) {
            Some(ok) => ok,
            None => {
                let mut read_buf = [0u8; 256];
                match stream.read(&mut read_buf) {
                    Ok(0) => return Ok(None),
                    Ok(n) => buf.extend_from_slice(&read_buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                    Err(e) => return Err(e),
                }
                match Self::try_parse_frame_header(buf) {
                    Some(ok) => ok,
                    None => return Ok(None),
                }
            }
        };
        let frame_len = frame_len as usize;
        let header_len = header_len as usize;
        let total_len = header_len + frame_len;
        if buf.len() < total_len {
            loop {
                let mut read_buf = [0u8; 256];
                match stream.read(&mut read_buf) {
                    Ok(0) => return Ok(None),
                    Ok(n) => buf.extend_from_slice(&read_buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                    Err(e) => return Err(e),
                }
                if buf.len() >= total_len {
                    break;
                }
            }
        }

        let frame = buf.drain(0..total_len).collect();
        Ok(Some(frame))
    }

    /// §9 尝试从缓冲区解析帧头（varint 长度前缀）。
    fn try_parse_frame_header(buf: &[u8]) -> Option<(u32, usize)> {
        let mut result: u32 = 0;
        let mut shift = 0;
        for (i, &byte) in buf.iter().enumerate() {
            result |= ((byte & 0x7F) as u32) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Some((result, i + 1));
            }
            if shift >= 35 {
                return None;
            }
        }
        None
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

        // §9 等待响应 (30s 超时，轮询实现)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let resp = loop {
            match rx.try_recv() {
                Ok(resp) => break resp,
                Err(async_channel::TryRecvError::Closed) => {
                    return Err(anyhow::anyhow!("connection closed"));
                }
                Err(async_channel::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(anyhow::anyhow!("request timeout"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        };
        if let Some(ResponseBody::Error(err)) = &resp.body {
            if !err.is_empty() {
                return Err(anyhow::anyhow!("mux server error: {}", err));
            }
        }
        Ok(resp)
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
        let req = RequestBody::SplitPane(mux_protocol::SplitPaneRequest {
            pane_id: pane.to_string(),
            direction: direction as i32,
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
        let _resp = self.send_request(req).await?;
        Ok(())
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
        let _resp = self.send_request(req).await?;
        Ok(())
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
            Some(ResponseBody::Attach(resp)) => Ok(resp),
            _ => Err(anyhow::anyhow!("unexpected response type for attach")),
        }
    }

    /// §3.10 断开连接。
    pub async fn detach(&self) -> Result<()> {
        let req = RequestBody::Detach(mux_protocol::DetachRequest {});
        let _resp = self.send_request(req).await?;
        Ok(())
    }

    // ========================================================================
    // §9 订阅通知（§9）
    // ========================================================================

    pub fn subscribe(&self) -> async_channel::Receiver<Notification> {
        let (tx, rx) = async_channel::bounded(256);
        self.inner.read().subscribers.lock().push(tx);
        rx
    }
}

// ============================================================================
// §9 MuxNotification: 公共通知类型别名
// ============================================================================

/// §9 通知类型别名。
pub type MuxNotification = Notification;
