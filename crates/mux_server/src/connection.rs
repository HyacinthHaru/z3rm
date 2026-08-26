// §9 Connection 模块 — mux_protocol 消息分发、帧编码/解码、通知广播。
// 每个客户端连接一个 tokio task, 处理请求并推送通知。

use anyhow::Context as _;
#[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
use interprocess::local_socket::tokio::Stream as LocalSocketStream;
use mux_protocol::proto::envelope::Payload as EnvelopePayload;
use mux_protocol::proto::fetch_grid_update_response::Update as FetchGridUpdateResponseUpdate;
use mux_protocol::proto::request::Body as RequestBody;
use mux_protocol::proto::response::Body as ResponseBody;
use mux_protocol::{
    FrameLengthError, FrameLengthErrorKind, MAX_FRAME_PAYLOAD, MAX_VARINT_LEN, check_frame_len,
    proto::*,
};
use prost::Message;
#[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
use sqlez::connection::Connection;
#[cfg(any(target_family = "wasm", not(feature = "desktop")))]
use crate::persistence::Connection;
use std::collections::HashSet;
use std::io::Read as _;
use std::sync::Arc;
#[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(all(not(target_family = "wasm"), not(feature = "desktop")))]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use crate::rt::mpsc;

// §3.3 客户端角色 (Plan 33)
use crate::pane::{ShellMarker, ShellMarkerKind};
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
        | RequestBody::NewWindow(_)
        | RequestBody::ListRecoveryCandidates(_)
        | RequestBody::ConfirmRecovery(_) => ClientRole::Admin,
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
#[cfg(all(not(target_family = "wasm")))]
pub async fn handle_connection<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    sessions: Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: Arc<parking_lot::Mutex<Connection>>,
    clipboard: Arc<crate::clipboard::ServerClipboard>,
    server_settings: Arc<crate::server_settings::ServerSettings>,
    shutdown_state: Arc<crate::ShutdownState>,
    extension_host: Arc<crate::extension_host::ServerExtensionHost>,
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
    let forward_tasks: Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>> =
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
        let extension_host = extension_host.clone();
        let forward_tasks = forward_tasks.clone();
        crate::rt::spawn(async move {
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
                    &extension_host,
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
    let write_handle = crate::rt::spawn(async move {
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

#[cfg(all(not(target_family = "wasm")))]
async fn wait_for_connection_tasks<Cleanup, CleanupFuture>(
    mut read_handle: crate::rt::JoinHandle<anyhow::Result<()>>,
    mut write_handle: crate::rt::JoinHandle<anyhow::Result<()>>,
    cleanup: Cleanup,
) where
    Cleanup: FnOnce() -> CleanupFuture,
    CleanupFuture: Future<Output = ()>,
{
    tokio::select! {
        result = &mut read_handle => {
            cleanup().await;
            match crate::rt::timeout(std::time::Duration::from_secs(1), &mut write_handle).await {
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
    forward_tasks: &Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>>,
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
#[cfg(all(not(target_family = "wasm")))]
async fn read_envelope<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
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
pub(crate) async fn dispatch_envelope(
    envelope: &Envelope,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    db: &Arc<parking_lot::Mutex<Connection>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    shutdown_state: &Arc<crate::ShutdownState>,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>>,
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
                db,
                clipboard,
                server_settings,
                client_role,
                connection_client_id,
                shutdown_state,
                extension_host,
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
    db: &Arc<parking_lot::Mutex<Connection>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    shutdown_state: &Arc<crate::ShutdownState>,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>>,
    trust: ConnectionTrust,
) -> anyhow::Result<()> {
    let request_id = req.request_id;
    extension_host.bind_sessions(sessions);

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
                extension_host,
            )
            .await?
        }
        RequestBody::Detach(_) => {
            handle_detach(sessions, connection_client_id, forward_tasks).await?
        }
        RequestBody::FetchGridUpdate(r) => handle_fetch_grid_update(r, sessions).await?,
        RequestBody::FetchScrollback(r) => handle_fetch_scrollback(r, sessions).await?,
        RequestBody::SearchScrollback(r) => handle_search_scrollback(r, sessions).await?,
        RequestBody::ListCommands(request) => handle_list_commands(request, sessions),
        // §4 A shadow request can fail for perfectly ordinary reasons — the
        // snapshot engine has not finished arming, the path is outside the
        // worktree, the version id is stale. `?` here would tear down the whole
        // connection over any of them, leaving the client with "connection
        // closed" and no reason; these become Error bodies instead.
        RequestBody::ListChangedFiles(request) => {
            shadow_response(handle_list_changed_files(request, sessions).await)
        }
        RequestBody::ListFileVersions(request) => {
            shadow_response(handle_list_file_versions(request, sessions).await)
        }
        RequestBody::GetFileVersion(request) => {
            shadow_response(handle_get_file_version(request, sessions).await)
        }
        RequestBody::DeclineFileVersion(request) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_decline_file_version(request, sessions, connection_client_id).await?
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
        RequestBody::ListRecoveryCandidates(_) => {
            if check_permission(role, ClientRole::Admin) {
                match handle_list_recovery_candidates(sessions, db) {
                    Ok(response) => response,
                    Err(error) => ResponseBody::Error(error.to_string()),
                }
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::ConfirmRecovery(request) => {
            if check_permission(role, ClientRole::Admin) {
                match handle_confirm_recovery(
                    request,
                    sessions,
                    db,
                    server_settings,
                    clipboard,
                    extension_host,
                ) {
                    Ok(response) => response,
                    Err(error) => ResponseBody::Error(error.to_string()),
                }
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }

        RequestBody::KillSession(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_kill_session(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::RenameSession(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_rename_session(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::InstallExtension(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_install_extension(r, extension_host).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        // §16.9 Server chrome actions: a click on chrome the daemon rendered.
        // Treated like a session mutation (ReadWrite): attached clients act,
        // pre-attach one-shot CLI connections cannot (they fall to ReadOnly).
        RequestBody::ExtensionChromeAction(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_extension_chrome_action(r, extension_host).await?
            } else {
                ResponseBody::Error("permission denied: read-write access required".to_string())
            }
        }
        RequestBody::NewWindow(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_new_window(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }

        // §3.3 需要 ReadWrite 的 pane 操作 (Plan 33)
        RequestBody::SpawnPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_spawn_pane(
                    r,
                    sessions,
                    server_settings,
                    clipboard,
                    extension_host,
                    connection_client_id,
                )
                .await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SplitPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_split_pane(
                    r,
                    sessions,
                    outbound_tx,
                    server_settings,
                    clipboard,
                    extension_host,
                    connection_client_id,
                )
                .await?
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
                handle_focus_pane(r, sessions, outbound_tx, connection_client_id).await?
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
                handle_resize_layout(r, sessions, outbound_tx, connection_client_id).await?
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
                handle_paste(r, sessions, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SetClipboard(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_set_clipboard(r, sessions, clipboard, connection_client_id).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SetPaneTitle(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_set_pane_title(r, sessions, connection_client_id).await?
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
                handle_zoom_pane(r, sessions, outbound_tx, connection_client_id).await?
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

    extension_host.bind_sessions(sessions);
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

/// §9 通过 outbound channel 把 Notification 封装成 Envelope 推送。
fn send_notification_envelope(
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    notification: Notification,
) -> Result<(), mpsc::error::SendError<Envelope>> {
    let envelope = Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
        payload: Some(EnvelopePayload::Notification(notification)),
    };
    outbound_tx.send(envelope)
}
fn start_session_snapshot_watch(
    session_id: String,
    cwd: String,
    sessions: Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) {
    crate::rt::spawn(async move {
        let session_id_for_start = session_id.clone();
        let cwd_for_start = cwd.clone();
        let result = crate::rt::spawn_blocking(move || {
            crate::snapshot::start(&session_id_for_start, &cwd_for_start)
        })
        .await;
        match result {
            Ok(Ok(Some(watch))) => {
                let mut live = sessions.write();
                if let Some(session) = live.iter_mut().find(|session| session.id == session_id) {
                    session.snapshot_watch = Some(watch);
                }
            }
            Ok(Ok(None)) => {
                zlog::info!(
                    "shadow snapshot not armed: session={} cwd={}",
                    session_id,
                    cwd
                );
            }
            Ok(Err(error)) => {
                zlog::warn!(
                    "shadow snapshot start failed: session={} cwd={} error={}",
                    session_id,
                    cwd,
                    error
                );
            }
            Err(error) => {
                zlog::warn!(
                    "shadow snapshot task panicked: session={} error={}",
                    session_id,
                    error
                );
            }
        }
    });
}
#[cfg(any(target_family = "wasm", not(feature = "desktop")))]
static NEXT_WASM_RUNTIME_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

#[cfg(any(target_family = "wasm", not(feature = "desktop")))]
fn wasm_runtime_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        NEXT_WASM_RUNTIME_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

async fn handle_create_session(
    req: &CreateSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    #[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
    let id = nanoid::nanoid!();
    #[cfg(any(target_family = "wasm", not(feature = "desktop")))]
    let id = wasm_runtime_id("session");
    let mut session = crate::session::Session::new(id.clone(), req.name.clone(), req.cwd.clone());

    // §16.6 spec 要求:每个新 session 自动创建一个 default tab,
    // 否则客户端 spawn_pane 时没有 tab_id 可用。
    let default_tab_id = "tab-0".to_string();
    session.add_tab(default_tab_id.clone(), req.name.clone());
    session.focused_tab = Some(default_tab_id);

    sessions.write().push(session);
    start_session_snapshot_watch(id.clone(), req.cwd.clone(), sessions.clone());

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
fn handle_list_recovery_candidates(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: &Arc<parking_lot::Mutex<Connection>>,
) -> anyhow::Result<ResponseBody> {
    let scan = crate::persistence::recovery_candidates(&db.lock())?;
    for error in &scan.rejected {
        tracing::warn!(%error, "rejected invalid mux recovery candidate");
    }
    let rejected = scan.rejected;
    let live_ids = sessions
        .read()
        .iter()
        .map(|session| session.id.clone())
        .collect::<std::collections::HashSet<_>>();
    Ok(ResponseBody::RecoveryCandidates(
        ListRecoveryCandidatesResponse {
            candidates: scan
                .candidates
                .into_iter()
                .filter(|candidate| !live_ids.contains(&candidate.id))
                .map(|candidate| RecoveryCandidateInfo {
                    id: candidate.id,
                    name: candidate.name,
                    cwd: candidate.cwd,
                    // 类型化 cutover 后所有候选都携带完整 tab/pane 元数据;
                    // 旧格式行在扫描阶段就被拒绝, 不会以 incomplete 候选发布。
                    metadata_complete: true,
                    pane_ids: candidate.layout.pane_ids(),
                })
                .collect(),
            rejected,
        },
    ))
}

static RECOVERY_RESERVATIONS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

struct RecoveryReservation(String);

impl RecoveryReservation {
    fn acquire(session_id: &str) -> anyhow::Result<Self> {
        let mut reservations = RECOVERY_RESERVATIONS.lock();
        anyhow::ensure!(
            reservations.insert(session_id.to_string()),
            "session recovery already in progress: {session_id}"
        );
        Ok(Self(session_id.to_string()))
    }
}

impl Drop for RecoveryReservation {
    fn drop(&mut self) {
        RECOVERY_RESERVATIONS.lock().remove(&self.0);
    }
}

fn handle_confirm_recovery(
    request: &ConfirmRecoveryRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: &Arc<parking_lot::Mutex<Connection>>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
) -> anyhow::Result<ResponseBody> {
    let candidate = {
        let connection = db.lock();
        let scan = crate::persistence::recovery_candidates(&connection)?;
        scan.candidates
            .into_iter()
            .find(|candidate| candidate.id == request.session_id)
            .ok_or_else(|| {
                anyhow::anyhow!("recovery candidate not found: {}", request.session_id)
            })?
    };
    anyhow::ensure!(
        !sessions
            .read()
            .iter()
            .any(|session| session.id == candidate.id),
        "session already live: {}",
        candidate.id
    );
    let _reservation = RecoveryReservation::acquire(&candidate.id)?;

    let mut spawned = Vec::with_capacity(candidate.panes.len());
    for pane in &candidate.panes {
        // Recovery intentionally passes no prior command: only a fresh default
        // shell may be started after explicit confirmation.
        let fresh = crate::pane::Pane::spawn_with_session(
            pane.id.clone(),
            candidate.id.clone(),
            pane.cwd.clone(),
            pane.cols,
            pane.rows,
            None,
            server_settings.scrollback_lines(),
        )?;
        fresh.set_title(pane.title.clone());
        spawned.push(fresh);
    }

    let mut session = crate::session::Session::new(
        candidate.id.clone(),
        candidate.name.clone(),
        candidate.cwd.clone(),
    );
    session.layout = candidate.layout.clone();
    session.focused_tab = candidate.focused_tab.clone();
    session.focused_pane = candidate.focused_pane.clone();
    for (id, title, pane_ids) in &candidate.tabs {
        session.tabs.insert(
            id.clone(),
            crate::session::Tab {
                id: id.clone(),
                title: title.clone(),
                pane_ids: pane_ids.clone(),
            },
        );
    }
    for pane in &spawned {
        session.panes.write().insert(pane.id.clone(), pane.clone());
    }
    validate_recovered_session(&session)?;

    {
        let mut live = sessions.write();
        anyhow::ensure!(
            !live.iter().any(|session| session.id == candidate.id),
            "session became live while recovery was in progress: {}",
            candidate.id
        );
        live.push(session);
    }
    start_session_snapshot_watch(
        candidate.id.clone(),
        candidate.cwd.clone(),
        sessions.clone(),
    );
    for pane in &spawned {
        let live = sessions.read();
        let session = live
            .iter()
            .find(|session| session.id == candidate.id)
            .ok_or_else(|| anyhow::anyhow!("recovered session disappeared"))?;
        register_pane_with_session_subscribers(session, pane);
        drop(live);
        install_pane_clipboard_hook(pane, sessions, clipboard);
        install_pane_exit_hook(pane, sessions, candidate.id.clone(), pane.id.clone());
    }
    // §16.8 The recovered session and its panes were created inside this
    // handler, so dispatch's start-of-request bind never saw them; bind now so
    // the extension host observes the session layout change that follows.
    extension_host.bind_sessions(sessions);
    broadcast_layout_changed(sessions, &candidate.id);

    Ok(ResponseBody::RecoveryConfirmed(ConfirmRecoveryResponse {
        session_id: candidate.id,
        pane_ids: spawned.iter().map(|pane| pane.id.clone()).collect(),
    }))
}

fn validate_recovered_session(session: &crate::session::Session) -> anyhow::Result<()> {
    let mut layout_panes = session.layout.pane_ids();
    layout_panes.sort();
    let mut registry_panes = session.panes.read().keys().cloned().collect::<Vec<_>>();
    registry_panes.sort();
    anyhow::ensure!(
        layout_panes == registry_panes,
        "recovered layout does not match panes"
    );
    anyhow::ensure!(
        session
            .tabs
            .values()
            .flat_map(|tab| &tab.pane_ids)
            .all(|pane_id| { registry_panes.binary_search(pane_id).is_ok() }),
        "recovered tab references an unknown pane"
    );
    anyhow::ensure!(
        registry_panes.iter().all(|pane_id| {
            session
                .tabs
                .values()
                .any(|tab| tab.pane_ids.contains(pane_id))
        }),
        "recovered pane is not assigned to a tab"
    );
    if let Some(focused) = &session.focused_pane {
        anyhow::ensure!(
            registry_panes.binary_search(focused).is_ok(),
            "invalid focused pane"
        );
    }
    if let Some(focused) = &session.focused_tab {
        anyhow::ensure!(session.tabs.contains_key(focused), "invalid focused tab");
    }
    Ok(())
}

async fn handle_kill_session(
    req: &KillSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    forward_tasks: &Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>>,
    trust: ConnectionTrust,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
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
            #[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
            let minted = if let Some(identity) = &req.identity {
                if !identity.client_id.is_empty() {
                    format!("{}-{}", identity.client_id, nanoid::nanoid!(8))
                } else {
                    format!("client-{}-{}", std::process::id(), nanoid::nanoid!(8))
                }
            } else {
                format!("client-{}-{}", std::process::id(), nanoid::nanoid!(8))
            };
            #[cfg(any(target_family = "wasm", not(feature = "desktop")))]
            let minted = req
                .identity
                .as_ref()
                .filter(|identity| !identity.client_id.is_empty())
                .map(|identity| {
                    format!("{}-{}", identity.client_id, wasm_runtime_id("client"))
                })
                .unwrap_or_else(|| wasm_runtime_id("client"));
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
                    snapshot: None,
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

    // The startup render can race attach; repaint after this subscriber is
    // registered so server-owned chrome is available on every new connection.
    extension_host.request_render();

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
        let handle = crate::rt::spawn(async move {
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
    forward_tasks: &Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>>,
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
        crate::rt::spawn(async move {
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
    #[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
    let window_id = format!("win-{}-{}", std::process::id(), nanoid::nanoid!());
    #[cfg(any(target_family = "wasm", not(feature = "desktop")))]
    let window_id = wasm_runtime_id("window");

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
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
    #[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
    let pane_id = nanoid::nanoid!();
    #[cfg(any(target_family = "wasm", not(feature = "desktop")))]
    let pane_id = wasm_runtime_id("pane");

    // §3.1 转换 ShellCommand → pane::ShellCommand
    let shell_cmd = req.command.as_ref().map(|c| crate::pane::ShellCommand {
        program: c.program.clone(),
        args: c.args.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    });

    // Validate the authoritative session before spawning a child. Otherwise a
    // stale CLI request can create an unregistered PTY and only fail later.
    let session_cwd = {
        let sessions_r = sessions.read();
        sessions_r
            .iter()
            .find(|session| session.id == req.session_id)
            .map(|session| session.cwd.clone())
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.session_id))?
    };
    let cwd = req.cwd.clone().unwrap_or(session_cwd);

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
        let session = sessions_w
            .iter_mut()
            .find(|session| session.id == req.session_id)
            .ok_or_else(|| {
                anyhow::anyhow!("session removed while spawning pane: {}", req.session_id)
            })?;
        if session.layout.is_empty_root() {
            session.layout =
                crate::layout::LayoutTree::with_pane(format!("node-{}", pane_id), pane_id.clone());
        } else {
            let anchor = session
                .focused_pane
                .as_ref()
                .filter(|focused| session.layout.root.find_pane(focused).is_some())
                .cloned()
                .or_else(|| {
                    session
                        .layout
                        .pane_ids()
                        .into_iter()
                        .find(|id| !id.is_empty())
                })
                .ok_or_else(|| anyhow::anyhow!("session layout has no pane anchor"))?;
            if let Err(error) = session.layout.split(
                &anchor,
                pane_id.clone(),
                crate::layout::SplitDirection::TopBottom,
            ) {
                return Ok(ResponseBody::Error(format!("spawn pane rejected: {error}")));
            }
        }

        session.panes.write().insert(pane_id.clone(), pane);
        session.set_focused_pane(pane_id.clone());
        session.focused_tab = Some(req.tab_id.clone());

        let tab = session
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

    // §16.8 Re-bind the extension host before the new pane can emit anything:
    // the pane's notification hook is only installed by bind_sessions, which
    // ran at dispatch start while this pane did not exist yet. Without this,
    // pane-level events (title/output/dirty) from the fresh PTY would miss the
    // host until the next request's bind.
    extension_host.bind_sessions(sessions);

    zlog::info!("pane spawned: id={} session={}", pane_id, req.session_id);

    // Publish existence before installing the exit hook. If the command has
    // already exited, late hook installation immediately replays removal,
    // preserving per-pane Added -> Removed ordering.
    broadcast_pane_added(sessions, &req.session_id, &pane_id, &req.tab_id);
    // Install only after PaneAdded publication. A command that already exited
    // replays cleanup here, preserving Added -> Removed ordering.
    let pane_for_exit = {
        let sessions_r = sessions.read();
        sessions_r
            .iter()
            .find(|session| session.id == req.session_id)
            .and_then(|session| session.panes.read().get(&pane_id).cloned())
    };
    if let Some(pane) = pane_for_exit {
        install_pane_exit_hook(&pane, sessions, req.session_id.clone(), pane_id.clone());
    }
    broadcast_layout_changed(sessions, &req.session_id);

    Ok(ResponseBody::PaneId(pane_id))
}

/// §3.10 Split an existing pane and optionally run a command in the new pane.
async fn handle_split_pane(
    req: &SplitPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
    let direction = match req.direction {
        1 => crate::layout::SplitDirection::LeftRight,
        2 => crate::layout::SplitDirection::TopBottom,
        _ => crate::layout::SplitDirection::LeftRight,
    };
    #[cfg(all(not(target_family = "wasm"), feature = "desktop"))]
    let new_pane_id = nanoid::nanoid!();
    #[cfg(any(target_family = "wasm", not(feature = "desktop")))]
    let new_pane_id = wasm_runtime_id("pane");

    let mut sessions_w = sessions.write();
    for session in sessions_w.iter_mut() {
        if session.layout.root.find_pane(&req.pane_id).is_some() {
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
            let parent_tab_id = session
                .tabs
                .iter()
                .find(|(_, tab)| tab.pane_ids.contains(&req.pane_id))
                .map(|(id, _)| id.clone())
                .ok_or_else(|| anyhow::anyhow!("pane is not assigned to a tab: {}", req.pane_id))?;
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
            // Only mutate the authoritative layout after PTY creation has
            // succeeded; on rejection `pane` drops here and kills the child.
            if let Err(error) = session
                .layout
                .split(&req.pane_id, new_pane_id.clone(), direction)
            {
                return Ok(ResponseBody::Error(format!("split pane rejected: {error}")));
            }
            session.panes.write().insert(new_pane_id.clone(), pane);
            session.set_focused_pane(new_pane_id.clone());
            session.focused_tab = Some(parent_tab_id.clone());

            let parent_tab_id_for_broadcast = parent_tab_id.clone();
            if let Some(tab) = session.tabs.get_mut(&parent_tab_id)
                && !tab.pane_ids.contains(&new_pane_id)
            {
                tab.pane_ids.push(new_pane_id.clone());
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
            // §16.8 The new pane did not exist when dispatch's bind_sessions
            // ran; re-bind before the PaneAdded fan-out so the extension host
            // receives pane-level events from the fresh pane from the start.
            extension_host.bind_sessions(sessions);
            broadcast_pane_added(
                sessions,
                &session_id_for_broadcast,
                &new_pane_id,
                &parent_tab_id_for_broadcast,
            );
            let pane_for_exit = {
                let sessions_r = sessions.read();
                sessions_r
                    .iter()
                    .find(|session| session.id == session_id_for_broadcast)
                    .and_then(|session| session.panes.read().get(&new_pane_id).cloned())
            };
            if let Some(pane) = pane_for_exit {
                install_pane_exit_hook(
                    &pane,
                    sessions,
                    session_id_for_broadcast.clone(),
                    new_pane_id.clone(),
                );
            }
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
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }

    let mut removed = false;
    let mut session_id = None;
    {
        let mut sessions_w = sessions.write();
        for session in sessions_w.iter_mut() {
            if session.panes.read().contains_key(&req.pane_id) {
                removed = session.remove_pane(&req.pane_id)?;
                session_id = Some(session.id.clone());
                if removed {
                    zlog::info!("pane closed: id={}", req.pane_id);
                }
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
        broadcast_pane_removed(sessions, &sid, &req.pane_id, 0);
        broadcast_layout_changed(sessions, &sid);
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 聚焦 pane
async fn handle_focus_pane(
    req: &FocusPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    _outbound_tx: &mpsc::UnboundedSender<Envelope>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
/// §16.9 响应帧校验余量: 覆盖 Envelope/Response 包装 (版本、request_id、
/// oneof tags、行数与版本号) 的保守编码开销。`encoded_len` 检查以此为基准
/// 保证整个帧落在 `MAX_FRAME_PAYLOAD` 之内, 客户端不会因帧超限掐断连接。
const RESPONSE_FRAME_HEADROOM: usize = 256;

/// §16.9 获取回滚缓冲区历史行
async fn handle_fetch_scrollback(
    req: &FetchScrollbackRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            let (lines, total, version) = match pane.fetch_scrollback_checked(
                req.from_line,
                req.direction,
                req.count,
            ) {
                Ok(result) => result,
                Err(error) => return Ok(ResponseBody::Error(error.rpc_message())),
            };
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
            // §16.9 编码后硬校验: 若实际序列化大小 (含极端 hyperlink 内容) 超出
            // 帧上限, 返回 typed error 而不是让写循环发出客户端无法接受的巨型帧。
            if resp.encoded_len() > MAX_FRAME_PAYLOAD - RESPONSE_FRAME_HEADROOM {
                return Ok(ResponseBody::Error(
                    "scrollback response exceeds frame limit".to_string(),
                ));
            }
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

/// §3.10 列出 pane 里由 OSC 133 marker 划出的命令。
///
/// 不返回 `Result`: 唯一的失败是找不到 pane, 用 `?` 会把整条连接拆掉, 客户端
/// 只能看到一句 "connection closed"。
fn handle_list_commands(
    request: &ListCommandsRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> ResponseBody {
    let pane = {
        let sessions_r = sessions.read();
        sessions_r
            .iter()
            .find_map(|session| session.panes.read().get(&request.pane_id).cloned())
    };
    let Some(pane) = pane else {
        return ResponseBody::Error(format!("pane not found: {}", request.pane_id));
    };

    let (markers, history_size) = shell_marker_snapshot(&pane);
    let recorded_markers = u32::try_from(markers.len()).unwrap_or(u32::MAX);
    let mut commands = group_shell_markers(&markers);
    let max_results = request.max_results as usize;
    if max_results != 0 && commands.len() > max_results {
        commands.drain(..commands.len() - max_results);
    }
    ResponseBody::Commands(ListCommandsResponse {
        commands,
        history_size,
        recorded_markers,
    })
}

/// §3.3 一个 pane 的全部 marker, 每个配上它此刻的 tmux 行号 (不可寻址时为 None)。
///
/// `shell_marker_positions` 与 scrollback 大小是两段独立的临界区。两者之间的
/// 追加是无害的: 追加不动历史下标, 只让 tmux 行号更负一点, 而那正是更新的事实。
/// 一次 addressing 作废则不然 —— 它在已解析出的下标底下重排了历史。所以这里
/// 复查 epoch, 配不上的就报"没有行号", 而不是报一个错的行号。
fn shell_marker_snapshot(pane: &crate::pane::Pane) -> (Vec<(ShellMarker, Option<i64>)>, u32) {
    const ATTEMPTS: usize = 4;
    for _ in 0..ATTEMPTS {
        let epoch = pane.row_addressing_epoch();
        let positions = pane.shell_marker_positions();
        let (_, history_size, _) = pane.fetch_scrollback(0, 1, 0);
        if pane.row_addressing_epoch() == epoch {
            let located = positions
                .into_iter()
                .map(|(marker, position)| (marker, tmux_line(position, history_size)))
                .collect();
            return (located, history_size);
        }
    }
    let (_, history_size, _) = pane.fetch_scrollback(0, 1, 0);
    let markers = pane
        .shell_markers()
        .into_iter()
        .map(|marker| (marker, None))
        .collect();
    (markers, history_size)
}

/// §3.10 把一个 marker 位置换算成 tmux 行号: 可见区首行是 0, 负数进历史。
///
/// 历史下标必然小于 scrollback 大小, 所以换算结果必然是负数。真出现非负值就
/// 说明下标和 scrollback 大小配错了 —— 宁可说"不知道", 也不能交出一个看起来
/// 像可见区行号的历史行号。
fn tmux_line(position: crate::pane::ShellMarkerPosition, history_size: u32) -> Option<i64> {
    match position {
        crate::pane::ShellMarkerPosition::History { index } => {
            Some(i64::from(index) - i64::from(history_size)).filter(|line| *line < 0)
        }
        crate::pane::ShellMarkerPosition::Viewport { line } => Some(i64::from(line)),
        crate::pane::ShellMarkerPosition::Unavailable => None,
    }
}

/// §3.3 OSC 133 在一条命令内部固定按 A → B → C → D 的顺序发送。
fn marker_slot(kind: ShellMarkerKind) -> usize {
    match kind {
        ShellMarkerKind::PromptStart => 0,
        ShellMarkerKind::CommandStart => 1,
        ShellMarkerKind::OutputStart => 2,
        ShellMarkerKind::CommandEnd => 3,
    }
}

/// §3.10 把一串 marker 归拢成命令。
///
/// 一条命令内部 kind 严格递增, 所以一个不再前进的 kind 就是下一条命令的开始。
/// shell 可以任意跳过 marker (有的只发 A 和 D), 命令还在跑时也不会有 D, 因此
/// 一条命令就是两次"重新开始"之间到达的那些 marker, 缺谁都不影响其余的。
fn group_shell_markers(markers: &[(ShellMarker, Option<i64>)]) -> Vec<CommandRange> {
    let mut commands: Vec<CommandRange> = Vec::new();
    let mut current: Option<(usize, CommandRange)> = None;
    for (marker, line) in markers {
        let slot = marker_slot(marker.kind);
        let mut command = match current.take() {
            Some((previous_slot, command)) if slot > previous_slot => command,
            Some((_, finished)) => {
                commands.push(finished);
                CommandRange {
                    id: marker.sequence,
                    ..Default::default()
                }
            }
            None => CommandRange {
                id: marker.sequence,
                ..Default::default()
            },
        };
        let recorded = Some(CommandMarker {
            line: *line,
            column: marker.column,
        });
        match marker.kind {
            ShellMarkerKind::PromptStart => command.prompt = recorded,
            ShellMarkerKind::CommandStart => command.command = recorded,
            ShellMarkerKind::OutputStart => command.output_start = recorded,
            ShellMarkerKind::CommandEnd => {
                command.command_end = recorded;
                command.exit_code = marker.exit_code;
            }
        }
        current = Some((slot, command));
    }
    if let Some((_, command)) = current {
        commands.push(command);
    }
    // 只有一个 prompt start 的那些是"画了个提示符", 不是命令 —— zsh 每次重画
    // 提示符都会发一个 A。丢掉它们, 列表里才只剩真的跑过的东西。
    commands.retain(|command| {
        command.command.is_some() || command.output_start.is_some() || command.command_end.is_some()
    });
    commands
}

async fn handle_fetch_grid_update(
    req: &FetchGridUpdateRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            let (update, output_sequence) = pane.fetch_grid_update(req.since_generation);
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
                    output_sequence,
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
                    output_sequence,
                },
                crate::grid_sync::GridUpdate::NoChange(current_gen) => FetchGridUpdateResponse {
                    from_generation: current_gen,
                    to_generation: current_gen,
                    update: None,
                    output_sequence,
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

    fn normalized_weights(ratios: &[f32], child_count: usize) -> Vec<f32> {
        if ratios.len() != child_count || ratios.is_empty() {
            return vec![1.0 / child_count.max(1) as f32; child_count];
        }
        let sum: f32 = ratios.iter().sum();
        if !sum.is_finite()
            || sum <= 0.0
            || ratios
                .iter()
                .any(|ratio| !ratio.is_finite() || *ratio < 0.0)
        {
            return vec![1.0 / child_count.max(1) as f32; child_count];
        }
        ratios.iter().map(|ratio| ratio / sum).collect()
    }

    fn append_axis_children(
        node: &LayoutNode,
        direction: SplitDirection,
        weight: f32,
        proto_children: &mut Vec<ProtoNode>,
        proto_ratios: &mut Vec<f32>,
    ) {
        if let LayoutNode::Split {
            direction: child_direction,
            children,
            ratios,
            ..
        } = node
            && *child_direction == direction
        {
            let weights = normalized_weights(ratios, children.len());
            for (child, child_weight) in children.iter().zip(weights) {
                append_axis_children(
                    child,
                    direction,
                    weight * child_weight,
                    proto_children,
                    proto_ratios,
                );
            }
            return;
        }

        if let Some(child) = convert(node) {
            proto_children.push(child);
            proto_ratios.push(weight);
        }
    }

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
                let weights = normalized_weights(ratios, children.len());
                let mut proto_children = Vec::new();
                let mut proto_ratios = Vec::new();
                for (child, weight) in children.iter().zip(weights) {
                    append_axis_children(
                        child,
                        *direction,
                        weight,
                        &mut proto_children,
                        &mut proto_ratios,
                    );
                }
                let proto_dir = match direction {
                    SplitDirection::LeftRight => ProtoDir::LeftRight,
                    SplitDirection::TopBottom => ProtoDir::TopBottom,
                } as i32;
                ProtoNode {
                    id: id.clone(),
                    node: Some(Node::Split(SplitNode {
                        direction: proto_dir,
                        children: proto_children,
                        ratios: proto_ratios,
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
        let removed = {
            let mut sessions_w = sessions.write();
            match sessions_w
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                Some(session) => match session.remove_pane(&pane_id) {
                    Ok(removed) => removed,
                    Err(error) => {
                        tracing::error!(%error, %pane_id, "failed to clean up exited pane");
                        false
                    }
                },
                None => false,
            }
        };
        if removed {
            broadcast_pane_removed(&sessions, &session_id, &pane_id, 0);
            broadcast_layout_changed(&sessions, &session_id);
        }
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

/// §3.3 Shared pre-mutation attachment guard for every mutating RPC handler.
///
/// `client_still_attached` already encodes the two allowed pre-attach shapes:
/// a connection whose id is `None` (tmux-style one-shot CLI commands such as
/// `send-keys` / `split-window` driven by `$Z3RM_PANE`, and voluntary-detach
/// sockets whose id was cleared) and a connection whose id is registered with
/// some session. The only failing shape is a connection whose id is set but
/// registered with no session anymore — a steal-kicked or otherwise stale
/// client. Every mutating handler MUST gate on this helper before touching
/// server state or fanning out any notification, so a kicked client gets
/// `ResponseBody::Error` instead of silently driving a session it no longer
/// belongs to.
fn ensure_mutation_allowed(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> Result<(), String> {
    if client_still_attached(sessions, connection_client_id) {
        Ok(())
    } else {
        Err("client not attached (kicked or detached)".to_string())
    }
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
    // §4 `PathBuf::join("")` appends a separator, so joining an empty suffix
    // turns "/root/file.txt" into "/root/file.txt/". `Path` equality ignores
    // the trailing separator, but `compute_path_hash` hashes the string — the
    // two spellings hash differently and every version lookup for a file that
    // still exists on disk misses.
    if suffix.as_os_str().is_empty() {
        return Ok(canonical_ancestor);
    }
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

/// §4 Turn a shadow-snapshot handler result into a response the client can read.
fn shadow_response(result: anyhow::Result<ResponseBody>) -> ResponseBody {
    match result {
        Ok(body) => body,
        Err(error) => ResponseBody::Error(format!("{error:#}")),
    }
}

async fn handle_list_changed_files(
    request: &mux_protocol::ListChangedFilesRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let (watch, _root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let changed = crate::rt::spawn_blocking(move || watch.list_changed_files())
        .await
        .context("joining shadow list-changed-files request")??;
    Ok(ResponseBody::ChangedFiles(
        mux_protocol::ListChangedFilesResponse {
            files: changed
                .into_iter()
                .map(|change| mux_protocol::ChangedFile {
                    path: change.path.to_string_lossy().into_owned(),
                    version_count: change.version_count,
                    latest_seq_no: change.latest_seq_no,
                })
                .collect(),
        },
    ))
}

async fn handle_list_file_versions(
    request: &mux_protocol::ListFileVersionsRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let (watch, root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let path = resolve_path_within_root(&root, &request.path)?;
    let versions = crate::rt::spawn_blocking(move || watch.list_versions(path))
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
    let content = crate::rt::spawn_blocking(move || watch.get_version(path, version_id))
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
    let (watch, root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let path = resolve_path_within_root(&root, &request.path)?;
    let version_id = request.version_id;
    let declined = crate::rt::spawn_blocking(move || watch.decline(path, version_id))
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
    match std::fs::File::open(&path) {
        Ok(mut file) => match (|| -> anyhow::Result<_> {
            let total_bytes = file.metadata()?.len();
            let response = read_file_response(req, &mut file, total_bytes)?;
            anyhow::ensure!(
                file.metadata()?.len() == total_bytes,
                "file size changed while reading"
            );
            Ok(response)
        })() {
            Ok(response) => Ok(ResponseBody::FileContent(response)),
            Err(error) => Ok(ResponseBody::Error(format!("read_file: {error:#}"))),
        },
        Err(e) => Ok(ResponseBody::Error(format!("read_file: {}", e))),
    }
}

fn read_file_response(
    req: &mux_protocol::ReadFileRequest,
    file: &mut (impl std::io::Read + std::io::Seek),
    total_bytes: u64,
) -> anyhow::Result<mux_protocol::ReadFileResponse> {
    let line_page_requested = req.offset_line.is_some() || req.max_lines.is_some();
    let byte_page_requested = req.offset_bytes.is_some() || req.max_bytes.is_some();
    anyhow::ensure!(
        !(line_page_requested && byte_page_requested),
        "line and byte pagination cannot be combined"
    );

    file.seek(std::io::SeekFrom::Start(0))?;
    let mut prefix = Vec::with_capacity(8192);
    (&mut *file).take(8192).read_to_end(&mut prefix)?;
    let is_binary = detect_binary(&prefix);
    let encoding = if is_binary { "binary" } else { "utf-8" }.to_string();

    if byte_page_requested {
        let max_bytes = req
            .max_bytes
            .unwrap_or(mux_protocol::DEFAULT_READ_FILE_PAGE_BYTES);
        anyhow::ensure!(max_bytes > 0, "max_bytes must be at least 1");
        anyhow::ensure!(
            max_bytes <= mux_protocol::MAX_READ_FILE_PAGE_BYTES,
            "max_bytes exceeds the per-page limit of {}",
            mux_protocol::MAX_READ_FILE_PAGE_BYTES
        );

        let offset_bytes = req.offset_bytes.unwrap_or(0);
        anyhow::ensure!(
            offset_bytes <= total_bytes,
            "offset_bytes is beyond the end of the file"
        );
        let start = offset_bytes;
        let page_bytes = total_bytes.saturating_sub(start).min(max_bytes as u64) as usize;
        file.seek(std::io::SeekFrom::Start(start))?;
        let mut content = Vec::with_capacity(page_bytes);
        (&mut *file)
            .take(page_bytes as u64)
            .read_to_end(&mut content)?;
        anyhow::ensure!(
            content.len() == page_bytes,
            "file changed while reading byte page"
        );
        let end = start + page_bytes as u64;
        let next_offset_bytes = (end < total_bytes).then_some(end);
        return Ok(mux_protocol::ReadFileResponse {
            content,
            is_binary,
            encoding,
            offset_line: 0,
            next_offset_line: None,
            total_lines: 0,
            offset_bytes,
            next_offset_bytes,
            total_bytes,
        });
    }

    if line_page_requested {
        let max_lines = req
            .max_lines
            .unwrap_or(mux_protocol::DEFAULT_READ_FILE_PAGE_LINES);
        anyhow::ensure!(max_lines > 0, "max_lines must be at least 1");
        anyhow::ensure!(
            max_lines <= mux_protocol::MAX_READ_FILE_PAGE_LINES,
            "max_lines exceeds the per-page limit of {}",
            mux_protocol::MAX_READ_FILE_PAGE_LINES
        );

        file.seek(std::io::SeekFrom::Start(0))?;
        let offset_line = req.offset_line.unwrap_or(0);
        let (content, total_lines, start) = read_line_page(
            std::io::BufReader::new(file),
            total_bytes,
            offset_line,
            max_lines,
        )?;
        let returned_lines = max_lines.min(total_lines.saturating_sub(offset_line));
        let next_offset_line = (offset_line.saturating_add(returned_lines) < total_lines)
            .then_some(offset_line.saturating_add(returned_lines));
        return Ok(mux_protocol::ReadFileResponse {
            content,
            is_binary,
            encoding,
            offset_line,
            next_offset_line,
            total_lines,
            offset_bytes: start,
            next_offset_bytes: None,
            total_bytes,
        });
    }

    // Neither pagination mode means a legacy request. The size check preserves
    // old clients without allowing them to force an oversized allocation/frame.
    anyhow::ensure!(
        total_bytes < mux_protocol::MAX_FRAME_PAYLOAD as u64,
        "legacy full-file request exceeds the frame limit; use pagination"
    );
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut content = Vec::with_capacity(total_bytes as usize);
    (&mut *file)
        .take(mux_protocol::MAX_FRAME_PAYLOAD as u64)
        .read_to_end(&mut content)?;
    anyhow::ensure!(
        content.len() as u64 == total_bytes,
        "file changed while reading legacy response"
    );
    Ok(mux_protocol::ReadFileResponse {
        total_lines: u32::try_from(logical_line_count(&content)).unwrap_or(u32::MAX),
        content,
        is_binary,
        encoding,
        offset_line: 0,
        next_offset_line: None,
        offset_bytes: 0,
        next_offset_bytes: None,
        total_bytes,
    })
}

fn read_line_page(
    mut reader: impl std::io::BufRead,
    expected_bytes: u64,
    offset_line: u32,
    max_lines: u32,
) -> anyhow::Result<(Vec<u8>, u32, u64)> {
    let page_end_line = u64::from(offset_line) + u64::from(max_lines);
    let mut current_line = 0u64;
    let mut absolute_offset = 0u64;
    let mut page_start = None;
    let mut content = Vec::new();
    let mut saw_byte = false;
    let mut last_byte = None;

    loop {
        let consumed = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                break;
            }
            for byte in buffer {
                if current_line == u64::from(offset_line) && page_start.is_none() {
                    page_start = Some(absolute_offset);
                }
                if current_line >= u64::from(offset_line) && current_line < page_end_line {
                    anyhow::ensure!(
                        content.len() < mux_protocol::MAX_READ_FILE_PAGE_BYTES as usize,
                        "requested line page exceeds the byte limit of {}; use byte pagination",
                        mux_protocol::MAX_READ_FILE_PAGE_BYTES
                    );
                    content.push(*byte);
                }
                saw_byte = true;
                last_byte = Some(*byte);
                absolute_offset = absolute_offset.saturating_add(1);
                if *byte == b'\n' {
                    current_line = current_line.saturating_add(1);
                }
            }
            buffer.len()
        };
        reader.consume(consumed);
    }

    anyhow::ensure!(
        absolute_offset == expected_bytes,
        "file changed while reading line page"
    );
    let total_lines = current_line + u64::from(saw_byte && last_byte != Some(b'\n'));
    let total_lines = u32::try_from(total_lines)
        .map_err(|_| anyhow::anyhow!("file has more than {} logical lines", u32::MAX))?;
    Ok((content, total_lines, page_start.unwrap_or(expected_bytes)))
}

fn logical_line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(bytes.last() != Some(&b'\n'))
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
    let listing = crate::rt::spawn_blocking(move || {
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
    let read_dir = std::fs::read_dir(path)
        .with_context(|| format!("reading directory: {}", path.display()))?;
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

/// §16.8 / §16.12 InstallExtension: validate and load a server-side extension
/// on the daemon's extension host.
///
/// The response is always the typed `ExtensionInstalled` — an empty `Error`
/// body reads as success on the client and would make
/// `mux::sync_extensions_to_remote` report an install that never happened.
/// Validation failures (bad manifest, client-only side, unsafe archive) come
/// back as `success=false` with the underlying error.
async fn handle_install_extension(
    req: &mux_protocol::InstallExtensionRequest,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
) -> anyhow::Result<ResponseBody> {
    let result = extension_host.install_extension(req).await;
    let (success, error) = match &result {
        Ok(()) => {
            zlog::info!("extension installed: name={}", req.name);
            (true, String::new())
        }
        Err(err) => {
            zlog::warn!(
                "extension install rejected: name={} error={:#}",
                req.name,
                err
            );
            (false, format!("{err:#}"))
        }
    };
    Ok(ResponseBody::ExtensionInstalled(
        mux_protocol::InstallExtensionResponse {
            name: req.name.clone(),
            success,
            error,
        },
    ))
}

/// §16.9 ExtensionChromeAction: route a click/change from server-rendered
/// chrome back to the authoritative daemon-side extension host. The daemon
/// validates the extension is loaded, not suspended, and the view id was
/// actually published to clients; any failure returns `accepted=false` with
/// a contextual error.
async fn handle_extension_chrome_action(
    req: &mux_protocol::ExtensionChromeActionRequest,
    extension_host: &Arc<crate::extension_host::ServerExtensionHost>,
) -> anyhow::Result<ResponseBody> {
    let result = extension_host.execute_chrome_action(req).await;
    let (accepted, error) = match &result {
        Ok(()) => {
            zlog::debug!(
                "chrome action accepted: extension={} view={} command={}",
                req.extension_id,
                req.view_id,
                req.command
            );
            (true, String::new())
        }
        Err(err) => {
            zlog::warn!(
                "chrome action rejected: extension={} view={} command={} error={:#}",
                req.extension_id,
                req.view_id,
                req.command,
                err
            );
            (false, format!("{err:#}"))
        }
    };
    Ok(ResponseBody::ExtensionChromeActionResult(
        mux_protocol::ExtensionChromeActionResponse { accepted, error },
    ))
}

/// §3.10 RenameSession: 更新 session 名称。
async fn handle_rename_session(
    req: &mux_protocol::RenameSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
) -> anyhow::Result<ResponseBody> {
    if let Err(message) = ensure_mutation_allowed(sessions, connection_client_id) {
        return Ok(ResponseBody::Error(message));
    }
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
    use crate::pane::ShellMarkerPosition;

    fn read_file_request() -> mux_protocol::ReadFileRequest {
        mux_protocol::ReadFileRequest {
            path: "file.txt".to_string(),
            offset_line: None,
            max_lines: None,
            offset_bytes: None,
            max_bytes: None,
        }
    }

    fn read_file_page_for_test(
        request: &mux_protocol::ReadFileRequest,
        bytes: &[u8],
    ) -> anyhow::Result<mux_protocol::ReadFileResponse> {
        let mut cursor = std::io::Cursor::new(bytes);
        read_file_response(request, &mut cursor, bytes.len() as u64)
    }

    #[test]
    fn read_file_line_pages_preserve_boundaries() {
        let bytes = b"zero\none\ntwo\nthree";
        let first = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_line: Some(0),
                max_lines: Some(2),
                ..read_file_request()
            },
            bytes,
        )
        .expect("first page");
        assert_eq!(first.content, b"zero\none\n");
        assert_eq!(first.offset_line, 0);
        assert_eq!(first.next_offset_line, Some(2));
        assert_eq!(first.total_lines, 4);

        let last = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_line: first.next_offset_line,
                max_lines: Some(2),
                ..read_file_request()
            },
            bytes,
        )
        .expect("last page");
        assert_eq!(last.content, b"two\nthree");
        assert_eq!(last.next_offset_line, None);

        let past_end = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_line: Some(9),
                max_lines: Some(2),
                ..read_file_request()
            },
            bytes,
        )
        .expect("past-end page");
        assert!(past_end.content.is_empty());
        assert_eq!(past_end.offset_line, 9);
        assert_eq!(past_end.next_offset_line, None);
    }

    #[test]
    fn read_file_byte_pages_bound_payloads() {
        let bytes = b"0123456789";
        let page = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_bytes: Some(3),
                max_bytes: Some(4),
                ..read_file_request()
            },
            bytes,
        )
        .expect("byte page");
        assert_eq!(page.content, b"3456");
        assert_eq!(page.offset_bytes, 3);
        assert_eq!(page.next_offset_bytes, Some(7));
        assert_eq!(page.total_bytes, 10);

        let last = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_bytes: page.next_offset_bytes,
                max_bytes: Some(4),
                ..read_file_request()
            },
            bytes,
        )
        .expect("last byte page");
        assert_eq!(last.content, b"789");
        assert_eq!(last.next_offset_bytes, None);

        let past_end = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_bytes: Some(11),
                max_bytes: Some(4),
                ..read_file_request()
            },
            bytes,
        )
        .expect_err("byte offsets beyond EOF must be rejected");
        assert!(past_end.to_string().contains("beyond the end"));
    }

    #[test]
    fn read_file_rejects_degenerate_or_mixed_pages() {
        let zero_lines = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_line: Some(0),
                max_lines: Some(0),
                ..read_file_request()
            },
            b"text",
        )
        .expect_err("zero line pages cannot advance");
        assert!(zero_lines.to_string().contains("max_lines"));

        let zero_bytes = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_bytes: Some(0),
                max_bytes: Some(0),
                ..read_file_request()
            },
            b"text",
        )
        .expect_err("zero byte pages cannot advance");
        assert!(zero_bytes.to_string().contains("max_bytes"));

        let mixed = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_line: Some(0),
                max_lines: Some(1),
                offset_bytes: Some(0),
                max_bytes: Some(1),
                ..read_file_request()
            },
            b"text",
        )
        .expect_err("pagination modes are exclusive");
        assert!(mixed.to_string().contains("cannot be combined"));
    }

    #[test]
    fn read_file_sparse_file_stays_bounded() {
        let mut file = tempfile::tempfile().expect("temp file");
        let total_bytes = mux_protocol::MAX_FRAME_PAYLOAD as u64 * 2;
        file.set_len(total_bytes).expect("make sparse file");

        let page = read_file_response(
            &mux_protocol::ReadFileRequest {
                offset_bytes: Some(total_bytes - 3),
                max_bytes: Some(16),
                ..read_file_request()
            },
            &mut file,
            total_bytes,
        )
        .expect("tail page");
        assert_eq!(page.content, vec![0; 3]);
        assert_eq!(page.total_bytes, total_bytes);
        assert_eq!(page.next_offset_bytes, None);

        let error = read_file_response(&read_file_request(), &mut file, total_bytes)
            .expect_err("legacy request must not allocate a huge sparse file");
        assert!(error.to_string().contains("frame limit"));
    }

    #[test]
    fn read_file_line_page_rejects_an_oversized_line() {
        let bytes = vec![b'x'; mux_protocol::MAX_READ_FILE_PAGE_BYTES as usize + 1];
        let error = read_file_page_for_test(
            &mux_protocol::ReadFileRequest {
                offset_line: Some(0),
                max_lines: Some(1),
                ..read_file_request()
            },
            &bytes,
        )
        .expect_err("one logical line must not bypass the byte cap");
        assert!(error.to_string().contains("byte limit"));
    }

    fn shell_marker(
        sequence: u64,
        kind: ShellMarkerKind,
        column: u32,
        exit_code: Option<i32>,
    ) -> ShellMarker {
        ShellMarker {
            sequence,
            kind,
            absolute_row: sequence,
            column,
            exit_code,
            epoch: 1,
        }
    }

    #[test]
    fn history_indices_become_negative_tmux_lines_and_viewport_rows_stay_put() {
        assert_eq!(
            tmux_line(ShellMarkerPosition::History { index: 0 }, 100),
            Some(-100)
        );
        assert_eq!(
            tmux_line(ShellMarkerPosition::History { index: 99 }, 100),
            Some(-1)
        );
        assert_eq!(
            tmux_line(ShellMarkerPosition::Viewport { line: 0 }, 100),
            Some(0)
        );
        assert_eq!(tmux_line(ShellMarkerPosition::Unavailable, 100), None);
        // 历史下标必然小于 scrollback 大小; 配错了宁可说"不知道", 也不能交出一个
        // 看起来像可见区行号的历史行号。
        assert_eq!(
            tmux_line(ShellMarkerPosition::History { index: 7 }, 3),
            None
        );
    }

    #[test]
    fn markers_group_into_one_command_per_a_to_d_run() {
        let markers = [
            (
                shell_marker(1, ShellMarkerKind::PromptStart, 0, None),
                Some(-9),
            ),
            (
                shell_marker(2, ShellMarkerKind::CommandStart, 2, None),
                Some(-9),
            ),
            (
                shell_marker(3, ShellMarkerKind::OutputStart, 0, None),
                Some(-8),
            ),
            (
                shell_marker(4, ShellMarkerKind::CommandEnd, 0, Some(0)),
                Some(-5),
            ),
            (
                shell_marker(5, ShellMarkerKind::PromptStart, 0, None),
                Some(-5),
            ),
            (
                shell_marker(6, ShellMarkerKind::CommandStart, 2, None),
                Some(-5),
            ),
            (
                shell_marker(7, ShellMarkerKind::OutputStart, 0, None),
                Some(-4),
            ),
            (
                shell_marker(8, ShellMarkerKind::CommandEnd, 0, Some(1)),
                Some(-1),
            ),
        ];
        let commands = group_shell_markers(&markers);
        assert_eq!(commands.len(), 2, "{commands:?}");
        assert_eq!(commands[0].id, 1);
        assert_eq!(commands[0].exit_code, Some(0));
        assert_eq!(
            commands[0].output_start.as_ref().and_then(|item| item.line),
            Some(-8)
        );
        assert_eq!(commands[1].id, 5);
        assert_eq!(commands[1].exit_code, Some(1));
    }

    /// 真实 shell 不保证四个 marker 都发, 命令还在跑时也没有 D。
    #[test]
    fn commands_survive_missing_markers() {
        // 只发 A 和 D 的 shell。
        let sparse = [
            (
                shell_marker(1, ShellMarkerKind::PromptStart, 0, None),
                Some(-9),
            ),
            (
                shell_marker(2, ShellMarkerKind::CommandEnd, 0, Some(2)),
                Some(-6),
            ),
            (
                shell_marker(3, ShellMarkerKind::PromptStart, 0, None),
                Some(-6),
            ),
            (
                shell_marker(4, ShellMarkerKind::CommandEnd, 0, None),
                Some(-2),
            ),
        ];
        let commands = group_shell_markers(&sparse);
        assert_eq!(commands.len(), 2, "{commands:?}");
        assert_eq!(commands[0].exit_code, Some(2));
        assert!(commands[0].command.is_none());
        assert!(commands[0].output_start.is_none());
        // D 发了但没带状态码: 已结束, 状态未知。两者必须能区分。
        assert!(commands[1].command_end.is_some());
        assert_eq!(commands[1].exit_code, None);

        // 还在跑: 有 A/B/C, 没有 D。
        let running = [
            (
                shell_marker(1, ShellMarkerKind::PromptStart, 0, None),
                Some(-3),
            ),
            (
                shell_marker(2, ShellMarkerKind::CommandStart, 2, None),
                Some(-3),
            ),
            (
                shell_marker(3, ShellMarkerKind::OutputStart, 0, None),
                Some(-2),
            ),
        ];
        let commands = group_shell_markers(&running);
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(commands[0].command_end.is_none());
    }

    /// zsh 每重画一次提示符就发一个 A。那不是命令, 不该占一行输出。
    #[test]
    fn bare_prompt_starts_are_not_commands() {
        let markers = [
            (
                shell_marker(1, ShellMarkerKind::PromptStart, 0, None),
                Some(-3),
            ),
            (
                shell_marker(2, ShellMarkerKind::PromptStart, 0, None),
                Some(-2),
            ),
            (
                shell_marker(3, ShellMarkerKind::PromptStart, 0, None),
                Some(-1),
            ),
            (
                shell_marker(4, ShellMarkerKind::CommandStart, 2, None),
                Some(-1),
            ),
        ];
        let commands = group_shell_markers(&markers);
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(commands[0].id, 3);
    }

    /// 行号不可用不该影响退出码: 位置和状态是两件独立的事。
    #[test]
    fn an_unaddressable_command_keeps_its_exit_code() {
        let markers = [
            (shell_marker(1, ShellMarkerKind::PromptStart, 0, None), None),
            (shell_marker(2, ShellMarkerKind::OutputStart, 0, None), None),
            (
                shell_marker(3, ShellMarkerKind::CommandEnd, 0, Some(127)),
                None,
            ),
        ];
        let commands = group_shell_markers(&markers);
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(commands[0].exit_code, Some(127));
        assert!(
            commands[0]
                .output_start
                .as_ref()
                .is_some_and(|marker| marker.line.is_none()),
            "the marker is recorded but carries no row: {commands:?}"
        );
    }

    /// Handler fixture: a live extension host (spawn/split bind it to the
    /// session before fanning out) and an unattached connection id mirroring
    /// a pre-attach one-shot CLI socket. The `TempDir` keeps the extension
    /// directory alive for the duration of the test.
    fn handler_fixture(
        sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    ) -> (
        tempfile::TempDir,
        Arc<crate::extension_host::ServerExtensionHost>,
        Arc<parking_lot::Mutex<Option<String>>>,
    ) {
        let extensions_dir = tempfile::tempdir().expect("temp dir");
        let host = crate::extension_host::ServerExtensionHost::start(
            sessions.clone(),
            extensions_dir.path().join("extensions"),
        );
        let unattached: Arc<parking_lot::Mutex<Option<String>>> =
            Arc::new(parking_lot::Mutex::new(None));
        (extensions_dir, host, unattached)
    }


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
        let forward_tasks = Arc::new(parking_lot::Mutex::new(vec![crate::rt::spawn(
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

    #[tokio::test]
    async fn fetch_scrollback_rejects_malformed_requests_with_typed_error() {
        let pane = match crate::pane::Pane::spawn(
            "scrollback-validation-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(crate::pane::ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn scrollback validation pane: {error}"),
        };
        let mut session = crate::session::Session::new(
            "scrollback-validation-session".to_string(),
            "scrollback-validation-session".to_string(),
            "/tmp".to_string(),
        );
        session.panes.write().insert(pane.id.clone(), pane);
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));

        // Malformed direction must produce a typed RPC error, not an Err that
        // tears down the connection.
        let bad_direction = FetchScrollbackRequest {
            pane_id: "scrollback-validation-pane".to_string(),
            from_line: 0,
            direction: 2,
            count: 10,
        };
        let response = handle_fetch_scrollback(&bad_direction, &sessions)
            .await
            .expect("malformed request must not break the handler");
        assert!(matches!(
            &response,
            ResponseBody::Error(message) if message.contains("invalid scrollback direction")
        ));

        // Oversized count must fail before any response rows are built.
        let oversized = FetchScrollbackRequest {
            pane_id: "scrollback-validation-pane".to_string(),
            from_line: 0,
            direction: 1,
            count: u32::MAX,
        };
        let response = handle_fetch_scrollback(&oversized, &sessions)
            .await
            .expect("oversized request must not break the handler");
        assert!(matches!(
            &response,
            ResponseBody::Error(message) if message.contains("exceeds protocol grid limit")
        ));

        // A valid request still succeeds with a normal Scrollback response.
        let valid = FetchScrollbackRequest {
            pane_id: "scrollback-validation-pane".to_string(),
            from_line: 0,
            direction: 1,
            count: 10,
        };
        let response = handle_fetch_scrollback(&valid, &sessions)
            .await
            .expect("valid request must succeed");
        assert!(matches!(&response, ResponseBody::Scrollback(_)));
    }

    #[tokio::test]
    async fn spawn_pane_rejects_missing_session_before_spawning() {
        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let request = SpawnPaneRequest {
            session_id: "missing".to_string(),
            tab_id: "tab".to_string(),
            size: Some(mux_protocol::TerminalSize { cols: 80, rows: 24 }),
            cwd: None,
            command: Some(mux_protocol::ShellCommand {
                program: "/definitely/must/not/spawn".to_string(),
                args: Vec::new(),
                env: Default::default(),
            }),
        };

        let (_extensions_dir, extension_host, unattached) = handler_fixture(&sessions);
        let error = handle_spawn_pane(
            &request,
            &sessions,
            &settings,
            &clipboard,
            &extension_host,
            &unattached,
        )
        .await
        .expect_err("missing session must fail before child spawn");

        assert_eq!(error.to_string(), "session not found: missing");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn split_spawn_failure_preserves_layout_and_registry() {
        let pane = crate::pane::Pane::spawn(
            "parent-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(crate::pane::ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        )
        .expect("spawn split parent");
        let mut session = crate::session::Session::new(
            "split-session".to_string(),
            "split-session".to_string(),
            "/tmp".to_string(),
        );
        session.panes.write().insert(pane.id.clone(), pane);
        session.add_tab("tab-1".to_string(), "shell".to_string());
        session
            .tabs
            .get_mut("tab-1")
            .unwrap()
            .pane_ids
            .push("parent-pane".to_string());
        session.layout =
            crate::layout::LayoutTree::with_pane("node-1".to_string(), "parent-pane".to_string());
        session.focused_pane = Some("parent-pane".to_string());
        session.focused_tab = Some("tab-1".to_string());
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let (outbound, _notifications) = mpsc::unbounded_channel();
        let (_extensions_dir, extension_host, unattached) = handler_fixture(&sessions);

        let error = handle_split_pane(
            &SplitPaneRequest {
                pane_id: "parent-pane".to_string(),
                direction: mux_protocol::split_node::SplitDirection::LeftRight as i32,
                command: Some(mux_protocol::ShellCommand {
                    program: "/definitely/must/not/spawn".to_string(),
                    args: Vec::new(),
                    env: Default::default(),
                }),
                cwd: None,
            },
            &sessions,
            &outbound,
            &settings,
            &clipboard,
            &extension_host,
            &unattached,
        )
        .await
        .expect_err("split spawn must fail");
        assert!(!error.to_string().is_empty());

        let sessions = sessions.read();
        let session = &sessions[0];
        assert_eq!(session.layout.pane_ids(), vec!["parent-pane"]);
        assert_eq!(
            session.panes.read().keys().cloned().collect::<Vec<_>>(),
            vec!["parent-pane"]
        );
        assert_eq!(session.tabs["tab-1"].pane_ids, vec!["parent-pane"]);
        assert_eq!(session.focused_pane.as_deref(), Some("parent-pane"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fast_exit_spawn_is_added_then_removed_without_zombie_state() {
        let mut session = crate::session::Session::new(
            "fast-exit-session".to_string(),
            "fast-exit-session".to_string(),
            "/tmp".to_string(),
        );
        session.add_tab("tab-1".to_string(), "shell".to_string());
        session.focused_tab = Some("tab-1".to_string());
        let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();
        session.add_lifecycle_subscriber("test-client".to_string(), lifecycle_tx);
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let (_extensions_dir, extension_host, unattached) = handler_fixture(&sessions);

        let response = handle_spawn_pane(
            &SpawnPaneRequest {
                session_id: "fast-exit-session".to_string(),
                tab_id: "tab-1".to_string(),
                size: Some(mux_protocol::TerminalSize { cols: 20, rows: 5 }),
                cwd: None,
                command: Some(mux_protocol::ShellCommand {
                    // macOS ships `true` only under /usr/bin; hardcoding /bin
                    // makes this fail as ENOENT instead of testing the fast
                    // exit it is named for.
                    program: ["/bin/true", "/usr/bin/true"]
                        .into_iter()
                        .find(|candidate| std::path::Path::new(candidate).exists())
                        .unwrap_or("/usr/bin/true")
                        .to_string(),
                    args: Vec::new(),
                    env: Default::default(),
                }),
            },
            &sessions,
            &settings,
            &clipboard,
            &extension_host,
            &unattached,
        )
        .await
        .expect("spawn fast-exit pane");
        let pane_id = match response {
            ResponseBody::PaneId(id) => id,
            response => panic!("expected pane id, got {response:?}"),
        };

        let first = crate::rt::timeout(std::time::Duration::from_secs(2), lifecycle_rx.recv())
            .await
            .expect("PaneAdded timeout")
            .expect("PaneAdded channel closed");
        let first_event = match first.payload {
            Some(EnvelopePayload::Notification(notification)) => notification.event,
            payload => panic!("expected notification envelope, got {payload:?}"),
        };
        assert!(matches!(
            first_event,
            Some(mux_protocol::notification::Event::PaneAdded(ref added))
                if added.pane_id == pane_id
        ));
        let removed = crate::rt::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let envelope = lifecycle_rx.recv().await.expect("lifecycle channel closed");
                let event = match envelope.payload {
                    Some(EnvelopePayload::Notification(notification)) => notification.event,
                    payload => panic!("expected notification envelope, got {payload:?}"),
                };
                match event {
                    Some(mux_protocol::notification::Event::PaneRemoved(removed))
                        if removed.pane_id == pane_id =>
                    {
                        return removed;
                    }
                    Some(mux_protocol::notification::Event::PaneAdded(added))
                        if added.pane_id == pane_id =>
                    {
                        panic!("duplicate PaneAdded for {pane_id}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("PaneRemoved timeout");
        assert_eq!(removed.pane_id, pane_id);

        let sessions = sessions.read();
        let session = &sessions[0];
        assert!(session.panes.read().is_empty());
        assert!(session.layout.is_empty_root());
        assert!(session.tabs["tab-1"].pane_ids.is_empty());
        assert!(session.focused_pane.is_none());
        assert!(session.focused_tab.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_into_second_tab_keeps_layout_registry_and_focus_coherent() {
        let mut session = crate::session::Session::new(
            "tab-session".to_string(),
            "tab-session".to_string(),
            "/tmp".to_string(),
        );
        session.add_tab("tab-1".to_string(), "one".to_string());
        session.focused_tab = Some("tab-1".to_string());
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let (_extensions_dir, extension_host, unattached) = handler_fixture(&sessions);

        let mut pane_ids = Vec::new();
        for tab_id in ["tab-1", "tab-2"] {
            let response = handle_spawn_pane(
                &SpawnPaneRequest {
                    session_id: "tab-session".to_string(),
                    tab_id: tab_id.to_string(),
                    size: Some(mux_protocol::TerminalSize { cols: 20, rows: 5 }),
                    cwd: None,
                    command: Some(mux_protocol::ShellCommand {
                        program: "/bin/cat".to_string(),
                        args: Vec::new(),
                        env: Default::default(),
                    }),
                },
                &sessions,
                &settings,
                &clipboard,
                &extension_host,
                &unattached,
            )
            .await
            .expect("spawn tab pane");
            match response {
                ResponseBody::PaneId(id) => pane_ids.push(id),
                response => panic!("expected pane id, got {response:?}"),
            }
        }

        let sessions = sessions.read();
        let session = &sessions[0];
        let mut layout_ids = session.layout.pane_ids();
        layout_ids.sort();
        let mut registry_ids = session.panes.read().keys().cloned().collect::<Vec<_>>();
        registry_ids.sort();
        assert_eq!(layout_ids, registry_ids);
        assert_eq!(session.tabs["tab-1"].pane_ids, vec![pane_ids[0].clone()]);
        assert_eq!(session.tabs["tab-2"].pane_ids, vec![pane_ids[1].clone()]);
        assert_eq!(session.focused_tab.as_deref(), Some("tab-2"));
        assert_eq!(session.focused_pane.as_deref(), Some(pane_ids[1].as_str()));
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
        let reader = crate::rt::spawn({
            let guard = DropSignal(reader_dropped.clone());
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            }
        });
        let writer = crate::rt::spawn(async { Ok(()) });

        wait_for_connection_tasks(reader, writer, || async {}).await;

        assert!(reader_dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reader_exit_drains_queued_writer_response() {
        let response_written = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (response_sender, response_receiver) = crate::rt::oneshot::channel();
        let reader = crate::rt::spawn(async move {
            response_sender
                .send(())
                .map_err(|_| anyhow::anyhow!("writer dropped queued response"))?;
            Ok(())
        });
        let writer = crate::rt::spawn({
            let response_written = response_written.clone();
            async move {
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
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
        let reader = crate::rt::spawn(async { Ok(()) });
        let writer = crate::rt::spawn({
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
            resolve_path_within_root(
                &root,
                canonical_root.join("inside.txt").to_string_lossy().as_ref()
            )
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

    fn attached_session(
        id: &str,
        cwd: &std::path::Path,
        client_id: &str,
    ) -> crate::session::Session {
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

    /// §3.3 A steal-kicked connection (client id set but no session
    /// membership) must be rejected with `ResponseBody::Error` before any
    /// mutation or notification, while a pre-attach CLI socket (id `None`)
    /// keeps driving panes by target — the detached-CLI contract.
    #[tokio::test]
    async fn kicked_client_is_rejected_before_pane_mutation() {
        let directory = tempfile::tempdir().expect("temp directory");
        let pane = crate::pane::Pane::spawn(
            "kicked-pane".to_string(),
            directory.path().to_string_lossy().into_owned(),
            20,
            5,
            None,
        )
        .expect("spawn test pane");
        let mut session = crate::session::Session::new(
            "session-1".to_string(),
            "session-1".to_string(),
            "/tmp".to_string(),
        );
        session.panes.write().insert(pane.id.clone(), pane.clone());
        session.add_attached_client(
            "client-1".to_string(),
            crate::session::AttachMode::Shared,
            ClientRole::ReadWrite,
            None,
        );
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));

        // Simulate a steal kick: the connection keeps its client id but the
        // session no longer lists it.
        sessions.write()[0].remove_attached_client("client-1");
        let kicked: Arc<parking_lot::Mutex<Option<String>>> =
            Arc::new(parking_lot::Mutex::new(Some("client-1".to_string())));

        // SetPaneTitle must fail before touching the pane title.
        let response = handle_set_pane_title(
            &mux_protocol::SetPaneTitleRequest {
                pane_id: "kicked-pane".to_string(),
                title: "should-not-stick".to_string(),
            },
            &sessions,
            &kicked,
        )
        .await
        .expect("set_pane_title returns a response");
        assert!(
            matches!(response, ResponseBody::Error(_)),
            "got {response:?}"
        );
        assert_eq!(*pane.title.read(), String::new());

        // FocusPane must fail before changing the authoritative focus.
        let (outbound, _notifications) = mpsc::unbounded_channel();
        let response = handle_focus_pane(
            &FocusPaneRequest {
                pane_id: "kicked-pane".to_string(),
            },
            &sessions,
            &outbound,
            &kicked,
        )
        .await
        .expect("focus_pane returns a response");
        assert!(
            matches!(response, ResponseBody::Error(_)),
            "got {response:?}"
        );
        assert_eq!(sessions.read()[0].focused_pane, None);

        // ZoomPane must fail before flipping the zoom state.
        let response = handle_zoom_pane(
            &mux_protocol::ZoomPaneRequest {
                pane_id: "kicked-pane".to_string(),
                zoom: true,
            },
            &sessions,
            &outbound,
            &kicked,
        )
        .await
        .expect("zoom_pane returns a response");
        assert!(
            matches!(response, ResponseBody::Error(_)),
            "got {response:?}"
        );
        assert!(!pane.is_zoomed());

        // A pre-attach CLI socket (no client id) is still allowed to drive
        // the pane by target — the detached/pre-attach CLI contract.
        let pre_attach: Arc<parking_lot::Mutex<Option<String>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let response = handle_set_pane_title(
            &mux_protocol::SetPaneTitleRequest {
                pane_id: "kicked-pane".to_string(),
                title: "cli-title".to_string(),
            },
            &sessions,
            &pre_attach,
        )
        .await
        .expect("pre-attach set_pane_title returns a response");
        assert!(
            matches!(response, ResponseBody::Error(_)),
            "got {response:?}"
        );
        assert_eq!(*pane.title.read(), "cli-title");
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
            assert!(resolve_session_file_path(&sessions, &client_id, "escape/secret.txt").is_err());
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
        assert!(
            link_to_dir.is_dir,
            "a symlink to a directory is a directory"
        );

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

    #[test]
    fn repeated_spawn_layout_round_trips_without_exhausting_proto_recursion() {
        let mut layout =
            crate::layout::LayoutTree::with_pane("node-pane-0".to_string(), "pane-0".to_string());
        let mut focused = "pane-0".to_string();
        for index in 1..=100 {
            let next = format!("pane-{index}");
            layout
                .split(
                    &focused,
                    next.clone(),
                    crate::layout::SplitDirection::TopBottom,
                )
                .expect("split focused pane");
            focused = next;
        }

        let envelope = Envelope {
            version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
            payload: Some(EnvelopePayload::Notification(Notification {
                event: Some(mux_protocol::notification::Event::SessionLayoutChanged(
                    mux_protocol::SessionLayoutChanged {
                        layout: Some(layout_tree_to_proto(&layout)),
                        snapshot: None,
                    },
                )),
            })),
        };
        let framed = mux_protocol::frame(&envelope).expect("frame lifecycle notification");
        let (decoded, consumed) =
            mux_protocol::unframe(&framed).expect("decode lifecycle notification");

        assert_eq!(consumed, framed.len());
        match decoded.payload {
            Some(EnvelopePayload::Notification(Notification {
                event: Some(mux_protocol::notification::Event::SessionLayoutChanged(changed)),
            })) => {
                let root = changed
                    .layout
                    .and_then(|layout| layout.root)
                    .expect("decoded layout root");
                match root.node {
                    Some(mux_protocol::layout_node::Node::Split(split)) => {
                        assert_eq!(split.children.len(), 101);
                        assert_eq!(split.ratios.len(), 101);
                        assert!((split.ratios.iter().sum::<f32>() - 1.0).abs() < 1e-6);
                        assert!((split.ratios[0] - 0.5).abs() < 1e-6);
                        assert!(split.ratios.iter().all(|ratio| *ratio > 0.0));
                        assert!(split.ratios[1..].windows(2).all(|pair| pair[0] >= pair[1]));
                    }
                    node => panic!("expected flat decoded split, got {node:?}"),
                }
            }
            payload => panic!("expected layout notification, got {payload:?}"),
        }
    }

    #[tokio::test]
    async fn install_extension_reports_failure_instead_of_an_empty_error() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let host =
            crate::extension_host::ServerExtensionHost::start(sessions, temp.path().to_path_buf());
        let response = handle_install_extension(
            &mux_protocol::InstallExtensionRequest {
                name: "z3rm-demo".to_string(),
                manifest: Vec::new(),
                source: Vec::new(),
            },
            &host,
        )
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

    #[cfg(unix)]
    #[tokio::test]
    async fn confirmed_recovery_restores_layout_with_fresh_shells() {
        let connection = Connection::open_memory(Some("confirmed_recovery_fresh_shells"));
        crate::persistence::init_tables(&connection).expect("initialize persistence tables");
        let database = Arc::new(parking_lot::Mutex::new(connection));
        let original_pane = crate::pane::Pane::spawn(
            "recovered-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            80,
            24,
            Some(crate::pane::ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        )
        .expect("spawn original pane");
        let mut original = crate::session::Session::new(
            "recover-me".to_string(),
            "recover-me".to_string(),
            "/tmp".to_string(),
        );
        original
            .panes
            .write()
            .insert(original_pane.id.clone(), original_pane.clone());
        original.add_tab("tab-1".to_string(), "shell".to_string());
        original
            .tabs
            .get_mut("tab-1")
            .expect("tab")
            .pane_ids
            .push(original_pane.id.clone());
        original.layout =
            crate::layout::LayoutTree::with_pane("node-1".to_string(), original_pane.id.clone());
        original.focused_tab = Some("tab-1".to_string());
        original.set_focused_pane(original_pane.id.clone());
        let persisted = Arc::new(parking_lot::RwLock::new(vec![original]));
        crate::persistence::snapshot_sessions(&persisted, &database)
            .expect("persist recovery candidate");
        drop(persisted);
        drop(original_pane);

        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let (_extensions_dir, extension_host) = {
            let extensions_dir = tempfile::tempdir().expect("temp dir");
            let host = crate::extension_host::ServerExtensionHost::start(
                sessions.clone(),
                extensions_dir.path().join("extensions"),
            );
            (extensions_dir, host)
        };

        let listed = handle_list_recovery_candidates(&sessions, &database)
            .expect("list recovery candidates");
        match listed {
            ResponseBody::RecoveryCandidates(list) => {
                assert_eq!(list.candidates.len(), 1);
                assert!(list.candidates[0].metadata_complete);
            }
            response => panic!("expected recovery candidates, got {response:?}"),
        }

        let response = handle_confirm_recovery(
            &ConfirmRecoveryRequest {
                session_id: "recover-me".to_string(),
            },
            &sessions,
            &database,
            &settings,
            &clipboard,
            &extension_host,
        )
        .expect("confirm recovery");
        match response {
            ResponseBody::RecoveryConfirmed(recovered) => {
                assert_eq!(recovered.session_id, "recover-me");
                assert_eq!(recovered.pane_ids, vec!["recovered-pane"]);
            }
            response => panic!("expected recovery confirmation, got {response:?}"),
        }

        let listed = handle_list_recovery_candidates(&sessions, &database)
            .expect("list candidates after recovery");
        match listed {
            ResponseBody::RecoveryCandidates(list) => assert!(list.candidates.is_empty()),
            response => panic!("expected recovery candidates, got {response:?}"),
        }

        let sessions = sessions.read();
        assert_eq!(sessions.len(), 1);
        let recovered = &sessions[0];
        assert_eq!(recovered.layout.pane_ids(), vec!["recovered-pane"]);
        assert_eq!(recovered.focused_tab.as_deref(), Some("tab-1"));
        assert_eq!(recovered.focused_pane.as_deref(), Some("recovered-pane"));
        let pane = recovered
            .panes
            .read()
            .get("recovered-pane")
            .cloned()
            .expect("recovered pane");
        assert!(
            pane.command.is_none(),
            "recovery must not rerun persisted command: {:?}",
            pane.command
        );
    }

    /// §3.7/§15.4 恢复必须重建保存时的精确布局树 (节点 ID、比例、方向、焦点),
    /// 且恢复后的 wire 投影与保存时的投影逐字节一致。
    #[cfg(unix)]
    #[tokio::test]
    async fn confirmed_recovery_restores_exact_multi_level_layout_projection() {
        let connection = Connection::open_memory(Some("exact_multi_level_recovery"));
        crate::persistence::init_tables(&connection).expect("initialize persistence tables");
        let database = Arc::new(parking_lot::Mutex::new(connection));

        let mut spawned = Vec::new();
        for id in ["pane-1", "pane-2", "pane-3"] {
            spawned.push(
                crate::pane::Pane::spawn(
                    id.to_string(),
                    std::env::temp_dir().to_string_lossy().to_string(),
                    80,
                    24,
                    Some(crate::pane::ShellCommand {
                        program: "/bin/cat".to_string(),
                        ..Default::default()
                    }),
                )
                .expect("spawn original pane"),
            );
        }
        let mut layout = crate::layout::LayoutTree::with_pane(
            "node-1".to_string(),
            "pane-1".to_string(),
        );
        layout
            .split(
                "pane-1",
                "pane-2".to_string(),
                crate::layout::SplitDirection::LeftRight,
            )
            .expect("split left-right");
        layout
            .resize_pane("pane-1", crate::layout::SplitDirection::LeftRight, 0.2)
            .expect("resize outer split");
        layout
            .split(
                "pane-2",
                "pane-3".to_string(),
                crate::layout::SplitDirection::TopBottom,
            )
            .expect("split top-bottom");
        layout
            .resize_pane("pane-2", crate::layout::SplitDirection::TopBottom, 0.1)
            .expect("resize inner split");
        let saved_projection = layout_tree_to_proto(&layout).encode_to_vec();

        let mut original = crate::session::Session::new(
            "recover-exact".to_string(),
            "recover-exact".to_string(),
            "/tmp".to_string(),
        );
        for pane in &spawned {
            original.panes.write().insert(pane.id.clone(), pane.clone());
        }
        original.add_tab("tab-1".to_string(), "shell".to_string());
        original.tabs.get_mut("tab-1").expect("tab").pane_ids = vec![
            "pane-1".to_string(),
            "pane-2".to_string(),
            "pane-3".to_string(),
        ];
        original.layout = layout.clone();
        original.focused_tab = Some("tab-1".to_string());
        original.set_focused_pane("pane-3".to_string());
        let persisted = Arc::new(parking_lot::RwLock::new(vec![original]));
        crate::persistence::snapshot_sessions(&persisted, &database)
            .expect("persist exact layout candidate");
        drop(persisted);
        drop(spawned);

        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let (_extensions_dir, extension_host) = {
            let extensions_dir = tempfile::tempdir().expect("temp dir");
            let host = crate::extension_host::ServerExtensionHost::start(
                sessions.clone(),
                extensions_dir.path().join("extensions"),
            );
            (extensions_dir, host)
        };

        let response = handle_confirm_recovery(
            &ConfirmRecoveryRequest {
                session_id: "recover-exact".to_string(),
            },
            &sessions,
            &database,
            &settings,
            &clipboard,
            &extension_host,
        )
        .expect("confirm exact layout recovery");
        match response {
            ResponseBody::RecoveryConfirmed(recovered) => {
                assert_eq!(
                    recovered.pane_ids,
                    vec!["pane-1", "pane-2", "pane-3"]
                );
            }
            response => panic!("expected recovery confirmation, got {response:?}"),
        }

        let sessions = sessions.read();
        assert_eq!(sessions.len(), 1);
        let recovered = &sessions[0];
        assert_eq!(
            recovered.layout.root, layout.root,
            "restored tree must be the exact saved tree"
        );
        assert_eq!(recovered.focused_tab.as_deref(), Some("tab-1"));
        assert_eq!(recovered.focused_pane.as_deref(), Some("pane-3"));
        assert_eq!(
            recovered.layout.pane_ids(),
            vec!["pane-1", "pane-2", "pane-3"]
        );
        assert_eq!(
            layout_tree_to_proto(&recovered.layout).encode_to_vec(),
            saved_projection,
            "restored layout projection must match the saved projection"
        );
    }

    /// 损坏的持久化行在确认恢复时必须报错, 且不得把任何 session 发布到
    /// live registry。
    #[tokio::test]
    async fn corrupt_persisted_row_confirm_fails_without_publishing_session() {
        let connection = Connection::open_memory(Some("corrupt_confirm_no_publish"));
        crate::persistence::init_tables(&connection).expect("initialize persistence tables");
        let mut insert = sqlez::statement::Statement::prepare(
            &connection,
            "INSERT INTO sessions (id, name, cwd, layout_snapshot, last_snapshot_timestamp) VALUES (?, ?, ?, ?, ?)",
        )
        .expect("prepare corrupt row insert");
        insert.bind(&"corrupt", 1).expect("bind id");
        insert.bind(&"corrupt", 2).expect("bind name");
        insert.bind(&"/tmp", 3).expect("bind cwd");
        insert
            .bind(&r#"{"version":2,"layout":{"nodes":[{"type":"split","id":"root","direction":"LeftRight","children":[1,2],"ratios":[0.0,1.0]},{"type":"pane","id":"n1","pane_id":"pane-1"},{"type":"pane","id":"n2","pane_id":"pane-2"}]},"tabs":[{"id":"tab-1","title":"shell","pane_ids":["pane-1","pane-2"]}],"panes":[{"id":"pane-1","cwd":"/tmp","title":"cat","cols":80,"rows":24},{"id":"pane-2","cwd":"/tmp","title":"cat","cols":80,"rows":24}],"focused_tab":"tab-1","focused_pane":"pane-1"}"#, 4)
            .expect("bind corrupt layout");
        insert.bind(&0_i64, 5).expect("bind timestamp");
        insert.exec().expect("insert corrupt row");
        drop(insert);
        let database = Arc::new(parking_lot::Mutex::new(connection));
        let settings = crate::server_settings::ServerSettings::load();
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let (_extensions_dir, extension_host, _unattached) = handler_fixture(&sessions);

        let error = handle_confirm_recovery(
            &ConfirmRecoveryRequest {
                session_id: "corrupt".to_string(),
            },
            &sessions,
            &database,
            &settings,
            &clipboard,
            &extension_host,
        )
        .expect_err("corrupt persisted layout must fail confirmation");
        assert!(!error.to_string().is_empty());
        assert!(
            sessions.read().is_empty(),
            "a rejected candidate must never publish a session"
        );
    }
}
