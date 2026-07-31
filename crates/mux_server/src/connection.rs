// §9 Connection 模块 — mux_protocol 消息分发、帧编码/解码、通知广播。
// 每个客户端连接一个 tokio task, 处理请求并推送通知。

use anyhow::Context as _;
use interprocess::local_socket::tokio::Stream as LocalSocketStream;
use mux_protocol::proto::envelope::Payload as EnvelopePayload;
use mux_protocol::proto::fetch_grid_update_response::Update as FetchGridUpdateResponseUpdate;
use mux_protocol::proto::request::Body as RequestBody;
use mux_protocol::proto::response::Body as ResponseBody;
use mux_protocol::{
    FrameLengthError, FrameLengthErrorKind, MAX_VARINT_LEN, check_frame_len, proto::*,
};
use prost::Message;
use sqlez::connection::Connection;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

// §3.3 客户端角色 (Plan 33)
use crate::session::ClientRole;

/// §3.3 将 proto ClientRole 值映射为内部 ClientRole
///
/// 未指定 (CLIENT_ROLE_UNSPECIFIED) 与无法识别的枚举值都必须 fail-closed:
/// 这个整数完全由对端提供, 把它当成 ReadWrite 等于让客户端自己挑权限。
pub fn proto_role_to_client_role(role: i32) -> ClientRole {
    match role {
        1 => ClientRole::ReadOnly,
        2 => ClientRole::ReadWrite,
        3 => ClientRole::Admin,
        _ => ClientRole::ReadOnly,
    }
}

/// §3.3 连接所用 transport 在客户端 attach 之前能提供的信任级别。
///
/// 角色是 attach 时才协商的, attach 之前只能靠 transport 判断。把这个判断
/// 提成一个显式类型, 是为了让新增 transport 的人必须选一个分支, 而不是默默
/// 继承本地 socket 的 Admin 默认值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionTrust {
    /// §9 本地 socket: 0600 ACL 保证对端与 daemon 同 UID, 与 tmux 对自己
    /// socket 的信任模型一致。
    LocalSocket,
    /// 没有对端认证的 transport (§25 resilient UDP、未来的 mTLS)。
    Unauthenticated,
}

impl ConnectionTrust {
    /// §3.3 客户端 attach 时没有声明 identity 的话使用的角色。
    pub fn attach_default_role(self) -> ClientRole {
        match self {
            ConnectionTrust::LocalSocket => ClientRole::Admin,
            ConnectionTrust::Unauthenticated => ClientRole::ReadOnly,
        }
    }
}

/// §3.3 尚未 attach 的连接在某一条请求上拥有的角色。
///
/// 默认 fail-closed 到 ReadOnly; 这里逐条列出的写操作是 tmux 风格的一次性
/// CLI 命令 —— `z3rm kill` / `kill-server` 从不 attach, `send-keys` /
/// `split-window` 等在 `$Z3RM_PANE` 已经指明目标时也会跳过 attach
/// (见 `z3rm::cli::dispatch::resolve_pane_id`)。没有对端认证的 transport
/// 一条都不放行。
pub fn pre_attach_role(trust: ConnectionTrust, body: &RequestBody) -> ClientRole {
    if trust != ConnectionTrust::LocalSocket {
        return ClientRole::ReadOnly;
    }
    match body {
        RequestBody::KillSession(_)
        | RequestBody::Shutdown(_)
        | RequestBody::RenameSession(_)
        | RequestBody::NewWindow(_) => ClientRole::Admin,
        RequestBody::SpawnPane(_)
        | RequestBody::SplitPane(_)
        | RequestBody::ClosePane(_)
        | RequestBody::FocusPane(_)
        | RequestBody::ResizePane(_)
        | RequestBody::ResizeLayout(_)
        | RequestBody::SendInput(_)
        | RequestBody::Paste(_)
        | RequestBody::SetClipboard(_)
        | RequestBody::SetPaneTitle(_)
        | RequestBody::ZoomPane(_)
        | RequestBody::DeclineFileVersion(_) => ClientRole::ReadWrite,
        _ => ClientRole::ReadOnly,
    }
}

/// §3.3 权限检查: 判断角色是否允许执行操作
pub fn check_permission(role: ClientRole, required: ClientRole) -> bool {
    match (role, required) {
        (ClientRole::Admin, _) => true,
        (ClientRole::ReadWrite, ClientRole::ReadWrite)
        | (ClientRole::ReadWrite, ClientRole::ReadOnly) => true,
        (ClientRole::ReadOnly, ClientRole::ReadOnly) => true,
        _ => false,
    }
}

pub fn effective_attach_role(role: ClientRole, mode: crate::session::AttachMode) -> ClientRole {
    match mode {
        crate::session::AttachMode::ReadOnly => ClientRole::ReadOnly,
        crate::session::AttachMode::Shared | crate::session::AttachMode::Steal => role,
    }
}

/// 处理单个客户端连接 (§9)
///
/// 单一 outbound mpsc channel 同时承载 Response 和 Notification:
/// 写循环 (write_handle) 消费 channel, 把 Envelope framed 写回 socket。
/// 这样所有写操作都在同一个 tokio task 内串行化, 避免并发 write 冲突。
pub async fn handle_connection(
    stream: LocalSocketStream,
    sessions: Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: Arc<parking_lot::Mutex<Connection>>,
    clipboard: Arc<crate::clipboard::ServerClipboard>,
    server_settings: Arc<crate::server_settings::ServerSettings>,
    shutdown_state: Arc<crate::ShutdownState>,
) -> anyhow::Result<()> {
    let (reader, writer) = tokio::io::split(stream);

    // §3.3 这个函数的参数类型本身就是信任凭据: 能走到这里的一定是 §9 那个
    // 0600 ACL 的本地 socket。网络 transport 必须走自己的入口并传入
    // `ConnectionTrust::Unauthenticated`。
    let trust = ConnectionTrust::LocalSocket;

    // §9 outbound channel: Response 或 Notification 都走这个
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Envelope>();

    // §3.3 客户端角色: 初始为 None, attach 后设置 (Plan 33)
    let client_role: Arc<parking_lot::Mutex<Option<ClientRole>>> =
        Arc::new(parking_lot::Mutex::new(None));
    // Per-connection client identity — never process-wide alone.
    let connection_client_id: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));
    // Per-connection forward task handles spawned in handle_attach.
    // Tracked so they can be aborted on detach/EOF to prevent the
    // outbound channel from never closing (P0 connection hang).
    let forward_tasks: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let read_handle = {
        let outbound_tx = outbound_tx.clone();
        let sessions = sessions.clone();
        let db = db.clone();
        let clipboard = clipboard.clone();
        let server_settings = server_settings.clone();
        let client_role = client_role.clone();
        let connection_client_id = connection_client_id.clone();
        let shutdown_state = shutdown_state.clone();
        let forward_tasks = forward_tasks.clone();
        tokio::spawn(async move {
            let mut reader = reader;
            let mut first = true;
            loop {
                let envelope = read_envelope(&mut reader).await?;
                // §3.10 协议版本协商: 首个 Envelope 的 version 必须与 server 匹配,
                // 否则回一个 error 并关闭连接, 避免跨 major 版本误解析。
                if first {
                    first = false;
                    if !version_compatible(&envelope.version) {
                        let _ = send_response(
                            &outbound_tx,
                            Response {
                                request_id: 0,
                                body: Some(ResponseBody::Error(
                                    "protocol version mismatch".to_string(),
                                )),
                            },
                        );
                        anyhow::bail!("protocol version mismatch");
                    }
                }
                dispatch_envelope(
                    &envelope,
                    &sessions,
                    &outbound_tx,
                    &db,
                    &clipboard,
                    &server_settings,
                    &client_role,
                    &connection_client_id,
                    &shutdown_state,
                    &forward_tasks,
                    trust,
                )
                .await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        })
    };

    // 丢弃原始 sender, 让 outbound channel 仅由 read_handle 的 clone 持有。
    // 这样 read_handle 退出 (版本不符 / 客户端断开) 时 channel 关闭, 写循环把已排队
    // 的 error 刷出后随之结束, 连接真正关闭; 否则残留 sender 会让写循环永远阻塞。
    drop(outbound_tx);

    // §9 写循环: 消费 outbound channel, framed 写回客户端.
    // §3.5 After a Shutdown ack response is flushed, notify the accept loop so
    // the process exits only once the client has the acknowledgment on the wire.
    let write_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(envelope) = outbound_rx.recv().await {
            let is_shutdown_ack = match &envelope.payload {
                Some(EnvelopePayload::Response(resp))
                    if shutdown_state
                        .requested
                        .load(std::sync::atomic::Ordering::SeqCst)
                        && resp.request_id
                            == shutdown_state
                                .ack_request_id
                                .load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    true
                }
                _ => false,
            };
            if let Ok(framed) = mux_protocol::frame(&envelope) {
                if writer.write_all(&framed).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
                if is_shutdown_ack {
                    tracing::info!(
                        request_id = shutdown_state
                            .ack_request_id
                            .load(std::sync::atomic::Ordering::SeqCst),
                        "mux shutdown ack flushed to client"
                    );
                    shutdown_state.acked.notify_one();
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    // §3.10 read_handle exits on EOF or error, but write_handle blocks on
    // outbound_rx.recv() which never closes because handle_attach clones
    // outbound_tx into session-shared subscriber state. We cannot rely on
    // tokio::join! — use select! so the writer terminates when the reader does.
    wait_for_connection_tasks(read_handle, write_handle, || {
        cleanup_connection_state(&sessions, &connection_client_id, &forward_tasks)
    })
    .await;
    Ok(())
}

async fn wait_for_connection_tasks<Cleanup, CleanupFuture>(
    mut read_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    mut write_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    cleanup: Cleanup,
) where
    Cleanup: FnOnce() -> CleanupFuture,
    CleanupFuture: Future<Output = ()>,
{
    tokio::select! {
        result = &mut read_handle => {
            cleanup().await;
            match tokio::time::timeout(std::time::Duration::from_secs(1), &mut write_handle).await {
                Ok(Ok(Err(error))) => tracing::debug!(%error, "mux writer stopped"),
                Ok(Err(error)) => tracing::warn!(%error, "mux writer task failed"),
                Ok(Ok(Ok(()))) => {}
                Err(_) => {
                    write_handle.abort();
                    if let Err(error) = write_handle.await
                        && !error.is_cancelled()
                    {
                        tracing::warn!(%error, "mux writer task failed during connection cleanup");
                    }
                }
            }
            match result {
                Ok(Err(error)) => tracing::debug!(%error, "mux reader stopped"),
                Err(error) => tracing::warn!(%error, "mux reader task failed"),
                Ok(Ok(())) => {}
            }
        }
        result = &mut write_handle => {
            read_handle.abort();
            match read_handle.await {
                Ok(Err(error)) => tracing::debug!(%error, "mux reader stopped during connection cleanup"),
                Err(error) if !error.is_cancelled() => {
                    tracing::warn!(%error, "mux reader task failed during connection cleanup");
                }
                Ok(Ok(())) | Err(_) => {}
            }
            cleanup().await;
            if let Err(error) = result {
                tracing::warn!(%error, "mux writer task failed");
            }
        }
    }
}

async fn cleanup_connection_state(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    if let Some(client_id) = connection_client_id.lock().clone() {
        let mut sessions = sessions.write();
        // §3.3 A closed socket is the authoritative signal that a GUI window is
        // gone (Plan 32): one window owns one connection, so the window leaves
        // the session here even when the client crashed without detaching.
        let released_windows = unregister_client_from_sessions(&mut sessions, &client_id);
        broadcast_window_removals(&sessions, &released_windows);
    }

    let forward_tasks = forward_tasks.lock().drain(..).collect::<Vec<_>>();
    for handle in &forward_tasks {
        handle.abort();
    }
    for handle in forward_tasks {
        if let Err(error) = handle.await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "mux forwarder task failed during connection cleanup");
        }
    }
}

/// §3.3 一个窗口在某个会话中的注册被释放 (Plan 32)。
///
/// `unregister_client_from_sessions` 只做状态变更并把结果交回调用方, 广播由
/// 调用方决定: `handle_attach` 需要把「同一窗口重新 attach」这种并未真正离开
/// 会话的情况压掉, 断连 / detach 路径则必须原样 fan-out。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleasedWindow {
    pub session_id: String,
    pub window_id: String,
}

