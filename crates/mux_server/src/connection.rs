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
pub fn proto_role_to_client_role(role: i32) -> ClientRole {
    match role {
        1 => ClientRole::ReadOnly,
        2 => ClientRole::ReadWrite,
        3 => ClientRole::Admin,
        _ => ClientRole::ReadWrite, // 未指定时默认为 ReadWrite
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
        unregister_client_from_sessions(&mut sessions, &client_id);
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

fn unregister_client_from_sessions(sessions: &mut [crate::session::Session], client_id: &str) {
    for session in sessions {
        session.remove_attached_client(client_id);
        session.remove_lifecycle_subscriber(client_id);
        for pane in session.panes.read().values() {
            pane.remove_subscriber(client_id);
        }
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
    db: &Arc<parking_lot::Mutex<Connection>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    shutdown_state: &Arc<crate::ShutdownState>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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
                &forward_tasks,
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
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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

    // §3.3 客户端角色:未 attach 时默认 Admin。
    //
    // **fail-open 假设** (此处显式记录,新增 transport 时必须重新评估):
    //   - 本地 Unix socket 走 §9 的 0600 ACL,已保证 user-level 隔离
    //   - 当前唯一的 transport 就是 Local;SSH 走 SSH-forwarded Unix socket,
    //     同样落到本机 socket 权限模型
    //   - 因此本地连接 = 同 UID 信任 = Admin
    //
    // **风险**:如果未来加入网络 transport (mTLS、UDP resilient §25),
    // 0600 ACL 不再适用,此默认会变成提权漏洞。届时必须改为:
    //   - 默认 ReadOnly (fail-closed)
    //   - 显式 identity 才能提权到 Admin
    //   - 或按 transport 类型分支默认角色
    let role = client_role.lock().unwrap_or(ClientRole::Admin);

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
                match handle_confirm_recovery(request, sessions, db, server_settings, clipboard) {
                    Ok(response) => response,
                    Err(error) => ResponseBody::Error(error.to_string()),
                }
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }

        RequestBody::KillSession(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_kill_session(r, sessions, db).await?
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
        RequestBody::InstallExtension(_) => {
            // §16.12 Extension install 是 client-side 操作,server 端没有 extension host。
            // 返回空 response 而非 error;真正的安装逻辑在 crates/z3rm/src/cli/marketplace.rs。
            ResponseBody::Error(String::new())
        }
        RequestBody::NewWindow(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_new_window(r, sessions, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }

        // §3.3 需要 ReadWrite 的 pane 操作 (Plan 33)
        RequestBody::SpawnPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_spawn_pane(r, sessions, server_settings, clipboard).await?
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
                handle_resize_pane(r, sessions).await?
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
                handle_send_input(r, sessions, connection_client_id).await?
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
        // 通过这些 RPC 读文件。权限:任意角色可读 (§15.1 client 是同 UID 信任)。
        RequestBody::ReadFile(r) => handle_read_file(r).await?,
        RequestBody::ListDir(r) => handle_list_dir(r).await?,
        RequestBody::StatFile(r) => handle_stat_file(r).await?,

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
    tokio::spawn(async move {
        let session_id_for_start = session_id.clone();
        let cwd_for_start = cwd.clone();
        let result = tokio::task::spawn_blocking(move || {
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
                    metadata_complete: candidate.metadata_complete,
                    pane_ids: candidate.layout.pane_ids(),
                })
                .collect(),
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
        candidate.metadata_complete,
        "recovery candidate {} lacks complete pane metadata",
        candidate.id
    );
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
    db: &Arc<parking_lot::Mutex<Connection>>,
) -> anyhow::Result<ResponseBody> {
    let session = {
        // Match persist_loop's db -> sessions lock order. Holding the DB lock
        // across both mutations prevents a periodic snapshot from reinserting
        // the session between durable deletion and in-memory removal.
        let conn = db.lock();
        let mut sessions = sessions.write();
        let Some(index) = sessions.iter().position(|session| session.id == req.id) else {
            return Ok(ResponseBody::Error(format!(
                "session not found: {}",
                req.id
            )));
        };
        crate::persistence::delete_session(&conn, &req.id)?;
        sessions.remove(index)
    };
    if let Some(watch) = session.snapshot_watch.as_ref() {
        watch.stop();
    }
    zlog::info!("session killed: id={}", req.id);
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 连接会话 — 把客户端的 outbound_tx 注册为所有 pane 的 subscriber
async fn handle_attach(
    req: &AttachRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    connection_client_id: &Arc<parking_lot::Mutex<Option<String>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    forward_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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
    unregister_client_from_sessions(&mut sessions_w, &client_id);
    let session = &mut sessions_w[target_session];

    // §3.3 角色解析: identity 显式声明时以其为准;否则保留既有角色
    // (本地 socket 默认 Admin,见 dispatch_request)。ReadOnly attach mode
    // 是会话级写保护, 必须降权整个连接, 否则 attach -r 后续 SendInput
    // 仍会按 Admin/ReadWrite 通过。
    let requested_role = if let Some(identity) = &req.identity {
        proto_role_to_client_role(identity.role)
    } else {
        client_role.lock().unwrap_or(ClientRole::Admin)
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
        session.attached_clients.write().clear();
        kicked_clients.extend(session.clear_lifecycle_subscribers());
        for pane in session.panes.read().values() {
            for kicked_client in &kicked_clients {
                pane.remove_subscriber(kicked_client);
            }
        }
    }
    session.add_attached_client(client_id.clone(), mode, role);

    if !req.window_id.is_empty() {
        session.add_window(req.window_id.clone());
    }

    // §3.4 Register this connection's outbound channel as a session-level
    // lifecycle subscriber. lifecycle_subscribers is keyed by client_id and
    // held by the session; the connection's outbound_tx is closed when its
    // read/write loop exits, after which broadcast_lifecycle prunes it.
    // Re-attach of the same client_id replaces the prior sender idempotently.
    session.add_lifecycle_subscriber(client_id.clone(), outbound_tx.clone());

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

    // §15.4 权威快照:tabs / layout / focused 必须反映 server 真实状态。
    // 旧实现写死 tabs: Vec::new() 是严重违反 spec §15.4 的 bug。
    let tabs_proto: Vec<mux_protocol::TabInfo> = session
        .tabs
        .values()
        .map(|t| mux_protocol::TabInfo {
            id: t.id.clone(),
            title: t.title.clone(),
            panes: t
                .pane_ids
                .iter()
                .filter_map(|pid| pane_info_for(session, pid))
                .collect(),
        })
        .collect();

    Ok(ResponseBody::Attach(AttachResponse {
        snapshot: Some(SessionSnapshot {
            session_id: session.id.clone(),
            focused_pane_id: session.focused_pane.clone().unwrap_or_default(),
            focused_tab_id: session.focused_tab.clone().unwrap_or_default(),
            tabs: tabs_proto,
            layout: Some(layout_tree_to_proto(&session.layout)),
        }),
    }))
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
        unregister_client_from_sessions(&mut sessions_w, &client_id);
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

async fn handle_new_window(
    req: &NewWindowRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    // §3.3 生成新窗口 ID
    let window_id = format!("win-{}-{}", std::process::id(), nanoid::nanoid!());

    let mut sessions_w = sessions.write();
    let session = sessions_w
        .iter_mut()
        .find(|s| s.id == req.session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.session_id))?;
    session.add_window(window_id.clone());
    drop(sessions_w);

    // §16.12 记录新窗口创建事件
    zlog::info!(
        "new window created: session={} window={}",
        req.session_id,
        window_id
    );

    // §3.3 广播 WindowAdded 通知到所有已连接窗口
    let notify = Notification {
        event: Some(mux_protocol::proto::notification::Event::WindowAdded(
            mux_protocol::WindowAdded {
                window_id: window_id.clone(),
                session_id: req.session_id.clone(),
            },
        )),
    };
    let _ = send_notification_envelope(outbound_tx, notify);

    // §3.3 返回新窗口信息 (无 snapshot — 客户端应另行 attach)
    Ok(ResponseBody::NewWindow(NewWindowResponse {
        window_id,
        snapshot: None,
    }))
}

fn session_cwd(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
) -> Option<String> {
    sessions
        .read()
        .iter()
        .find(|session| session.id == session_id)
        .map(|session| session.cwd.clone())
}

/// §3.10 创建 pane — 真正 spawn PTY + alacritty Term (server-canonical)
async fn handle_spawn_pane(
    req: &SpawnPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    server_settings: &Arc<crate::server_settings::ServerSettings>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
) -> anyhow::Result<ResponseBody> {
    let session_cwd = session_cwd(sessions, &req.session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.session_id))?;
    let pane_id = nanoid::nanoid!();

    // §3.1 转换 ShellCommand → pane::ShellCommand
    let shell_cmd = req.command.as_ref().map(|c| crate::pane::ShellCommand {
        program: c.program.clone(),
        args: c.args.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    });

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
            // succeeded; a spawn error must leave the original tree intact.
            if let Err(error) =
                session
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
async fn handle_resize_pane(
    req: &ResizePaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            pane.resize(req.cols, req.rows)?;
            return Ok(ResponseBody::Error(String::new()));
        }
    }
    Ok(ResponseBody::Error("pane not found".to_string()))
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

/// §3.10 Forward raw client input to the PTY unchanged.
async fn handle_send_input(
    req: &SendInputRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
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

    // Terminal output sequences are interpreted only by the emulator output path.
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

    fn normalized_weights(ratios: &[f32], child_count: usize) -> Vec<f32> {
        if ratios.len() != child_count || ratios.is_empty() {
            return vec![1.0 / child_count.max(1) as f32; child_count];
        }
        let sum: f32 = ratios.iter().sum();
        if !sum.is_finite() || sum <= 0.0 || ratios.iter().any(|r| !r.is_finite() || *r < 0.0) {
            return vec![1.0 / child_count.max(1) as f32; child_count];
        }
        ratios.iter().map(|r| r / sum).collect()
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

fn broadcast_layout_changed(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    session_id: &str,
) {
    let layout_proto = {
        let sessions_r = sessions.read();
        sessions_r
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| layout_tree_to_proto(&s.layout))
    };
    let Some(layout) = layout_proto else {
        return;
    };
    let notify = Notification {
        event: Some(mux_protocol::notification::Event::SessionLayoutChanged(
            mux_protocol::SessionLayoutChanged {
                layout: Some(layout),
            },
        )),
    };
    broadcast_lifecycle_in_session(sessions, session_id, notify);
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

fn resolve_shadow_path(
    root: &std::path::Path,
    requested: &str,
) -> anyhow::Result<std::path::PathBuf> {
    use std::path::Component;

    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing shadow root: {}", root.display()))?;
    let requested_path = std::path::Path::new(requested);
    anyhow::ensure!(
        !requested_path.as_os_str().is_empty(),
        "shadow path is empty"
    );
    anyhow::ensure!(
        !requested_path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "shadow path may not contain parent traversal"
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
            .context("shadow path has no existing ancestor")?;
    }
    let canonical_ancestor = existing_ancestor.canonicalize().with_context(|| {
        format!(
            "canonicalizing shadow path ancestor: {}",
            existing_ancestor.display()
        )
    })?;
    anyhow::ensure!(
        canonical_ancestor.starts_with(&canonical_root),
        "shadow path escapes session cwd"
    );

    let suffix = candidate
        .strip_prefix(existing_ancestor)
        .context("resolving shadow path suffix")?;
    Ok(canonical_ancestor.join(suffix))
}

async fn handle_list_file_versions(
    request: &mux_protocol::ListFileVersionsRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let (watch, root) = snapshot_context_for_session(sessions, &request.session_id)?;
    let path = resolve_shadow_path(&root, &request.path)?;
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
    let path = resolve_shadow_path(&root, &request.path)?;
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
    let path = resolve_shadow_path(&root, &request.path)?;
    let version_id = request.version_id;
    tokio::task::spawn_blocking(move || watch.decline(path, version_id))
        .await
        .context("joining shadow decline request")??;
    Ok(ResponseBody::DeclineFileVersion(
        mux_protocol::DeclineFileVersionResponse { restored: true },
    ))
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

/// §16.6 ReadFile: 读取本地文件,自动检测 binary。
/// §4.7 shadow_snapshot 集成后,路径会经过 worktree 解析。当前直接读 fs。
async fn handle_read_file(req: &mux_protocol::ReadFileRequest) -> anyhow::Result<ResponseBody> {
    let path = std::path::Path::new(&req.path);
    match std::fs::read(path) {
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

/// §16.6 ListDir: 列出目录条目。
async fn handle_list_dir(req: &mux_protocol::ListDirRequest) -> anyhow::Result<ResponseBody> {
    let path = std::path::Path::new(&req.path);
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut out = Vec::new();
            for entry in entries.flatten() {
                let meta = entry.metadata().ok();
                let name = entry.file_name().to_string_lossy().into_owned();
                out.push(mux_protocol::DirEntry {
                    name,
                    is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                    size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    is_modified: false, // §4.7 shadow_snapshot 集成后填充
                });
            }
            // 目录列表排序:目录优先,然后按名称 (确定性输出便于测试)。
            out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
            Ok(ResponseBody::DirListing(mux_protocol::ListDirResponse {
                entries: out,
            }))
        }
        Err(e) => Ok(ResponseBody::Error(format!("list_dir: {}", e))),
    }
}

/// §16.6 StatFile: 返回文件元数据。
async fn handle_stat_file(req: &mux_protocol::StatFileRequest) -> anyhow::Result<ResponseBody> {
    let path = std::path::Path::new(&req.path);
    match std::fs::metadata(path) {
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
            );
            let (sender, _receiver) = mpsc::unbounded_channel();
            session.add_lifecycle_subscriber("client-1".to_string(), sender);
        }

        unregister_client_from_sessions(&mut sessions, "client-1");

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

    #[tokio::test]
    async fn kill_missing_session_returns_nonempty_error() {
        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let connection =
            Connection::open_memory(Some("kill_missing_session_returns_nonempty_error"));
        crate::persistence::init_tables(&connection).expect("initialize persistence tables");
        let database = Arc::new(parking_lot::Mutex::new(connection));

        let response = handle_kill_session(
            &KillSessionRequest {
                id: "missing".to_string(),
            },
            &sessions,
            &database,
        )
        .await
        .expect("handle missing session kill");

        match response {
            ResponseBody::Error(error) => assert_eq!(error, "session not found: missing"),
            response => panic!("expected error response, got {response:?}"),
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
        );
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
        assert!(matches!(
            pane_notifications.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(forward_tasks.lock().is_empty());
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

        let error = handle_spawn_pane(&request, &sessions, &settings, &clipboard)
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

        let response = handle_spawn_pane(
            &SpawnPaneRequest {
                session_id: "fast-exit-session".to_string(),
                tab_id: "tab-1".to_string(),
                size: Some(mux_protocol::TerminalSize { cols: 20, rows: 5 }),
                cwd: None,
                command: Some(mux_protocol::ShellCommand {
                    program: "/bin/true".to_string(),
                    args: Vec::new(),
                    env: Default::default(),
                }),
            },
            &sessions,
            &settings,
            &clipboard,
        )
        .await
        .expect("spawn fast-exit pane");
        let pane_id = match response {
            ResponseBody::PaneId(id) => id,
            response => panic!("expected pane id, got {response:?}"),
        };

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), lifecycle_rx.recv())
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
        let removed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
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

        assert!(resolve_shadow_path(&root, "../outside/file.txt").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).expect("create symlink");
            assert!(resolve_shadow_path(&root, "escape/file.txt").is_err());
        }
    }

    #[test]
    fn shadow_path_allows_missing_file_below_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().join("root");
        std::fs::create_dir_all(root.join("nested")).expect("create root");

        let resolved = resolve_shadow_path(&root, "nested/deleted.txt").expect("resolve path");

        assert_eq!(resolved, root.join("nested/deleted.txt"));
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
}