fn unregister_client_from_sessions(
    sessions: &mut [crate::session::Session],
    client_id: &str,
) -> Vec<ReleasedWindow> {
    let mut released = Vec::new();
    for session in sessions {
        let claimed_window = session.remove_attached_client(client_id);
        session.remove_lifecycle_subscriber(client_id);
        for pane in session.panes.read().values() {
            pane.remove_subscriber(client_id);
            drop_client_viewport(pane, client_id);
        }
        if let Some(window_id) = claimed_window
            && session.release_window(&window_id)
        {
            released.push(ReleasedWindow {
                session_id: session.id.clone(),
                window_id,
            });
        }
    }
    released
}

/// §3.3 新窗口加入会话的通知 (Plan 32)。
fn window_added_notification(session_id: &str, window_id: &str) -> Notification {
    Notification {
        event: Some(mux_protocol::notification::Event::WindowAdded(
            mux_protocol::WindowAdded {
                window_id: window_id.to_string(),
                session_id: session_id.to_string(),
            },
        )),
    }
}

/// §3.3 窗口离开会话的通知 (Plan 32)。
fn window_removed_notification(session_id: &str, window_id: &str) -> Notification {
    Notification {
        event: Some(mux_protocol::notification::Event::WindowRemoved(
            mux_protocol::WindowRemoved {
                window_id: window_id.to_string(),
                session_id: session_id.to_string(),
            },
        )),
    }
}

/// §3.4 把窗口注销 fan-out 给会话里剩下的每个连接 (at-least-once)。
/// 丢一条 `WindowRemoved` 会在其他窗口留下一个不存在的窗口条目。
fn broadcast_window_removals(sessions: &[crate::session::Session], released: &[ReleasedWindow]) {
    for window in released {
        let Some(session) = sessions
            .iter()
            .find(|session| session.id == window.session_id)
        else {
            continue;
        };
        session.broadcast_lifecycle(window_removed_notification(
            &window.session_id,
            &window.window_id,
        ));
    }
}

/// §16.2 Release a departing client's min-fit constraint. A pane that was
/// clamped by this client grows back, so the failure has to be visible rather
/// than leaving every remaining client stuck at the smallest size.
fn drop_client_viewport(pane: &Arc<crate::pane::Pane>, client_id: &str) {
    if let Err(error) = pane.remove_client_viewport(client_id) {
        tracing::warn!(
            error = %error,
            pane_id = %pane.id,
            %client_id,
            "min-fit resize after client detach failed"
        );
    }
}

/// §3.10 协议版本协商: major 必须一致 (major = 破坏性变更, minor = 兼容新增)。
/// 缺失 version 视为不兼容。
fn version_compatible(version: &Option<ProtocolVersion>) -> bool {
    match version {
        Some(v) => v.major == mux_protocol::PROTOCOL_VERSION.major,
        None => false,
    }
}

/// §9 从 socket 读取长度前缀帧, 解码 Envelope
async fn read_envelope(
    reader: &mut tokio::io::ReadHalf<LocalSocketStream>,
) -> anyhow::Result<Envelope> {
    let mut len: u64 = 0;
    let mut shift: u32 = 0;

    for byte_index in 0..MAX_VARINT_LEN {
        let byte = reader.read_u8().await?;
        if byte & 0x80 == 0 {
            len |= (byte as u64) << shift;
            let len = check_frame_len(len)?;
            let mut data = vec![0u8; len];
            reader.read_exact(&mut data).await?;
            let envelope = Envelope::decode(&data[..])?;
            return Ok(envelope);
        }
        len |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        if byte_index + 1 >= MAX_VARINT_LEN {
            return Err(FrameLengthError(FrameLengthErrorKind::OverlongPrefix).into());
        }
    }

    Err(FrameLengthError(FrameLengthErrorKind::OverlongPrefix).into())
}

/// §9 分发 Envelope 到请求/通知处理器
async fn dispatch_envelope(
    envelope: &Envelope,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    _db: &Arc<parking_lot::Mutex<Connection>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    shutdown_state: &Arc<crate::ShutdownState>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    trust: ConnectionTrust,
) -> anyhow::Result<()> {
    let payload = match &envelope.payload {
        Some(p) => p,
        None => return Ok(()),
    };

    match payload {
        EnvelopePayload::Request(req) => {
            let request_id = req.request_id;
            // dispatch_request 内部会把 Response 通过 outbound_tx 发回,
            // 也可能向 outbound_tx push Notification (用于 attach 等)
            dispatch_request(
                req,
                sessions,
                outbound_tx,
                clipboard,
                server_settings,
                client_role,
                connection_client_id,
                shutdown_state,
                &forward_tasks,
                trust,
            )
            .await?;
            // request_id 仅用于日志, 实际 response 已经在 dispatch_request 内发出
            let _ = request_id;
        }
        EnvelopePayload::Response(_) => {
            tracing::warn!("unexpected Response from client");
        }
        EnvelopePayload::Notification(_) => {
            tracing::warn!("unexpected Notification from client");
        }
    }

    Ok(())
}
/// §9 分发请求到具体处理器, 通过 outbound_tx 把 Response 写回客户端。
async fn dispatch_request(
    req: &Request,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    shutdown_state: &Arc<crate::ShutdownState>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    trust: ConnectionTrust,
) -> anyhow::Result<()> {
    let request_id = req.request_id;

    let body = match &req.body {
        Some(b) => b,
        None => {
            send_response(
                outbound_tx,
                Response {
                    request_id,
                    body: Some(ResponseBody::Error("empty request body".to_string())),
                },
            )?;
            return Ok(());
        }
    };

    // §3.3 客户端角色: attach 时协商; 尚未 attach 的连接由 `pre_attach_role`
    // 按 transport + 请求类型显式放行, 默认落到 ReadOnly (fail-closed)。
    let role = match *client_role.lock() {
        Some(role) => role,
        None => pre_attach_role(trust, body),
    };

    let resp_body = match body {
        // §3.3 无权限要求的操作
        RequestBody::CreateSession(r) => handle_create_session(r, sessions).await?,
        RequestBody::ListSessions(_) => handle_list_sessions(sessions).await?,
        RequestBody::Attach(r) => {
            handle_attach(
                r,
                sessions,
                client_role,
                connection_client_id,
                outbound_tx,
                forward_tasks,
                trust,
            )
            .await?
        }
        RequestBody::Detach(_) => {
            handle_detach(sessions, connection_client_id, forward_tasks).await?
        }
        RequestBody::FetchGridUpdate(r) => handle_fetch_grid_update(r, sessions).await?,
        RequestBody::FetchScrollback(r) => handle_fetch_scrollback(r, sessions).await?,
        RequestBody::SearchScrollback(r) => handle_search_scrollback(r, sessions).await?,
        RequestBody::ListFileVersions(request) => {
            handle_list_file_versions(request, sessions).await?
        }
        RequestBody::GetFileVersion(request) => handle_get_file_version(request, sessions).await?,
        RequestBody::DeclineFileVersion(request) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_decline_file_version(request, sessions).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::Shutdown(_) => {
            if check_permission(role, ClientRole::Admin) {
                // §3.5 Mark shutdown + queue the ack response. The writer loop
                // flushes that response and only then notifies the accept loop,
                // so the client receives the ack before the process exits.
                shutdown_state
                    .requested
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                shutdown_state
                    .ack_request_id
                    .store(request_id, std::sync::atomic::Ordering::SeqCst);
                send_response(
                    outbound_tx,
                    Response {
                        request_id,
                        body: Some(ResponseBody::Error(String::new())),
                    },
                )?;
                tracing::info!(request_id, "mux shutdown response queued for flush-ack");
                return Ok(());
            }
            ResponseBody::Error("permission denied: admin required".to_string())
        }
        RequestBody::GetClipboard(_) => handle_get_clipboard(clipboard).await?,

        // §3.3 Admin-only 操作 (Plan 33)
        RequestBody::KillSession(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_kill_session(r, sessions).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::RenameSession(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_rename_session(r, sessions).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::InstallExtension(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_install_extension(r).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::NewWindow(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_new_window(r, sessions).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }

        // §3.3 需要 ReadWrite 的 pane 操作 (Plan 33)
        RequestBody::SpawnPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_spawn_pane(r, sessions, outbound_tx, server_settings, clipboard).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SplitPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_split_pane(r, sessions, outbound_tx, server_settings, clipboard).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::ClosePane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_close_pane(r, sessions, outbound_tx, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::FocusPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_focus_pane(r, sessions, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::ResizePane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_resize_pane(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::ResizeLayout(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_resize_layout(r, sessions, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }

        // §3.3 需要 ReadWrite 的输入操作 (Plan 33)
        RequestBody::SendInput(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_send_input(r, sessions, clipboard, outbound_tx, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::Paste(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_paste(r, sessions).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SetClipboard(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_set_clipboard(r, sessions, clipboard).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SetPaneTitle(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_set_pane_title(r, sessions).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }

        // §3.3 文件操作:server 在本地或远端文件系统执行。§16.6 GUI file viewer
        // 通过这些 RPC 读文件。路径由 `resolve_session_file_path` 限制在调用方
        // 已 attach 的 session cwd 之内 (§3.2 worktree 根)。
        RequestBody::ReadFile(r) => {
            if check_permission(role, ClientRole::ReadOnly) {
                handle_read_file(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-only required".to_string())
            }
        }
        RequestBody::ListDir(r) => {
            if check_permission(role, ClientRole::ReadOnly) {
                handle_list_dir(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-only required".to_string())
            }
        }
        RequestBody::StatFile(r) => {
            if check_permission(role, ClientRole::ReadOnly) {
                handle_stat_file(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-only required".to_string())
            }
        }

        // §3.3 Pane zoom 和 shell integration
        RequestBody::ZoomPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_zoom_pane(r, sessions, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::ShellIntegration(r) => handle_shell_integration(r, sessions).await?,
        RequestBody::SubscribePaneOutput(_) => {
            // §3.1 In-place render-path: 订阅已由 pane.subscribe 自动注册;
            // 此请求仅确认订阅成功。实际数据通过 PaneOutputChunk 通知推送。
            ResponseBody::SubscribePaneOutput(mux_protocol::SubscribePaneOutputResponse {})
        }
    };

    // §9 把 Response 通过 outbound channel 写回客户端
    send_response(
        outbound_tx,
        Response {
            request_id,
            body: Some(resp_body),
        },
    )?;

    Ok(())
}

/// §9 通过 outbound channel 把 Response 封装成 Envelope 发回客户端。
fn send_response(
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    response: Response,
) -> anyhow::Result<()> {
    let envelope = Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
        payload: Some(EnvelopePayload::Response(response)),
    };
    outbound_tx
        .send(envelope)
        .map_err(|_| anyhow::anyhow!("client disconnected"))
}

async fn handle_create_session(
    req: &CreateSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let id = nanoid::nanoid!();
    let mut session = crate::session::Session::new(id.clone(), req.name.clone(), req.cwd.clone());

    // §16.6 spec 要求:每个新 session 自动创建一个 default tab,
    // 否则客户端 spawn_pane 时没有 tab_id 可用。
    let default_tab_id = "tab-0".to_string();
    session.add_tab(default_tab_id.clone(), req.name.clone());
    session.focused_tab = Some(default_tab_id);

    // §4 Wire shadow_snapshot: start the Monitor + recorder on a blocking
    // thread so the async task is not stalled by inotify setup / SQLite open.
    // The session is registered immediately; the snapshot watch is attached
    // when the background task completes.
    let snapshot_id = id.clone();
    let snapshot_cwd = req.cwd.clone();
    let sessions_for_snapshot = sessions.clone();
    let log_id = snapshot_id.clone();
    let log_cwd = snapshot_cwd.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            crate::snapshot::start(&snapshot_id, &snapshot_cwd)
        })
        .await;
        match result {
            Ok(Ok(Some(watch))) => {
                let mut sessions_w = sessions_for_snapshot.write();
                if let Some(s) = sessions_w.iter_mut().find(|s| s.id == log_id) {
                    s.snapshot_watch = Some(watch);
                }
            }
            Ok(Ok(None)) => {
                zlog::info!(
                    "shadow snapshot not armed: session={} cwd={}",
                    log_id,
                    log_cwd
                );
            }
            Ok(Err(error)) => {
                zlog::warn!(
                    "shadow snapshot start failed: session={} cwd={} error={}",
                    log_id,
                    log_cwd,
                    error
                );
            }
            Err(join_error) => {
                zlog::warn!(
                    "shadow snapshot task panicked: session={} error={}",
                    log_id,
                    join_error
                );
            }
        }
    });

    sessions.write().push(session);

    // §16.12 记录 session 创建事件
    zlog::info!(
        "session created: id={} name={} cwd={}",
        id,
        req.name,
        req.cwd
    );

    Ok(ResponseBody::Session(SessionInfo {
        id,
        name: req.name.clone(),
        cwd: req.cwd.clone(),
        created_timestamp: 0,
        attached_clients: 0,
    }))
}

/// §3.10 列出所有会话
async fn handle_list_sessions(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    let infos: Vec<SessionInfo> = sessions_r
        .iter()
        .map(|s| SessionInfo {
            id: s.id.clone(),
            name: s.name.clone(),
            cwd: s.cwd.clone(),
            created_timestamp: s.created_timestamp,
            attached_clients: s.attached_client_count(),
        })
        .collect();

    Ok(ResponseBody::Sessions(ListSessionsResponse {
        sessions: infos,
    }))
}

/// §3.10 结束会话
async fn handle_kill_session(
    req: &KillSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    if let Some(session) = take_session(sessions, &req.id) {
        if let Some(watch) = session.snapshot_watch.as_ref() {
            watch.stop();
        }
        zlog::info!("session killed: id={}", req.id);
    } else {
        zlog::warn!("kill session not found: id={}", req.id);
    }
    Ok(ResponseBody::Error(String::new()))
}

fn take_session(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
) -> Option<crate::session::Session> {
    let mut sessions = sessions.write();
    let index = sessions
        .iter()
        .position(|session| session.id == session_id)?;
    Some(sessions.remove(index))
}
/// §3.10 连接会话 — 把客户端的 outbound_tx 注册为所有 pane 的 subscriber
async fn handle_attach(
    req: &AttachRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    trust: ConnectionTrust,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    let target_session = sessions_w
        .iter()
        .position(|session| session.id == req.session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.session_id))?;

    zlog::info!(
        "client attached: session={} mode={:?}",
        req.session_id,
        req.mode
    );

    // Prefer an already-assigned per-connection id so re-attach is idempotent
    // for this socket. Mint a connection-scoped id (never process-wide alone).
    let client_id = {
        let mut stored = connection_client_id.lock();
        if let Some(existing) = stored.as_ref() {
            existing.clone()
        } else {
            let minted = if let Some(identity) = &req.identity {
                if !identity.client_id.is_empty() {
                    format!("{}-{}", identity.client_id, nanoid::nanoid!(8))
                } else {
                    format!("client-{}-{}", std::process::id(), nanoid::nanoid!(8))
                }
            } else {
                format!("client-{}-{}", std::process::id(), nanoid::nanoid!(8))
            };
            *stored = Some(minted.clone());
            minted
        }
    };
    let released_windows = unregister_client_from_sessions(&mut sessions_w, &client_id);

    // §3.3 空 window_id 表示对端不是 GUI 窗口 (CLI 一次性命令), 不参与窗口成员
    // 资格。
    let attach_window_id = (!req.window_id.is_empty()).then(|| req.window_id.clone());
    // 同一个窗口重新 attach (§15.4 原地重连 / 幂等 attach) 会先释放再注册, 但它
    // 从未真正离开会话, 所以这一对 WindowRemoved/WindowAdded 都压掉, 只 fan-out
    // 该连接此前占用的其他窗口注册。
    let (reattached_windows, stale_windows): (Vec<ReleasedWindow>, Vec<ReleasedWindow>) =
        released_windows.into_iter().partition(|released| {
            released.session_id == req.session_id
                && Some(released.window_id.as_str()) == attach_window_id.as_deref()
        });
    let reattaching_same_window = !reattached_windows.is_empty();
    broadcast_window_removals(&sessions_w, &stale_windows);

    let session = &mut sessions_w[target_session];

    // §3.3 角色解析: identity 显式声明时以其为准;否则保留既有角色, 首次
    // attach 则退回 transport 的默认角色 (本地 socket = 同 UID = Admin,
    // 无认证 transport = ReadOnly)。ReadOnly attach mode 是会话级写保护,
    // 必须降权整个连接, 否则 attach -r 后续 SendInput 仍会按
    // Admin/ReadWrite 通过。
    let requested_role = if let Some(identity) = &req.identity {
        proto_role_to_client_role(identity.role)
    } else {
        client_role.lock().unwrap_or(trust.attach_default_role())
    };

    let mode = match req.mode {
        0 => crate::session::AttachMode::Shared,
        1 => crate::session::AttachMode::Shared,
        2 => crate::session::AttachMode::Steal,
        3 => crate::session::AttachMode::ReadOnly,
        _ => crate::session::AttachMode::Shared,
    };
    let role = effective_attach_role(requested_role, mode);
    *client_role.lock() = Some(role);
    // The connection-scoped client was removed from every prior session above.
    if mode == crate::session::AttachMode::Steal {
        let kick_notification = Notification {
            event: Some(mux_protocol::notification::Event::SessionLayoutChanged(
                mux_protocol::SessionLayoutChanged {
                    layout: Some(mux_protocol::LayoutTree { root: None }),
                },
            )),
        };
        session.broadcast_lifecycle(kick_notification);
        let mut kicked_clients: HashSet<String> = session
            .attached_clients
            .read()
            .iter()
            .map(|attached| attached.client_id.clone())
            .collect();
        let kicked_windows: Vec<String> = session
            .attached_clients
            .read()
            .iter()
            .filter_map(|attached| attached.window_id.clone())
            .collect();
        session.attached_clients.write().clear();
        // §3.3 抢占踢出的窗口注销必须在清空 lifecycle 订阅之前广播, 否则被踢的
        // 客户端永远收不到「自己已离开会话」这条通知 (Plan 32)。
        for window_id in &kicked_windows {
            if session.release_window(window_id) {
                session
                    .broadcast_lifecycle(window_removed_notification(&req.session_id, window_id));
            }
        }
        kicked_clients.extend(session.clear_lifecycle_subscribers());
        for pane in session.panes.read().values() {
            for kicked_client in &kicked_clients {
                pane.remove_subscriber(kicked_client);
                drop_client_viewport(pane, kicked_client);
            }
        }
    }
    session.add_attached_client(client_id.clone(), mode, role, attach_window_id.clone());

    // §3.4 Register this connection's outbound channel as a session-level
    // lifecycle subscriber. lifecycle_subscribers is keyed by client_id and
    // held by the session; the connection's outbound_tx is closed when its
    // read/write loop exits, after which broadcast_lifecycle prunes it.
    // Re-attach of the same client_id replaces the prior sender idempotently.
    session.add_lifecycle_subscriber(client_id.clone(), outbound_tx.clone());

    // §3.3 窗口在这里才真正加入会话 (Plan 32): 只有 attach 才带连接身份, 也只有
    // 绑定了连接身份的窗口才能在断连时被精确释放。注册在 lifecycle 订阅之后,
    // 这样刚接入的窗口自己也会收到这条 WindowAdded。
    if let Some(window_id) = &attach_window_id
        && session.add_window(window_id.clone())
        && !reattaching_same_window
    {
        session.broadcast_lifecycle(window_added_notification(&req.session_id, window_id));
    }

    // §3.4 把该连接的 outbound_tx 注册为 session 内所有 pane 的 subscriber。
    // 后续 PTY output → bump generation → broadcast PaneDirty → 此连接收到。
    // 我们用一个 helper channel 把 Notification 包成 Envelope 转发到 outbound。
    let notification_forward_tx = outbound_tx.clone();
    let panes = session.panes.clone();
    let panes_r = panes.read();
    for (_id, pane) in panes_r.iter() {
        // 创建一个内层 channel, 把 Notification 转成 Envelope 转发
        let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<Notification>();
        pane.add_subscriber(client_id.clone(), inner_tx);
        // forward task: 把 inner Notification 包成 Envelope 发到 outbound
        let forward_tx = notification_forward_tx.clone();
        let handle = tokio::spawn(async move {
            while let Some(notif) = inner_rx.recv().await {
                let envelope = Envelope {
                    version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
                    payload: Some(EnvelopePayload::Notification(notif)),
                };
                if forward_tx.send(envelope).is_err() {
                    break;
                }
            }
        });
        forward_tasks.lock().push(handle);
    }

    Ok(ResponseBody::Attach(AttachResponse {
        snapshot: Some(session_snapshot(session)),
    }))
}

/// §15.4 权威快照:tabs / layout / focused 必须反映 server 真实状态。
/// 旧实现写死 tabs: Vec::new() 是严重违反 spec §15.4 的 bug。
fn session_snapshot(session: &crate::session::Session) -> SessionSnapshot {
    let tabs = session
        .tabs
        .values()
        .map(|tab| mux_protocol::TabInfo {
            id: tab.id.clone(),
            title: tab.title.clone(),
            panes: tab
                .pane_ids
                .iter()
                .filter_map(|pane_id| pane_info_for(session, pane_id))
                .collect(),
        })
        .collect();

    SessionSnapshot {
        session_id: session.id.clone(),
        focused_pane_id: session.focused_pane.clone().unwrap_or_default(),
        focused_tab_id: session.focused_tab.clone().unwrap_or_default(),
        tabs,
        layout: Some(layout_tree_to_proto(&session.layout)),
    }
}

/// §3.10 断开连接 — remove this connection's client registration.
///
/// Voluntary detach clears `connection_client_id` so subsequent CLI RPCs
/// (send-keys, capture-pane, …) treat the socket as pre-attach again —
/// matching tmux, where a detached client can still drive panes by target.
/// Steal kick leaves the id set without a session membership, so
/// `client_still_attached` keeps failing writers in milliseconds.
async fn handle_detach(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    if let Some(client_id) = connection_client_id.lock().take() {
        // §3.3 Detach retires this connection's window from the session too
        // (Plan 32) — the GUI calls it when its window closes.
        let released_windows = unregister_client_from_sessions(&mut sessions_w, &client_id);
        broadcast_window_removals(&sessions_w, &released_windows);
    }
    // Abort all forward tasks so their outbound_tx clones are dropped,
    // preventing stale PaneDirty/PaneOutput delivery and duplicate
    // subscriber registration on re-attach.
    for handle in forward_tasks.lock().drain(..) {
        handle.abort();
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §3.3 在现有会话中创建新窗口 (Plan 32)

/// §3.4 Register `pane` with every currently attached client's outbound channel
/// so PaneDirty / PaneOutput fan out to the whole session, not only the spawner.

/// §16.6 Install emulator ClipboardStore → ServerClipboard fan-out for a pane.
fn install_pane_clipboard_hook(
    pane: &std::sync::Arc<crate::pane::Pane>,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
) {
    let sessions = Arc::clone(sessions);
    let clipboard = Arc::clone(clipboard);
    pane.set_clipboard_hook(Box::new(move |data: String| {
        let entry = crate::clipboard::ClipboardEntry {
            content_type: crate::clipboard::ClipboardContentType::Text,
            data: data.into_bytes(),
            origin_host: std::env::var("HOSTNAME").unwrap_or_else(|_| "z3rm-server".into()),
        };
        let mut txs = Vec::new();
        for session in sessions.read().iter() {
            for tx in session.lifecycle_subscribers.read().values() {
                txs.push(tx.clone());
            }
        }
        clipboard.set_clipboard(entry, &txs);
    }));
}

fn register_pane_with_session_subscribers(
    session: &crate::session::Session,
    pane: &std::sync::Arc<crate::pane::Pane>,
) {
    let subs: Vec<_> = session
        .lifecycle_subscribers
        .read()
        .iter()
        .map(|(client_id, sender)| (client_id.clone(), sender.clone()))
        .collect();
    for (client_id, outbound_tx) in subs {
        let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<Notification>();
        pane.add_subscriber(client_id, inner_tx);
        let forward_tx = outbound_tx;
        tokio::spawn(async move {
            while let Some(notif) = inner_rx.recv().await {
                let envelope = Envelope {
                    version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
                    payload: Some(EnvelopePayload::Notification(notif)),
                };
                if forward_tx.send(envelope).is_err() {
                    break;
                }
            }
        });
    }
}

/// §3.3 为一个即将打开的 GUI 窗口分配权威窗口 ID (Plan 32)。
///
/// 只分配 ID 并回一份会话快照, 不改动 `connected_windows`: 窗口的成员资格必须
/// 与一个连接绑定 (见 `handle_attach`), 否则 daemon 会留下一个没人认领、断连时
/// 也无从释放的窗口。客户端拿到 ID 后立即用它 attach。
async fn handle_new_window(
    req: &NewWindowRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let window_id = format!("win-{}-{}", std::process::id(), nanoid::nanoid!());

    let sessions_r = sessions.read();
    let session = sessions_r
        .iter()
        .find(|s| s.id == req.session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.session_id))?;
    let snapshot = session_snapshot(session);
    drop(sessions_r);

    // §16.12 记录新窗口创建事件
    zlog::info!(
        "new window id minted: session={} window={}",
        req.session_id,
        window_id
    );

    Ok(ResponseBody::NewWindow(NewWindowResponse {
        window_id,
        snapshot: Some(snapshot),
    }))
}

/// §3.10 创建 pane — 真正 spawn PTY + alacritty Term (server-canonical)
async fn handle_spawn_pane(
    req: &SpawnPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
) -> anyhow::Result<ResponseBody> {
    let pane_id = nanoid::nanoid!();

    // §3.1 转换 ShellCommand → pane::ShellCommand
    let shell_cmd = req.command.as_ref().map(|c| crate::pane::ShellCommand {
        program: c.program.clone(),
        args: c.args.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    });

    // §3.10 解析 cwd (空则用 session.cwd)
    let cwd = {
        let sessions_r = sessions.read();
        sessions_r
            .iter()
            .find(|s| s.id == req.session_id)
            .map(|s| s.cwd.clone())
            .unwrap_or_default()
    };
    let cwd = req.cwd.clone().unwrap_or(cwd);

    // §3.1 解析 size
    let (cols, rows) = req
        .size
        .as_ref()
        .map(|s| (s.cols, s.rows))
        .unwrap_or((80, 24));

    // §16.11 scrollback capacity comes from the live ServerSettings (env +
    // server.json, hot-reloaded) so a daemon-wide change takes effect for the
    // next spawned pane without a restart.
    let scrollback_lines = server_settings.scrollback_lines();

    // §3.1 spawn PTY + alacritty Term
    // §3.4 spawn_with_session 注入 session_id, 让 PTY read loop 在自然退出时
    // 能定位所在会话并 fan-out PaneRemoved。
    let pane = crate::pane::Pane::spawn_with_session(
        pane_id.clone(),
        req.session_id.clone(),
        cwd,
        cols,
        rows,
        shell_cmd,
        scrollback_lines,
    )?;

    // §3.10 把 pane 加入 session 的 panes registry、tab 列表、layout 树。
    // §15.4 attach 返回的权威快照必须反映这些登记。
    {
        let mut sessions_w = sessions.write();
        if let Some(session) = sessions_w.iter_mut().find(|s| s.id == req.session_id) {
            session.panes.write().insert(pane_id.clone(), pane);
            session.set_focused_pane(pane_id.clone());

            // §3.3 / §16.9 把 pane 注册到指定 tab。Tab 不存在则按 id 创建,
            // 防止客户端传入尚未创建的 tab_id 时静默丢弃 pane。
            let tab =
                session
                    .tabs
                    .entry(req.tab_id.clone())
                    .or_insert_with(|| crate::session::Tab {
                        id: req.tab_id.clone(),
                        title: String::new(),
                        pane_ids: Vec::new(),
                    });
            if !tab.pane_ids.contains(&pane_id) {
                tab.pane_ids.push(pane_id.clone());
            }

            // §3.7 在 layout 中登记 pane:第一个 pane 成根,后续通过 split 接入。
            if session.layout.is_empty_root() {
                session.layout = crate::layout::LayoutTree::with_pane(
                    format!("node-{}", pane_id),
                    pane_id.clone(),
                );
            }
        }
    }

    // §3.4 Register the new pane with every attached client's outbound so
    // PaneDirty/PaneOutput reach the whole session (not only the spawner).
    {
        let sessions_r = sessions.read();
        if let Some(session) = sessions_r.iter().find(|s| s.id == req.session_id) {
            if let Some(pane) = session.panes.read().get(&pane_id) {
                register_pane_with_session_subscribers(session, pane);
                install_pane_clipboard_hook(pane, sessions, clipboard);
            }
        }
    }

    zlog::info!("pane spawned: id={} session={}", pane_id, req.session_id);

    // §3.4 Install natural-exit hook before fan-out so a fast-exiting shell
    // still produces PaneRemoved for every attached client.
    {
        let sessions_r = sessions.read();
        if let Some(session) = sessions_r.iter().find(|s| s.id == req.session_id) {
            if let Some(pane) = session.panes.read().get(&pane_id) {
                install_pane_exit_hook(pane, sessions, req.session_id.clone(), pane_id.clone());
            }
        }
    }

    // §3.4 fan-out PaneAdded 到所有 attached 客户端 (session 级 lifecycle 路径,
    // at-least-once: 每个 attached 连接的 outbound channel 都收一份, 不只是发起方)。
    broadcast_pane_added(sessions, &req.session_id, &pane_id, &req.tab_id);

    Ok(ResponseBody::PaneId(pane_id))
}

/// §3.10 Split an existing pane and optionally run a command in the new pane.
async fn handle_split_pane(
    req: &SplitPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
) -> anyhow::Result<ResponseBody> {
    let direction = match req.direction {
        1 => crate::layout::SplitDirection::LeftRight,
        2 => crate::layout::SplitDirection::TopBottom,
        _ => crate::layout::SplitDirection::LeftRight,
    };
    let new_pane_id = nanoid::nanoid!();

    let mut sessions_w = sessions.write();
    for session in sessions_w.iter_mut() {
        if session.layout.root.find_pane(&req.pane_id).is_some() {
            session
                .layout
                .split(&req.pane_id, new_pane_id.clone(), direction)?;

            let parent_cwd = session
                .panes
                .read()
                .get(&req.pane_id)
                .map(|pane| pane.get_cwd())
                .unwrap_or_default();
            let parent_cols = session
                .panes
                .read()
                .get(&req.pane_id)
                .map(|pane| pane.get_cols())
                .unwrap_or(80);
            let parent_rows = session
                .panes
                .read()
                .get(&req.pane_id)
                .map(|pane| pane.get_rows())
                .unwrap_or(24);
            let cwd = req
                .cwd
                .clone()
                .filter(|cwd| !cwd.is_empty())
                .unwrap_or(parent_cwd);
            let command = req
                .command
                .as_ref()
                .map(|command| crate::pane::ShellCommand {
                    program: command.program.clone(),
                    args: command.args.clone(),
                    env: command.env.clone().into_iter().collect(),
                });
            let pane = crate::pane::Pane::spawn_with_session(
                new_pane_id.clone(),
                session.id.clone(),
                cwd,
                parent_cols,
                parent_rows,
                command,
                // §16.11 honor live ServerSettings scrollback (env + server.json).
                server_settings.scrollback_lines(),
            )?;
            session.panes.write().insert(new_pane_id.clone(), pane);
            session.set_focused_pane(new_pane_id.clone());

            let parent_tab_id = session
                .tabs
                .iter()
                .find(|(_, tab)| tab.pane_ids.contains(&req.pane_id))
                .map(|(id, _)| id.clone());
            let parent_tab_id_for_broadcast = parent_tab_id.clone().unwrap_or_default();
            if let Some(tab_id) = parent_tab_id {
                if let Some(tab) = session.tabs.get_mut(&tab_id) {
                    if !tab.pane_ids.contains(&new_pane_id) {
                        tab.pane_ids.push(new_pane_id.clone());
                    }
                }
            }

            let pane_ref = session.panes.read().get(&new_pane_id).cloned();
            if let Some(pane) = pane_ref {
                // Bind split pane to this session so exit hooks / env resolve.
                pane.set_session_id(session.id.clone());
                register_pane_with_session_subscribers(session, &pane);
                install_pane_clipboard_hook(&pane, sessions, clipboard);
            }
            // §3.4 fan-out PaneAdded + 更新后 layout 到所有 attached 客户端。
            // 写入 session 在 scopes 结束 (sessions_w 被 drop) 后释放,
            // 由 lifecycle helper 重新获取读锁单次发送, 避免嵌套写锁的死锁。
            let session_id_for_broadcast = session.id.clone();
            drop(sessions_w);
            {
                let sessions_r = sessions.read();
                if let Some(session) = sessions_r.iter().find(|s| s.id == session_id_for_broadcast)
                {
                    if let Some(pane) = session.panes.read().get(&new_pane_id) {
                        install_pane_exit_hook(
                            pane,
                            sessions,
                            session_id_for_broadcast.clone(),
                            new_pane_id.clone(),
                        );
                    }
                }
            }
            broadcast_pane_added(
                sessions,
                &session_id_for_broadcast,
                &new_pane_id,
                &parent_tab_id_for_broadcast,
            );
            broadcast_layout_changed(sessions, &session_id_for_broadcast);
            return Ok(ResponseBody::PaneId(new_pane_id));
        }
    }

    Ok(ResponseBody::Error("pane not found".into()))
}

/// §3.10 关闭 pane
async fn handle_close_pane(
    req: &ClosePaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    _outbound_tx: &mpsc::UnboundedSender<Envelope>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if !client_still_attached(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(
            "client not attached (kicked or detached)".to_string(),
        ));
    }

    let mut removed = false;
    let mut session_id = None;
    {
        let mut sessions_w = sessions.write();
        for session in sessions_w.iter_mut() {
            let had = session.panes.write().remove(&req.pane_id).is_some();
            if had {
                removed = true;
                session_id = Some(session.id.clone());
                detach_pane_from_layout(session, &req.pane_id);
                zlog::info!("pane closed: id={}", req.pane_id);
                break;
            }
        }
    }
    if !removed {
        return Ok(ResponseBody::Error(format!(
            "pane not found: {}",
            req.pane_id
        )));
    }
    if let Some(sid) = session_id {
        broadcast_layout_changed(sessions, &sid);
        broadcast_pane_removed(sessions, &sid, &req.pane_id, 0);
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 聚焦 pane
async fn handle_focus_pane(
    req: &FocusPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    _outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    let session_id = {
        let mut sessions = sessions.write();
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.layout.root.find_pane(&req.pane_id).is_some())
        else {
            return Ok(ResponseBody::Error(format!(
                "pane not found: {}",
                req.pane_id
            )));
        };
        session.set_focused_pane(req.pane_id.clone());
        session.id.clone()
    };
    broadcast_lifecycle_in_session(
        sessions,
        &session_id,
        Notification {
            event: Some(mux_protocol::notification::Event::PaneFocused(
                mux_protocol::PaneFocused {
                    pane_id: req.pane_id.clone(),
                },
            )),
        },
    );
    broadcast_layout_changed(sessions, &session_id);
    Ok(ResponseBody::Error(String::new()))
}
/// §3.10 调整 pane 尺寸 — 真正调用 pane.resize (PTY TIOCSWINSZ + alacritty)
///
/// §16.2 An attached client's resize is a report of *its* viewport, not an
/// authoritative pane size: the pane takes the min-fit across every attached
/// client so the smallest one still sees the whole grid. `find_pane` clones the
/// `Arc` so neither the sessions lock nor the pane map is held across the
/// resize, which would otherwise nest under the pane commit lock.
async fn handle_resize_pane(
    req: &ResizePaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    let Some(pane) = find_pane(sessions, &req.pane_id) else {
        return Ok(ResponseBody::Error("pane not found".to_string()));
    };
    let client_id = connection_client_id.lock().clone();
    match client_id {
        Some(client_id) => pane.set_client_viewport(client_id, req.cols, req.rows)?,
        // Pre-attach CLI callers address panes by target and have no viewport
        // of their own, so there is nothing to min-fit against.
        None => pane.resize(req.cols, req.rows)?,
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §16.9 Resize the server-authoritative layout ratio of a pane.
async fn handle_resize_layout(
    req: &mux_protocol::ResizeLayoutRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    _outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    let direction = match req.direction {
        1 => crate::layout::SplitDirection::LeftRight,
        2 => crate::layout::SplitDirection::TopBottom,
        _ => {
            return Ok(ResponseBody::Error(format!(
                "invalid split direction: {}",
                req.direction
            )));
        }
    };
    let session_id = {
        let mut sessions_w = sessions.write();
        let Some(session) = sessions_w
            .iter_mut()
            .find(|s| s.layout.root.find_pane(&req.pane_id).is_some())
        else {
            return Ok(ResponseBody::Error(format!(
                "pane not found: {}",
                req.pane_id
            )));
        };
        if let Err(error) = session
            .layout
            .resize_pane(&req.pane_id, direction, req.delta)
        {
            tracing::warn!(error = %error, pane_id = %req.pane_id, "layout resize_pane failed");
            return Ok(ResponseBody::Error(format!("{error}")));
        }
        session.id.clone()
    };
    broadcast_layout_changed(sessions, &session_id);
    zlog::info!(
        "layout resized: pane={} direction={:?} delta={}",
        req.pane_id,
        direction,
        req.delta
    );
    Ok(ResponseBody::Error(String::new()))
}
fn find_pane(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    pane_id: &str,
) -> Option<Arc<crate::pane::Pane>> {
    let sessions = sessions.read();
    sessions
        .iter()
        .find_map(|session| session.panes.read().get(pane_id).cloned())
}

/// §3.10 发送输入 + §16.6 OSC 52 剪贴板拦截
async fn handle_send_input(
    req: &SendInputRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    _outbound_tx: &mpsc::UnboundedSender<Envelope>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if !client_still_attached(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(
            "client not attached (kicked or detached)".to_string(),
        ));
    }
    let Some(pane) = find_pane(sessions, &req.pane_id) else {
        return Ok(ResponseBody::Error(format!(
            "pane not found: {}",
            req.pane_id
        )));
    };

    // §16.6 解析 OSC 52 序列: ESC ] 52 ; c ; <base64> BEL/ST
    let mut osc52_parser = crate::clipboard::Osc52Parser::new();
    if let Some(base64_content) = osc52_parser.feed(&req.data) {
        // §16.6 OSC 52 触发剪贴板更新并通知所有客户端
        let origin_host = std::env::var("HOSTNAME").unwrap_or_else(|_| "z3rm-server".to_string());
        {
            let mut txs = Vec::new();
            for session in sessions.read().iter() {
                for tx in session.lifecycle_subscribers.read().values() {
                    txs.push(tx.clone());
                }
            }
            clipboard.set_from_osc52(&base64_content, origin_host, &txs)?;
        }
        // OSC 52 序列已被消费, 不转发到 PTY
        return Ok(ResponseBody::Error(String::new()));
    }

    // §16.6 检查 bracketed paste 模式切换序列
    // ESC [ ? 2004 h (enable) / ESC [ ? 2004 l (disable)
    const BRACKETED_PASTE_ENABLE: &[u8] = &[0x1B, b'[', b'?', b'2', b'0', b'0', b'4', b'h'];
    const BRACKETED_PASTE_DISABLE: &[u8] = &[0x1B, b'[', b'?', b'2', b'0', b'0', b'4', b'l'];
    if req.data == BRACKETED_PASTE_ENABLE {
        // §16.6 启用 bracketed paste
        pane.set_bracketed_paste_mode(true);
        return Ok(ResponseBody::Error(String::new()));
    }
    if req.data == BRACKETED_PASTE_DISABLE {
        // §16.6 禁用 bracketed paste
        pane.set_bracketed_paste_mode(false);
        return Ok(ResponseBody::Error(String::new()));
    }

    // §3.10 普通输入: 转发到 PTY
    pane.write_input(&req.data)?;
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 粘贴文本 — 调用 pane.paste (内部处理 bracketed paste markers)
async fn handle_paste(
    req: &PasteRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let Some(pane) = find_pane(sessions, &req.pane_id) else {
        return Ok(ResponseBody::Error(format!(
            "pane not found: {}",
            req.pane_id
        )));
    };
    pane.paste(&req.text)?;
    Ok(ResponseBody::Error(String::new()))
}

/// §16.6 设置剪贴板
async fn handle_set_clipboard(
    req: &SetClipboardRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
) -> anyhow::Result<ResponseBody> {
    // §16.6 从 proto 消息转换并设置剪贴板
    let entry = match &req.entry {
        Some(proto_entry) => crate::clipboard::ClipboardEntry::from_proto(proto_entry),
        None => {
            return Ok(ResponseBody::Error("empty clipboard entry".to_string()));
        }
    };
    // Fan-out ClipboardChanged to every attached client's lifecycle channel.
    let mut txs = Vec::new();
    for session in sessions.read().iter() {
        for tx in session.lifecycle_subscribers.read().values() {
            txs.push(tx.clone());
        }
    }
    clipboard.set_clipboard(entry, &txs);
    Ok(ResponseBody::Error(String::new()))
}

/// §16.6 获取剪贴板
async fn handle_get_clipboard(
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
) -> anyhow::Result<ResponseBody> {
    let entry = clipboard.get_clipboard();
    match entry {
        Some(entry) => {
            let proto_entry = entry.to_proto();
            Ok(ResponseBody::Clipboard(GetClipboardResponse {
                entry: Some(proto_entry),
            }))
        }
        None => Ok(ResponseBody::Clipboard(GetClipboardResponse {
            entry: Some(mux_protocol::proto::ClipboardEntry {
                content_type: mux_protocol::proto::clipboard_entry::ClipboardContentType::Text
                    as i32,
                data: Vec::new(),
                origin_host: String::new(),
            }),
        })),
    }
}

fn grid_cell_to_proto(cell: crate::grid_sync::Cell) -> Cell {
    let underline_style = match cell.style.underline {
        crate::grid_sync::UnderlineStyle::None => 1,
        crate::grid_sync::UnderlineStyle::Single => 2,
        crate::grid_sync::UnderlineStyle::Double => 3,
        crate::grid_sync::UnderlineStyle::Curly => 4,
        crate::grid_sync::UnderlineStyle::Dotted => 5,
        crate::grid_sync::UnderlineStyle::Dashed => 6,
    };
    Cell {
        char: cell.character,
        style: Some(CellStyle {
            bold: cell.style.bold,
            italic: cell.style.italic,
            underline: !matches!(cell.style.underline, crate::grid_sync::UnderlineStyle::None),
            strikethrough: cell.style.strikethrough,
            dim: cell.style.dim,
            reverse: cell.style.reverse,
            underline_style,
            underline_color: cell.style.underline_color,
            wide_char: cell.style.wide_char,
            wide_char_spacer: cell.style.wide_char_spacer,
            leading_wide_char_spacer: cell.style.leading_wide_char_spacer,
            wrapline: cell.style.wrapline,
            hidden: cell.style.hidden,
        }),
        foreground: cell.foreground,
        background: cell.background,
        zerowidth: cell.zerowidth,
        hyperlink: cell.hyperlink.map(|hyperlink| mux_protocol::Hyperlink {
            id: hyperlink.id,
            uri: hyperlink.uri,
        }),
    }
}

/// §3.3 获取 grid 更新
/// §16.9 获取回滚缓冲区历史行
async fn handle_fetch_scrollback(
    req: &FetchScrollbackRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            let (lines, total, version) =
                pane.fetch_scrollback(req.from_line, req.direction, req.count);
            let resp = FetchScrollbackResponse {
                lines: lines
                    .into_iter()
                    .map(|r| RowChange {
                        row: r.row,
                        cells: r.cells.into_iter().map(grid_cell_to_proto).collect(),
                    })
                    .collect(),
                total_lines: total,
                scrollback_version: version,
            };
            return Ok(ResponseBody::Scrollback(resp));
        }
    }
    Ok(ResponseBody::Error("pane not found".to_string()))
}

/// §16.9 搜索回滚缓冲区
async fn handle_search_scrollback(
    req: &SearchScrollbackRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            let (matches, version) =
                pane.search_scrollback(&req.regex, req.from_line, req.direction, req.max_results);
            let resp = SearchScrollbackResponse {
                matches: matches
                    .into_iter()
                    .map(|(line_num, row)| SearchMatch {
                        line_number: line_num,
                        context: row.cells.into_iter().map(grid_cell_to_proto).collect(),
                    })
                    .collect(),
                scrollback_version: version,
            };
            return Ok(ResponseBody::SearchScrollback(resp));
        }
    }
    Ok(ResponseBody::Error("pane not found".to_string()))
}

async fn handle_fetch_grid_update(
    req: &FetchGridUpdateRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            let update = pane.fetch_grid_update(req.since_generation);
            let resp = match update {
                crate::grid_sync::GridUpdate::Diff {
                    from_generation,
                    to_generation,
                    diff,
                } => FetchGridUpdateResponse {
                    from_generation,
                    to_generation,
                    update: Some(FetchGridUpdateResponseUpdate::Diff(GridDiff {
                        rows: diff
                            .rows
                            .into_iter()
                            .map(|r| RowChange {
                                row: r.row,
                                cells: r.cells.into_iter().map(grid_cell_to_proto).collect(),
                            })
                            .collect(),
                    })),
                },
                crate::grid_sync::GridUpdate::FullSnapshot {
                    to_generation,
                    snapshot,
                } => FetchGridUpdateResponse {
                    from_generation: 0,
                    to_generation,
                    update: Some(FetchGridUpdateResponseUpdate::FullSnapshot(
                        FullGridSnapshot {
                            cols: snapshot.cols,
                            rows: snapshot.rows,
                            cells: snapshot.cells.into_iter().map(grid_cell_to_proto).collect(),
                            cursor: Some(CursorState {
                                col: snapshot.cursor.col,
                                row: snapshot.cursor.row,
                                style: match snapshot.cursor.style {
                                    crate::grid_sync::CursorShape::Block => 1,
                                    crate::grid_sync::CursorShape::Bar => 2,
                                    crate::grid_sync::CursorShape::Underline => 3,
                                    crate::grid_sync::CursorShape::HollowBlock => 4,
                                    crate::grid_sync::CursorShape::Hidden => 5,
                                },
                                visible: snapshot.cursor.visible,
                                blinking: snapshot.cursor.blinking,
                            }),
                            alternate_screen: snapshot.alternate_screen,
                            // §15.12 usize → u32 (saturating; scrollback 远小于 u32::MAX)。
                            display_offset: u32::try_from(snapshot.display_offset)
                                .unwrap_or(u32::MAX),
                            history_size: u32::try_from(snapshot.history_size).unwrap_or(u32::MAX),
                            history_version: snapshot.history_version,
                            modes: Some(snapshot.modes),
                        },
                    )),
                },
                crate::grid_sync::GridUpdate::NoChange(current_gen) => FetchGridUpdateResponse {
                    from_generation: current_gen,
                    to_generation: current_gen,
                    update: None,
                },
            };
            return Ok(ResponseBody::GridUpdate(resp));
        }
    }
    Ok(ResponseBody::Error("pane not found".to_string()))
}

// ============================================================================
// §15.4 Attach snapshot helpers
// ============================================================================

/// §15.4 把 session.panes 中的 pane 元数据转成 proto PaneInfo。
fn pane_info_for(
    session: &crate::session::Session,
    pane_id: &str,
) -> Option<mux_protocol::PaneInfo> {
    let panes = session.panes.read();
    let pane = panes.get(pane_id)?;
    let metadata = pane.metadata_snapshot();
    Some(mux_protocol::PaneInfo {
        id: pane.id.clone(),
        cwd: pane.get_cwd(),
        title: metadata.title,
        command: pane.command.clone().unwrap_or_default(),
        generation: metadata.generation,
        size: Some(mux_protocol::TerminalSize {
            cols: metadata.cols,
            rows: metadata.rows,
        }),
        is_alive: metadata.is_alive,
        zoomed: metadata.zoomed,
    })
}

/// §15.4 / §16.9 把内部 LayoutTree 转成 proto LayoutTree。
/// 空根 (session 初始状态) 转 `root: None`。
fn layout_tree_to_proto(tree: &crate::layout::LayoutTree) -> mux_protocol::LayoutTree {
    use crate::layout::{LayoutNode, SplitDirection};
    use mux_protocol::proto::layout_node::Node;
    use mux_protocol::proto::split_node::SplitDirection as ProtoDir;
    use mux_protocol::proto::{LayoutNode as ProtoNode, PaneLeaf, SplitNode};

    fn convert(node: &LayoutNode) -> Option<ProtoNode> {
        let proto_node = match node {
            LayoutNode::Pane { id, pane_id } if id.is_empty() && pane_id.is_empty() => {
                return None;
            }
            LayoutNode::Pane { id, pane_id } => ProtoNode {
                id: id.clone(),
                node: Some(Node::Pane(PaneLeaf {
                    pane_id: pane_id.clone(),
                })),
            },
            LayoutNode::Split {
                id,
                direction,
                children,
                ratios,
            } => {
                let proto_children: Vec<ProtoNode> = children.iter().filter_map(convert).collect();
                let proto_dir = match direction {
                    SplitDirection::LeftRight => ProtoDir::LeftRight,
                    SplitDirection::TopBottom => ProtoDir::TopBottom,
                } as i32;
                ProtoNode {
                    id: id.clone(),
                    node: Some(Node::Split(SplitNode {
                        direction: proto_dir,
                        children: proto_children,
                        ratios: ratios.clone(),
                    })),
                }
            }
        };
        Some(proto_node)
    }

    mux_protocol::LayoutTree {
        root: convert(&tree.root),
    }
}

/// §3.4 lifecycle 广播 helper: 对 `session_id` 会话内所有已注册 lifecycle 订阅者
/// (每个 attached 连接的 outbound channel) 投递一条 Notification (at-least-once)。
///
/// 该路径与 pane 维度 lossy 的 PaneOutput/PaneDirty 完全分离:
/// - lifecycle 通知 (PaneAdded / PaneRemoved / SessionLayoutChanged) 必须
///   送达每一个 attached 客户端, 不只是发起操作的连接;
/// - PaneDirty 是 best-effort 的刷新触发器, 一次丢失无害 (下次 generation bump
///   会再发, 客户端 fetch_grid_update 也会自驱 reconcile)。
///
/// outbound channel 是 tokio::mpsc::unbounded, 因此 subscriber 慢不会丢通知
/// (会背压到该连接的写循环), closed channel 的 send 失败时立即清理对应订阅,
/// 避免泄漏 disconnected 客户端。
fn broadcast_lifecycle_in_session(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
    notif: Notification,
) {
    let sessions_r = sessions.read();
    let Some(session) = sessions_r.iter().find(|s| s.id == session_id) else {
        return;
    };
    session.broadcast_lifecycle(notif);
}

/// §16.9 / §3.4 把当前会话的 layout 变更 fan-out 到所有 attached 连接。
///
/// 调用方在 split/close/focus/zoom 后调用此函数; 通知进入会话级 lifecycle 路径
/// 而非仅发起方的 outbound_tx, 因此多个 attached 客户端都会收到 layout 刷新。

/// §3.7 把 pane 从会话 layout 中摘除。
///
/// `LayoutTree::remove_pane` 拒绝移除唯一的根 pane (移除后树就没有根了), 而
/// 会话最后一个 pane 退出时正好落在这个分支上 —— 忽略这个错误会让 layout 留下
/// 一个指向已死 pane 的僵尸节点。这种情况下正确的结果是回到空根,
/// `handle_spawn_pane` 会在下一个 pane 出现时重新播种它。
fn detach_pane_from_layout(session: &mut crate::session::Session, pane_id: &str) {
    let is_sole_root = matches!(
        &session.layout.root,
        crate::layout::LayoutNode::Pane { pane_id: root_pane_id, .. }
            if root_pane_id.as_str() == pane_id
    );
    if is_sole_root {
        session.layout = crate::layout::LayoutTree::empty();
        return;
    }
    if let Err(error) = session.layout.remove_pane(pane_id) {
        tracing::error!(
            %error,
            %pane_id,
            session_id = %session.id,
            "failed to remove pane from layout"
        );
    }
}

/// §3.4 When a pane's shell exits, remove it from the session and fan-out PaneRemoved.
fn install_pane_exit_hook(
    pane: &std::sync::Arc<crate::pane::Pane>,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: String,
    pane_id: String,
) {
    let sessions = sessions.clone();
    let hook = std::sync::Arc::new(move || {
        {
            let mut sessions_w = sessions.write();
            if let Some(session) = sessions_w.iter_mut().find(|s| s.id == session_id) {
                detach_pane_from_layout(session, &pane_id);
                session.panes.write().remove(&pane_id);
            }
        }
        broadcast_pane_removed(&sessions, &session_id, &pane_id, 0);
        broadcast_layout_changed(&sessions, &session_id);
    });
    pane.set_exit_hook(hook);
}

fn client_still_attached(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> bool {
    let Some(client_id) = connection_client_id.lock().clone() else {
        // Pre-attach local socket: allow (create_session etc.).
        return true;
    };
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        if session
            .attached_clients
            .read()
            .iter()
            .any(|c| c.client_id == client_id)
        {
            return true;
        }
    }
    false
}

fn broadcast_layout_changed(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
) {
    let sessions_r = sessions.read();
    let Some(session) = sessions_r.iter().find(|s| s.id == session_id) else {
        return;
    };
    // §3.3 一次 fan-out 覆盖会话的每个已连接窗口 (Plan 32)。
    let notified_windows = session.broadcast_layout_change(layout_tree_to_proto(&session.layout));
    tracing::trace!(
        session_id,
        windows = notified_windows.len(),
        "layout change broadcast"
    );
}

/// §3.4 fan-out PaneAdded 到该会话所有 attached 连接 (split / spawn 后调用)。
fn broadcast_pane_added(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
    pane_id: &str,
    tab_id: &str,
) {
    let notify = Notification {
        event: Some(mux_protocol::notification::Event::PaneAdded(
            mux_protocol::PaneAdded {
                pane_id: pane_id.to_string(),
                tab_id: tab_id.to_string(),
            },
        )),
    };
    broadcast_lifecycle_in_session(sessions, session_id, notify);
}

/// §3.4 fan-out PaneRemoved 到该会话所有 attached 连接 (close / 自然退出后调用)。
fn broadcast_pane_removed(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
    pane_id: &str,
    exit_code: i32,
) {
    let notify = Notification {
        event: Some(mux_protocol::notification::Event::PaneRemoved(
            mux_protocol::PaneRemoved {
                pane_id: pane_id.to_string(),
                exit_code,
            },
        )),
    };
    broadcast_lifecycle_in_session(sessions, session_id, notify);
}

/// §4.7 / §16.6 把客户端提供的路径解析到 `root` 之内, 越界一律拒绝。
///
/// 三条约束缺一不可:
/// - 显式拒绝 `..`, 否则相对路径可以向上跳出 root;
/// - 目标可能还不存在 (被删除的版本 / 尚未创建的文件), 所以 canonicalize 的是
///   最近的**已存在祖先** —— canonicalize 失败绝不能等同于放行;
/// - 比较的是 canonical 前缀, 因此 root 内指向外部的 symlink 也会被挡下。
fn resolve_path_within_root(
    root: &std::path::Path,
    requested: &str,
) -> anyhow::Result<std::path::PathBuf> {
    use std::path::Component;

    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing session root: {}", root.display()))?;
    let requested_path = std::path::Path::new(requested);
    anyhow::ensure!(!requested_path.as_os_str().is_empty(), "path is empty");
    anyhow::ensure!(
        !requested_path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "path may not contain parent traversal"
    );

    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        canonical_root.join(requested_path)
    };

    let mut existing_ancestor = candidate.as_path();
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor
            .parent()
            .context("path has no existing ancestor")?;
    }
    let canonical_ancestor = existing_ancestor.canonicalize().with_context(|| {
        format!(
            "canonicalizing path ancestor: {}",
            existing_ancestor.display()
        )
    })?;
    anyhow::ensure!(
        canonical_ancestor.starts_with(&canonical_root),
        "path escapes session cwd"
    );

    let suffix = candidate
        .strip_prefix(existing_ancestor)
        .context("resolving path suffix")?;
    Ok(canonical_ancestor.join(suffix))
}

/// §16.6 一个连接可以访问的文件系统范围。
///
/// `root` 是它已 attach 的 session 的 cwd (§3.2 的 worktree 根);
/// `snapshot_watch` 用来回答 §4.7 的"这个文件改过没有"。
struct SessionFileScope {
    root: std::path::PathBuf,
    snapshot_watch: Option<Arc<crate::snapshot::SnapshotWatch>>,
}

fn session_file_scopes(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> Vec<SessionFileScope> {
    let Some(client_id) = connection_client_id.lock().clone() else {
        return Vec::new();
    };
    sessions
        .read()
        .iter()
        .filter(|session| {
            session
                .attached_clients
                .read()
                .iter()
                .any(|attached| attached.client_id == client_id)
        })
        .map(|session| SessionFileScope {
            root: std::path::PathBuf::from(&session.cwd),
            snapshot_watch: session.snapshot_watch.clone(),
        })
        .collect()
}

/// §16.6 把 ReadFile / ListDir / StatFile 的路径限制在调用方已 attach 的
/// session cwd 之内, 并返回该 session 的 shadow watch。
///
/// server 跑在用户真实的文件系统上, 这几个 RPC 没有沙箱就等于把整台机器的读
/// 权限交给任何能连上 socket 的客户端 (`../../etc/passwd`)。没有 attach 的连接
/// 没有 worktree 范围, 直接拒绝, 而不是退化成"整个文件系统"。
fn resolve_session_file_path(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    requested: &str,
) -> anyhow::Result<(
    std::path::PathBuf,
    Option<Arc<crate::snapshot::SnapshotWatch>>,
)> {
    let scopes = session_file_scopes(sessions, connection_client_id);
    anyhow::ensure!(
        !scopes.is_empty(),
        "file access requires an attached session"
    );
    let mut last_error = None;
    for scope in scopes {
        match resolve_path_within_root(&scope.root, requested) {
            Ok(path) => return Ok((path, scope.snapshot_watch)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("no attached session worktree"))
        .context(format!(
            "path is outside the attached session worktree: {requested}"
        )))
}

async fn handle_list_file_versions(
    request: &mux_protocol::ListFileVersionsRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let (watch, root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let path = resolve_path_within_root(&root, &request.path)?;
    let versions = tokio::task::spawn_blocking(move || watch.list_versions(path))
        .await
        .context("joining shadow list-versions request")??;
    Ok(ResponseBody::FileVersions(
        mux_protocol::ListFileVersionsResponse {
            versions: versions
                .into_iter()
                .map(|version| mux_protocol::FileVersion {
                    version_id: version.version_id,
                    seq_no: version.seq_no,
                    trigger: format!("{:?}", version.trigger).to_ascii_lowercase(),
                })
                .collect(),
        },
    ))
}

async fn handle_get_file_version(
    request: &mux_protocol::GetFileVersionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let (watch, root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let path = resolve_path_within_root(&root, &request.path)?;
    let version_id = request.version_id;
    let content = tokio::task::spawn_blocking(move || watch.get_version(path, version_id))
        .await
        .context("joining shadow get-version request")??
        .with_context(|| format!("shadow version not found: {version_id}"))?;
    Ok(ResponseBody::FileVersionContent(
        mux_protocol::GetFileVersionResponse { content },
    ))
}

async fn handle_decline_file_version(
    request: &mux_protocol::DeclineFileVersionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let (watch, root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let path = resolve_path_within_root(&root, &request.path)?;
    let version_id = request.version_id;
    let declined = tokio::task::spawn_blocking(move || watch.decline(path, version_id))
        .await
        .context("joining shadow decline request")?;
    match declined {
        Ok(()) => Ok(ResponseBody::DeclineFileVersion(
            mux_protocol::DeclineFileVersionResponse { restored: true },
        )),
        // §4.8 恢复失败必须让客户端看到原因: 写死 restored=true 会把一次没有
        // 发生的回滚显示成成功, 而 `?` 会连带拆掉整条连接。
        Err(error) => Ok(ResponseBody::Error(format!(
            "decline_file_version: {error:#}"
        ))),
    }
}

fn snapshot_context_for_session(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
) -> anyhow::Result<(Arc<crate::snapshot::SnapshotWatch>, std::path::PathBuf)> {
    let sessions = sessions.read();
    let session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .with_context(|| format!("session not found: {session_id}"))?;
    let watch = session
        .snapshot_watch
        .clone()
        .with_context(|| format!("shadow snapshot is not active for session: {session_id}"))?;
    Ok((watch, std::path::PathBuf::from(&session.cwd)))
}

// ============================================================================
// Plan 10: §3.3 / §16.6 Real file RPC handlers (previously stubs)
// ============================================================================

/// §16.6 ReadFile: 读取 attach 会话 worktree 内的文件,自动检测 binary。
async fn handle_read_file(
    req: &mux_protocol::ReadFileRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    let path = match resolve_session_file_path(sessions, connection_client_id, &req.path) {
        Ok((path, _snapshot_watch)) => path,
        Err(error) => return Ok(ResponseBody::Error(format!("read_file: {error:#}"))),
    };
    match std::fs::read(&path) {
        Ok(bytes) => {
            // Binary detection: check for null bytes in first 8KB (same heuristic
            // as shadow_snapshot Monitor — ELF/PE/Mach-O magic or > 10% null).
            let is_binary = detect_binary(&bytes);
            let encoding = if is_binary {
                "binary".to_string()
            } else {
                "utf-8".to_string()
            };
            Ok(ResponseBody::FileContent(mux_protocol::ReadFileResponse {
                content: bytes,
                is_binary,
                encoding,
            }))
        }
        Err(e) => Ok(ResponseBody::Error(format!("read_file: {}", e))),
    }
}

/// §16.6 ListDir: 列出 attach 会话 worktree 内某个目录的条目。
async fn handle_list_dir(
    req: &mux_protocol::ListDirRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    let (path, snapshot_watch) =
        match resolve_session_file_path(sessions, connection_client_id, &req.path) {
            Ok(resolved) => resolved,
            Err(error) => return Ok(ResponseBody::Error(format!("list_dir: {error:#}"))),
        };
    // §4.3 list_versions 要和单写 recorder 线程做一次同步 round-trip,
    // 目录里每个文件一次, 不能压在 async worker 上。
    let listing = tokio::task::spawn_blocking(move || {
        let has_shadow_versions = |file: &std::path::Path| match snapshot_watch.as_ref() {
            Some(watch) => match watch.list_versions(file.to_path_buf()) {
                Ok(versions) => !versions.is_empty(),
                Err(error) => {
                    tracing::warn!(
                        path = %file.display(),
                        %error,
                        "shadow version lookup failed during list_dir"
                    );
                    false
                }
            },
            None => false,
        };
        read_dir_entries(&path, &has_shadow_versions)
    })
    .await
    .context("joining list-dir request")?;
    match listing {
        Ok(entries) => Ok(ResponseBody::DirListing(mux_protocol::ListDirResponse {
            entries,
        })),
        Err(error) => Ok(ResponseBody::Error(format!("list_dir: {error:#}"))),
    }
}

/// §16.6 读取一个目录的条目。
///
/// `is_modified` 的语义 (§4.7): shadow snapshot 在本 session 的监视期内为该
/// 路径记录过版本, 也就是"session 启动之后被改过"。目录恒为 false —— watcher
/// 只给文件建版本。session 没有 armed 的 watcher 时该字段恒为 false, 含义是
/// "未知"而不是"未修改"。
///
/// 元数据读不到的条目会被跳过并记日志: proto `DirEntry` 没有错误字段, 把权限
/// 失败或竞态删除报成 `is_dir=false, size=0` 会让客户端把它当成一个空文件。
/// 指向目录的 symlink 按目标类型上报 (文件浏览器要的是目标语义), 断链 symlink
/// 退回它自身的 lstat, 这样它仍然出现在列表里。
fn read_dir_entries(
    path: &std::path::Path,
    has_shadow_versions: &dyn Fn(&std::path::Path) -> bool,
) -> anyhow::Result<Vec<mux_protocol::DirEntry>> {
    let mut entries = Vec::new();
    let read_dir =
        std::fs::read_dir(path).with_context(|| format!("reading directory: {}", path.display()))?;
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    directory = %path.display(),
                    %error,
                    "list_dir skipped an unreadable directory entry"
                );
                continue;
            }
        };
        let entry_path = entry.path();
        let metadata = match std::fs::metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(target_error) => match entry.metadata() {
                Ok(link_metadata) => {
                    tracing::debug!(
                        path = %entry_path.display(),
                        error = %target_error,
                        "list_dir reporting link metadata for an unresolvable target"
                    );
                    link_metadata
                }
                Err(error) => {
                    tracing::warn!(
                        path = %entry_path.display(),
                        %error,
                        "list_dir skipped an entry with unreadable metadata"
                    );
                    continue;
                }
            },
        };
        let is_dir = metadata.is_dir();
        entries.push(mux_protocol::DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            size: metadata.len(),
            is_modified: !is_dir && has_shadow_versions(&entry_path),
        });
    }
    // 目录列表排序:目录优先,然后按名称 (确定性输出便于测试)。
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// §16.6 StatFile: 返回 attach 会话 worktree 内某个路径的元数据。
async fn handle_stat_file(
    req: &mux_protocol::StatFileRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    let path = match resolve_session_file_path(sessions, connection_client_id, &req.path) {
        Ok((path, _snapshot_watch)) => path,
        Err(error) => return Ok(ResponseBody::Error(format!("stat_file: {error:#}"))),
    };
    match std::fs::metadata(&path) {
        Ok(meta) => Ok(ResponseBody::FileStat(mux_protocol::StatFileResponse {
            exists: true,
            size: meta.len(),
            is_dir: meta.is_dir(),
            modified_timestamp: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(ResponseBody::FileStat(mux_protocol::StatFileResponse {
                exists: false,
                size: 0,
                is_dir: false,
                modified_timestamp: 0,
            }))
        }
        Err(e) => Ok(ResponseBody::Error(format!("stat_file: {}", e))),
    }
}

/// §16.8 / §16.12 InstallExtension: server 端没有 extension host。
///
/// QuickJS runtime 只在 client 侧 (`crates/quickjs_runtime`), daemon 既不能
/// 执行也不能校验 server-side 扩展; 真正的安装逻辑在
/// `crates/z3rm/src/cli/marketplace.rs`。这里返回 `success=false` 的类型化
/// 响应而不是空 `Error` —— 空 `Error` 在客户端等价于成功, 会让
/// `mux::sync_extensions_to_remote` 把一次没发生的安装报成成功。
async fn handle_install_extension(
    req: &mux_protocol::InstallExtensionRequest,
) -> anyhow::Result<ResponseBody> {
    zlog::warn!(
        "extension install rejected: name={} (mux_server has no extension host)",
        req.name
    );
    Ok(ResponseBody::ExtensionInstalled(
        mux_protocol::InstallExtensionResponse {
            name: req.name.clone(),
            success: false,
            error: "mux_server has no extension host: server-side extension install is not \
                    supported"
                .to_string(),
        },
    ))
}

/// §3.10 RenameSession: 更新 session 名称。
async fn handle_rename_session(
    req: &mux_protocol::RenameSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    let session = sessions_w
        .iter_mut()
        .find(|s| s.id == req.id)
        .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.id))?;
    let old_name = std::mem::replace(&mut session.name, req.name.clone());
    zlog::info!(
        "session renamed: id={} old={} new={}",
        req.id,
        old_name,
        req.name
    );
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 SetPaneTitle: 更新 pane 标题 (OSC 0/1/2 也可以触发)。
async fn handle_set_pane_title(
    req: &mux_protocol::SetPaneTitleRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    let mut matched_session_id: Option<String> = None;
    let mut matched_pane: Option<std::sync::Arc<crate::pane::Pane>> = None;
    for session in sessions_r.iter() {
        if let Some(pane) = session.panes.read().get(&req.pane_id).cloned() {
            matched_session_id = Some(session.id.clone());
            matched_pane = Some(pane);
            break;
        }
    }
    drop(sessions_r);

    match matched_pane {
        Some(pane) => {
            pane.set_title(req.title.clone());
            // §3.4 / §3.3 broadcast PaneTitleChanged so every attached client
            // learns the title string (PaneDirty only carries pane_id).
            // broadcast_lifecycle_in_session looks up the session by id.
            if let Some(session_id) = matched_session_id {
                broadcast_lifecycle_in_session(
                    sessions,
                    &session_id,
                    Notification {
                        event: Some(mux_protocol::notification::Event::PaneTitleChanged(
                            mux_protocol::PaneTitleChanged {
                                pane_id: req.pane_id.clone(),
                                title: req.title.clone(),
                            },
                        )),
                    },
                );
            }
            zlog::info!("pane title set: pane={} title={}", req.pane_id, req.title);
            Ok(ResponseBody::Error(String::new()))
        }
        None => Ok(ResponseBody::Error(format!(
            "set_pane_title: pane {} not found",
            req.pane_id
        ))),
    }
}

/// §3.3 ZoomPane: 切换 pane zoom 状态, bump generation, 通知客户端。
async fn handle_zoom_pane(
    req: &mux_protocol::ZoomPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    let mut matched_session_id: Option<String> = None;
    let mut pane_found = false;

    {
        let sessions_r = sessions.read();
        for session in sessions_r.iter() {
            if let Some(pane) = session.panes.read().get(&req.pane_id) {
                pane.set_zoomed(req.zoom);
                matched_session_id = Some(session.id.clone());
                pane_found = true;
                break;
            }
        }
    }

    if !pane_found {
        return Ok(ResponseBody::Error(format!(
            "zoom_pane: pane {} not found",
            req.pane_id
        )));
    }

    // §3.4 zoom 影响 layout 可见性; PaneZoomed + SessionLayoutChanged 都属于
    // lifecycle 事件范畴, 走会话级 lifecycle fan-out 路径送达所有 attached 客户端。
    let session_id = matched_session_id.expect("pane_found implies matched session");
    {
        let sessions_r = sessions.read();
        if let Some(session) = sessions_r.iter().find(|s| s.id == session_id) {
            session.broadcast_lifecycle(Notification {
                event: Some(mux_protocol::notification::Event::PaneZoomed(
                    mux_protocol::PaneZoomed {
                        pane_id: req.pane_id.clone(),
                        zoomed: req.zoom,
                    },
                )),
            });
        }
    }
    broadcast_layout_changed(sessions, &session_id);

    zlog::info!("pane zoom: pane={} zoomed={}", req.pane_id, req.zoom);
    Ok(ResponseBody::ZoomPane(mux_protocol::ZoomPaneResponse {}))
}

/// §3.3 ShellIntegration: 查询 pane 的 cwd 和 prompt marker 信息。
async fn handle_shell_integration(
    req: &mux_protocol::ShellIntegrationRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        if let Some(pane) = session.panes.read().get(&req.pane_id) {
            return Ok(ResponseBody::ShellIntegration(
                mux_protocol::ShellIntegrationResponse {
                    cwd: pane.get_cwd(),
                    prompt_marker: pane.get_prompt_marker(),
                },
            ));
        }
    }
    Ok(ResponseBody::Error(format!(
        "shell_integration: pane {} not found",
        req.pane_id
    )))
}

/// §4.7 binary 检测 — 与 shadow_snapshot::Monitor::is_binary_file 同算法。
fn detect_binary(bytes: &[u8]) -> bool {
    const ELF_MAGIC: &[u8] = b"\x7fELF";
    const PE_MAGIC: &[u8] = b"MZ";
    const MACHO_MAGIC: &[u8] = b"\xfe\xed\xfa\xce";

    if bytes.len() >= 4 {
        if bytes.starts_with(ELF_MAGIC) || bytes.starts_with(MACHO_MAGIC) {
            return true;
        }
    }
    if bytes.len() >= 2 && bytes.starts_with(PE_MAGIC) {
        return true;
    }

    // null byte ratio check on first 512 bytes
    let check_len = bytes.len().min(512);
    let null_count = bytes[..check_len].iter().filter(|&&b| b == 0).count();
    (null_count as f64 / check_len as f64) > 0.1
}

#[cfg(test)]
mod connection_unit_tests {
    use super::*;

    #[test]
    fn take_session_returns_removed_session() {
        let sessions = Arc::new(parking_lot::RwLock::new(vec![
            crate::session::Session::new(
                "session-1".to_string(),
                "one".to_string(),
                "/tmp".to_string(),
            ),
        ]));

        let removed = take_session(&sessions, "session-1");

        assert_eq!(
            removed.as_ref().map(|session| session.id.as_str()),
            Some("session-1")
        );
        assert!(sessions.read().is_empty());
    }

    #[test]
    fn unregister_client_removes_every_session_subscription() {
        let mut sessions = vec![
            crate::session::Session::new(
                "session-1".to_string(),
                "one".to_string(),
                "/tmp".to_string(),
            ),
            crate::session::Session::new(
                "session-2".to_string(),
                "two".to_string(),
                "/tmp".to_string(),
            ),
        ];
        for session in &mut sessions {
            session.add_attached_client(
                "client-1".to_string(),
                crate::session::AttachMode::Shared,
                ClientRole::ReadWrite,
                None,
            );
            let (sender, _receiver) = mpsc::unbounded_channel();
            session.add_lifecycle_subscriber("client-1".to_string(), sender);
        }

        let released = unregister_client_from_sessions(&mut sessions, "client-1");

        assert!(released.is_empty(), "no client claimed a window");
        assert!(
            sessions
                .iter()
                .all(|session| session.attached_client_count() == 0)
        );
        assert!(
            sessions
                .iter()
                .all(|session| session.lifecycle_subscribers.read().is_empty())
        );
    }

    /// §3.3 断连必须让 `connected_windows` 收缩, 并把 `WindowRemoved` fan-out
    /// 给会话里剩下的窗口 (Plan 32)。
    #[test]
    fn unregister_client_releases_its_window_and_notifies_peers() {
        let mut session = crate::session::Session::new(
            "session-1".to_string(),
            "one".to_string(),
            "/tmp".to_string(),
        );
        for (client_id, window_id) in [("client-a", "win-a"), ("client-b", "win-b")] {
            session.add_attached_client(
                client_id.to_string(),
                crate::session::AttachMode::Shared,
                ClientRole::ReadWrite,
                Some(window_id.to_string()),
            );
            session.add_window(window_id.to_string());
        }
        let (peer_sender, mut peer_notifications) = mpsc::unbounded_channel();
        session.add_lifecycle_subscriber("client-b".to_string(), peer_sender);
        let (leaving_sender, _leaving_notifications) = mpsc::unbounded_channel();
        session.add_lifecycle_subscriber("client-a".to_string(), leaving_sender);
        let mut sessions = vec![session];

        let released = unregister_client_from_sessions(&mut sessions, "client-a");

        assert_eq!(
            released,
            vec![ReleasedWindow {
                session_id: "session-1".to_string(),
                window_id: "win-a".to_string(),
            }]
        );
        assert_eq!(sessions[0].get_windows(), vec!["win-b".to_string()]);

        broadcast_window_removals(&sessions, &released);

        let envelope = match peer_notifications.try_recv() {
            Ok(envelope) => envelope,
            Err(error) => panic!("surviving window must receive WindowRemoved: {error}"),
        };
        match envelope.payload {
            Some(EnvelopePayload::Notification(Notification {
                event: Some(mux_protocol::notification::Event::WindowRemoved(removed)),
            })) => {
                assert_eq!(removed.window_id, "win-a");
                assert_eq!(removed.session_id, "session-1");
            }
            other => panic!("expected WindowRemoved, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn connection_cleanup_removes_registration_and_aborts_forwarders() {
        let mut session = crate::session::Session::new(
            "session-1".to_string(),
            "one".to_string(),
            "/tmp".to_string(),
        );
        session.add_attached_client(
            "client-1".to_string(),
            crate::session::AttachMode::Shared,
            ClientRole::ReadWrite,
            Some("win-1".to_string()),
        );
        session.add_window("win-1".to_string());
        let (subscriber, _notifications) = mpsc::unbounded_channel();
        session.add_lifecycle_subscriber("client-1".to_string(), subscriber);
        let pane = match crate::pane::Pane::spawn(
            "cleanup-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(crate::pane::ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn cleanup pane: {error}"),
        };
        let (pane_subscriber, mut pane_notifications) = mpsc::unbounded_channel();
        pane.add_subscriber("client-1".to_string(), pane_subscriber);
        session.panes.write().insert(pane.id.clone(), pane);
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));
        let client_id = Arc::new(parking_lot::Mutex::new(Some("client-1".to_string())));
        let forward_tasks = Arc::new(parking_lot::Mutex::new(vec![tokio::spawn(
            std::future::pending::<()>(),
        )]));

        cleanup_connection_state(&sessions, &client_id, &forward_tasks).await;

        let sessions = sessions.read();
        assert_eq!(sessions[0].attached_client_count(), 0);
        assert!(sessions[0].lifecycle_subscribers.read().is_empty());
        // §3.3 A dropped connection must shrink connected_windows (Plan 32),
        // otherwise every crashed GUI leaks a window into the session forever.
        assert!(
            sessions[0].get_windows().is_empty(),
            "connection cleanup must release the window it claimed"
        );
        assert!(matches!(
            pane_notifications.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(forward_tasks.lock().is_empty());
    }

    struct DropSignal(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn writer_exit_cancels_reader_task() {
        let reader_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = tokio::spawn({
            let guard = DropSignal(reader_dropped.clone());
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            }
        });
        let writer = tokio::spawn(async { Ok(()) });

        wait_for_connection_tasks(reader, writer, || async {}).await;

        assert!(reader_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reader_exit_drains_queued_writer_response() {
        let response_written = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (response_sender, response_receiver) = tokio::sync::oneshot::channel();
        let reader = tokio::spawn(async move {
            response_sender
                .send(())
                .map_err(|_| anyhow::anyhow!("writer dropped queued response"))?;
            Ok(())
        });
        let writer = tokio::spawn({
            let response_written = response_written.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                response_receiver.await?;
                response_written.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        });

        wait_for_connection_tasks(reader, writer, || async {}).await;

        assert!(response_written.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reader_exit_bounds_stalled_writer_drain() {
        let writer_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = tokio::spawn(async { Ok(()) });
        let writer = tokio::spawn({
            let guard = DropSignal(writer_dropped.clone());
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            }
        });

        wait_for_connection_tasks(reader, writer, || async {}).await;

        assert!(writer_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn shadow_path_rejects_parent_and_symlink_escape() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().join("root");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::create_dir_all(&outside).expect("create outside");

        assert!(resolve_path_within_root(&root, "../outside/file.txt").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).expect("create symlink");
            assert!(resolve_path_within_root(&root, "escape/file.txt").is_err());
        }
    }

    #[test]
    fn shadow_path_allows_missing_file_below_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().join("root");
        std::fs::create_dir_all(root.join("nested")).expect("create root");

        let resolved = resolve_path_within_root(&root, "nested/deleted.txt").expect("resolve path");

        // `resolve_path_within_root` canonicalizes the root, so the expectation
        // has to as well: on macOS the temp dir is `/var/...`, a symlink to
        // `/private/var/...`.
        let canonical_root = root.canonicalize().expect("canonicalize root");
        assert_eq!(resolved, canonical_root.join("nested/deleted.txt"));
    }

    #[test]
    fn path_within_root_rejects_absolute_escape_and_accepts_absolute_inside() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().join("root");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(root.join("inside.txt"), b"inside").expect("write file");
        let canonical_root = root.canonicalize().expect("canonicalize root");

        assert!(resolve_path_within_root(&root, "/etc/passwd").is_err());
        // A path that does not exist outside the root must be rejected too:
        // failing to canonicalize is not a reason to let it through.
        assert!(resolve_path_within_root(&root, "/etc/definitely-not-here").is_err());
        assert_eq!(
            resolve_path_within_root(&root, canonical_root.join("inside.txt").to_string_lossy().as_ref())
                .expect("absolute path inside root"),
            canonical_root.join("inside.txt")
        );
    }

    /// A file that does not exist yet must still be refused when it resolves
    /// outside the root — `..` is only one of the two escape routes.
    #[test]
    fn path_within_root_rejects_missing_file_outside_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().join("root");
        std::fs::create_dir_all(&root).expect("create root");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create outside");

        let missing = outside.join("never-created.txt");
        assert!(resolve_path_within_root(&root, missing.to_string_lossy().as_ref()).is_err());
    }

    fn attached_session(id: &str, cwd: &std::path::Path, client_id: &str) -> crate::session::Session {
        let mut session = crate::session::Session::new(
            id.to_string(),
            id.to_string(),
            cwd.to_string_lossy().into_owned(),
        );
        session.add_attached_client(
            client_id.to_string(),
            crate::session::AttachMode::Shared,
            ClientRole::ReadWrite,
            None,
        );
        session
    }

    #[test]
    fn file_path_requires_an_attached_session() {
        let directory = tempfile::tempdir().expect("temp directory");
        let sessions = Arc::new(parking_lot::RwLock::new(vec![attached_session(
            "session-1",
            directory.path(),
            "client-1",
        )]));

        let unattached = Arc::new(parking_lot::Mutex::new(None));
        assert!(resolve_session_file_path(&sessions, &unattached, "file.txt").is_err());

        // A connection whose client id is not registered with any session has
        // no worktree either — it must not fall back to the whole filesystem.
        let stranger = Arc::new(parking_lot::Mutex::new(Some("client-2".to_string())));
        assert!(resolve_session_file_path(&sessions, &stranger, "file.txt").is_err());
    }

    #[test]
    fn file_path_is_sandboxed_to_the_attached_session_cwd() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().join("root");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(root.join("nested")).expect("create root");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(outside.join("secret.txt"), b"secret").expect("write secret");
        let canonical_root = root.canonicalize().expect("canonicalize root");

        let sessions = Arc::new(parking_lot::RwLock::new(vec![attached_session(
            "session-1",
            &root,
            "client-1",
        )]));
        let client_id = Arc::new(parking_lot::Mutex::new(Some("client-1".to_string())));

        let (resolved, watch) = resolve_session_file_path(&sessions, &client_id, "nested/file.txt")
            .expect("path inside the session cwd resolves");
        assert_eq!(resolved, canonical_root.join("nested/file.txt"));
        assert!(watch.is_none());

        // `..` traversal.
        assert!(resolve_session_file_path(&sessions, &client_id, "../outside/secret.txt").is_err());
        // Absolute path outside the session cwd.
        assert!(resolve_session_file_path(&sessions, &client_id, "/etc/passwd").is_err());
        assert!(
            resolve_session_file_path(
                &sessions,
                &client_id,
                outside.join("secret.txt").to_string_lossy().as_ref()
            )
            .is_err()
        );
        // A path that does not exist and is outside the root.
        assert!(
            resolve_session_file_path(
                &sessions,
                &client_id,
                outside.join("missing.txt").to_string_lossy().as_ref()
            )
            .is_err()
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).expect("create symlink");
            assert!(
                resolve_session_file_path(&sessions, &client_id, "escape/secret.txt").is_err()
            );
        }
    }

    #[test]
    fn read_dir_entries_reports_shadow_modification_and_sorts_directories_first() {
        let directory = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir_all(directory.path().join("sub")).expect("create sub");
        std::fs::write(directory.path().join("edited.txt"), b"edited").expect("write edited");
        std::fs::write(directory.path().join("pristine.txt"), b"pristine").expect("write pristine");

        let entries = read_dir_entries(directory.path(), &|path: &std::path::Path| {
            path.file_name().and_then(|name| name.to_str()) == Some("edited.txt")
        })
        .expect("read entries");

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "edited.txt", "pristine.txt"]);
        assert!(entries[0].is_dir);
        assert!(entries[1].is_modified);
        assert_eq!(entries[1].size, b"edited".len() as u64);
        assert!(!entries[2].is_modified);
    }

    #[cfg(unix)]
    #[test]
    fn read_dir_entries_distinguishes_symlinks_from_empty_files() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target_directory = directory.path().join("target");
        std::fs::create_dir_all(&target_directory).expect("create target");
        std::os::unix::fs::symlink(&target_directory, directory.path().join("link-to-dir"))
            .expect("create dir symlink");
        std::os::unix::fs::symlink(
            directory.path().join("gone"),
            directory.path().join("broken"),
        )
        .expect("create broken symlink");

        let entries = read_dir_entries(directory.path(), &|_path| false).expect("read entries");

        let link_to_dir = entries
            .iter()
            .find(|entry| entry.name == "link-to-dir")
            .expect("dir symlink listed");
        assert!(link_to_dir.is_dir, "a symlink to a directory is a directory");

        let broken = entries
            .iter()
            .find(|entry| entry.name == "broken")
            .expect("broken symlink still listed");
        assert!(!broken.is_dir);
        assert!(
            broken.size > 0,
            "a broken symlink must carry its own lstat size, not a fabricated 0"
        );
    }

    #[test]
    fn detach_pane_from_layout_clears_sole_root_instead_of_leaving_a_zombie() {
        let mut session = crate::session::Session::new(
            "session-1".to_string(),
            "one".to_string(),
            "/tmp".to_string(),
        );
        session.layout =
            crate::layout::LayoutTree::with_pane("node-pane-1".to_string(), "pane-1".to_string());

        detach_pane_from_layout(&mut session, "pane-1");

        assert!(
            session.layout.is_empty_root(),
            "the last pane's node must not survive its shell"
        );
        assert!(session.layout.root.find_pane("pane-1").is_none());
    }

    #[test]
    fn detach_pane_from_layout_removes_a_split_child() {
        let mut session = crate::session::Session::new(
            "session-1".to_string(),
            "one".to_string(),
            "/tmp".to_string(),
        );
        session.layout =
            crate::layout::LayoutTree::with_pane("node-pane-1".to_string(), "pane-1".to_string());
        session
            .layout
            .split(
                "pane-1",
                "pane-2".to_string(),
                crate::layout::SplitDirection::LeftRight,
            )
            .expect("split layout");

        detach_pane_from_layout(&mut session, "pane-2");

        assert!(session.layout.root.find_pane("pane-2").is_none());
        assert!(session.layout.root.find_pane("pane-1").is_some());
    }

    #[tokio::test]
    async fn install_extension_reports_failure_instead_of_an_empty_error() {
        let response = handle_install_extension(&mux_protocol::InstallExtensionRequest {
            name: "z3rm-demo".to_string(),
            manifest: Vec::new(),
            source: Vec::new(),
        })
        .await
        .expect("install handler");

        match response {
            ResponseBody::ExtensionInstalled(installed) => {
                assert_eq!(installed.name, "z3rm-demo");
                assert!(!installed.success);
                assert!(!installed.error.is_empty());
            }
            other => panic!("expected a typed install response, got {other:?}"),
        }
    }
}
