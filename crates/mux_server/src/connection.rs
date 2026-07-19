// §9 Connection 模块 — mux_protocol 消息分发、帧编码/解码、通知广播。
// 每个客户端连接一个 tokio task, 处理请求并推送通知。

use mux_protocol::proto::request::Body as RequestBody;
use prost::Message;
use mux_protocol::proto::fetch_grid_update_response::Update as FetchGridUpdateResponseUpdate;
use mux_protocol::proto::response::Body as ResponseBody;
use mux_protocol::proto::envelope::Payload as EnvelopePayload;
use mux_protocol::proto::*;
use sqlez::connection::Connection;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
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
        (ClientRole::ReadWrite, ClientRole::ReadWrite) |
        (ClientRole::ReadWrite, ClientRole::ReadOnly) => true,
        (ClientRole::ReadOnly, ClientRole::ReadOnly) => true,
        _ => false,
    }
}

/// 处理单个客户端连接 (§9)
///
/// 单一 outbound mpsc channel 同时承载 Response 和 Notification:
/// 写循环 (write_handle) 消费 channel, 把 Envelope framed 写回 socket。
/// 这样所有写操作都在同一个 tokio task 内串行化, 避免并发 write 冲突。
pub async fn handle_connection(
    stream: UnixStream,
    sessions: Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: Arc<parking_lot::Mutex<Connection>>,
    clipboard: Arc<crate::clipboard::ServerClipboard>,
) -> anyhow::Result<()> {
    let (reader, writer) = tokio::io::split(stream);

    // §9 outbound channel: Response 或 Notification 都走这个
    let (outbound_tx, mut outbound_rx) =
        mpsc::unbounded_channel::<Envelope>();

    // §3.3 客户端角色: 初始为 None, attach 后设置 (Plan 33)
    let client_role: Arc<parking_lot::Mutex<Option<ClientRole>>> =
        Arc::new(parking_lot::Mutex::new(None));

    let read_handle = {
        let outbound_tx = outbound_tx.clone();
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let envelope = read_envelope(&mut reader).await?;
                dispatch_envelope(&envelope, &sessions, &outbound_tx, &db, &clipboard, &client_role).await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        })
    };

    // §9 写循环: 消费 outbound channel, framed 写回客户端
    let write_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(envelope) = outbound_rx.recv().await {
            if let Ok(framed) = mux_protocol::frame(&envelope) {
                if writer.write_all(&framed).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    let _ = tokio::join!(read_handle, write_handle);
    Ok(())
}

/// §9 从 socket 读取长度前缀帧, 解码 Envelope
async fn read_envelope(
    reader: &mut tokio::io::ReadHalf<UnixStream>,
) -> anyhow::Result<Envelope> {
    // 读取 varint 长度前缀 (§9)
    let mut len: u64 = 0;
    let mut shift: u32 = 0;

    loop {
        let byte = reader.read_u8().await?;
        len |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }

    // 读取 payload 数据
    let mut data = vec![0u8; len as usize];
    reader.read_exact(&mut data).await?;

    // 解码 Envelope (varint 前缀已读取, 用 decode 而非 decode_length_delimited)
    let envelope = Envelope::decode(&data[..])?;
    Ok(envelope)
}

/// §9 分发 Envelope 到请求/通知处理器
async fn dispatch_envelope(
    envelope: &Envelope,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
    _db: &Arc<parking_lot::Mutex<Connection>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
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
            dispatch_request(req, sessions, outbound_tx, clipboard, client_role).await?;
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
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
) -> anyhow::Result<()> {
    let request_id = req.request_id;

    let body = match &req.body {
        Some(b) => b,
        None => {
            send_response(outbound_tx, Response {
                request_id,
                body: Some(ResponseBody::Error("empty request body".to_string())),
            })?;
            return Ok(());
        }
    };

    // §3.3 客户端角色:未 attach 时默认 Admin。
    // 本地 socket 的 0600 权限已实现 user-level 隔离 (§9),
    // 未显式声明 role 的本地连接按 Admin 处理;Attach 可降级。
    let role = client_role.lock().unwrap_or(ClientRole::Admin);

    let resp_body = match body {
        // §3.3 无权限要求的操作
        RequestBody::CreateSession(r) => handle_create_session(r, sessions).await?,
        RequestBody::ListSessions(_) => handle_list_sessions(sessions).await?,
        RequestBody::Attach(r) => handle_attach(r, sessions, client_role, outbound_tx).await?,
        RequestBody::Detach(_) => handle_detach(sessions).await?,
        RequestBody::FetchGridUpdate(r) => handle_fetch_grid_update(r, sessions).await?,
        RequestBody::FetchScrollback(r) => handle_fetch_scrollback(r, sessions).await?,
        RequestBody::SearchScrollback(r) => handle_search_scrollback(r, sessions).await?,
        RequestBody::GetClipboard(_) => handle_get_clipboard(clipboard).await?,

        // §3.3 Admin-only 操作 (Plan 33)
        RequestBody::KillSession(r) => {
            if check_permission(role, ClientRole::Admin) {
                handle_kill_session(r, sessions).await?
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::RenameSession(_r) => {
            if check_permission(role, ClientRole::Admin) {
                ResponseBody::Error("rename_session not implemented yet".to_string())
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
        }
        RequestBody::InstallExtension(_) => {
            if check_permission(role, ClientRole::Admin) {
                ResponseBody::Error("install_extension not implemented yet".to_string())
            } else {
                ResponseBody::Error("permission denied: admin required".to_string())
            }
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
                handle_spawn_pane(r, sessions, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SplitPane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_split_pane(r, sessions, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::ClosePane(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_close_pane(r, sessions, outbound_tx).await?
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

        // §3.3 需要 ReadWrite 的输入操作 (Plan 33)
        RequestBody::SendInput(r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                handle_send_input(r, sessions, clipboard, outbound_tx).await?
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
                handle_set_clipboard(r, clipboard, outbound_tx).await?
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }
        RequestBody::SetPaneTitle(_r) => {
            if check_permission(role, ClientRole::ReadWrite) {
                ResponseBody::Error("set_pane_title not implemented yet".to_string())
            } else {
                ResponseBody::Error("permission denied: read-write required".to_string())
            }
        }

        // §3.3 文件操作暂不限制 (Plan 33)
        RequestBody::ReadFile(_) => ResponseBody::Error("read_file not implemented yet".to_string()),
        RequestBody::ListDir(_) => ResponseBody::Error("list_dir not implemented yet".to_string()),
        RequestBody::StatFile(_) => ResponseBody::Error("stat_file not implemented yet".to_string()),
    };

    // §9 把 Response 通过 outbound channel 写回客户端
    send_response(outbound_tx, Response {
        request_id,
        body: Some(resp_body),
    })?;

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
    outbound_tx.send(envelope).map_err(|_| anyhow::anyhow!("client disconnected"))
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

    // §16.12 记录 session 创建事件
    zlog::info!("session created: id={} name={} cwd={}", id, req.name, req.cwd);

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

    Ok(ResponseBody::Sessions(ListSessionsResponse { sessions: infos }))
}

/// §3.10 结束会话
async fn handle_kill_session(
    req: &KillSessionRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    let idx = sessions_w.iter().position(|s| s.id == req.id);
    if let Some(idx) = idx {
        sessions_w.remove(idx);
        // §16.12 记录 session 销毁事件
        zlog::info!("session killed: id={}", req.id);
    } else {
        zlog::warn!("kill session not found: id={}", req.id);
    }
    Ok(ResponseBody::Error(String::new()))
}
/// §3.10 连接会话 — 把客户端的 outbound_tx 注册为所有 pane 的 subscriber
async fn handle_attach(
    req: &AttachRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    client_role: &Arc<parking_lot::Mutex<Option<ClientRole>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    let session = sessions_w
        .iter_mut()
        .find(|s| s.id == req.session_id)
        .ok_or_else(|| anyhow::anyhow!("session not found: {}", req.session_id))?;

    zlog::info!("client attached: session={} mode={:?}", req.session_id, req.mode);

    // §3.3 解析客户端身份 (Plan 33)
    let client_id = if let Some(identity) = &req.identity {
        if !identity.client_id.is_empty() {
            identity.client_id.clone()
        } else {
            format!("client-{}", std::process::id())
        }
    } else {
        format!("client-{}", std::process::id())
    };

    // §3.3 角色解析:identity 显式声明时以其为准;否则保留既有角色
    // (本地 socket 默认 Admin,见 dispatch_request)。这避免本地连接
    // 在 attach 后被静默降权,无法执行 create/kill session。
    let role = if let Some(identity) = &req.identity {
        proto_role_to_client_role(identity.role)
    } else {
        client_role.lock().unwrap_or(ClientRole::Admin)
    };
    *client_role.lock() = Some(role);

    let mode = match req.mode {
        0 => crate::session::AttachMode::Shared,
        1 => crate::session::AttachMode::Shared,
        2 => crate::session::AttachMode::Steal,
        3 => crate::session::AttachMode::ReadOnly,
        _ => crate::session::AttachMode::Shared,
    };
    session.add_attached_client(client_id, mode, role);

    if !req.window_id.is_empty() {
        session.add_window(req.window_id.clone());
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
        pane.add_subscriber(inner_tx);
        // forward task: 把 inner Notification 包成 Envelope 发到 outbound
        let forward_tx = notification_forward_tx.clone();
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

/// §3.10 断开连接
async fn handle_detach(
    _sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    Ok(ResponseBody::Error(String::new()))
}

/// §3.3 在现有会话中创建新窗口 (Plan 32)
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
        event: Some(
            mux_protocol::proto::notification::Event::WindowAdded(
                mux_protocol::WindowAdded {
                    window_id: window_id.clone(),
                    session_id: req.session_id.clone(),
                },
            ),
        ),
    };
    let _ = send_notification_envelope(outbound_tx, notify);

    // §3.3 返回新窗口信息 (无 snapshot — 客户端应另行 attach)
    Ok(ResponseBody::NewWindow(NewWindowResponse {
        window_id,
        snapshot: None,
    }))
}

/// §3.10 创建 pane — 真正 spawn PTY + alacritty Term (server-canonical)
async fn handle_spawn_pane(
    req: &SpawnPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
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

    // §3.1 spawn PTY + alacritty Term
    let pane = crate::pane::Pane::spawn(
        pane_id.clone(),
        cwd,
        cols,
        rows,
        shell_cmd,
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
            let tab = session.tabs.entry(req.tab_id.clone()).or_insert_with(|| {
                crate::session::Tab {
                    id: req.tab_id.clone(),
                    title: String::new(),
                    pane_ids: Vec::new(),
                }
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

    // §3.4 把该连接的 outbound_tx 注册为新 pane 的 subscriber
    // (其他已 attach 的连接也需要注册 — TODO: session-level subscriber list)
    {
        let sessions_r = sessions.read();
        if let Some(session) = sessions_r.iter().find(|s| s.id == req.session_id) {
            let panes = session.panes.clone();
            if let Some(pane) = panes.read().get(&pane_id) {
                let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<Notification>();
                pane.add_subscriber(inner_tx);
                let forward_tx = outbound_tx.clone();
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
    }

    zlog::info!("pane spawned: id={} session={}", pane_id, req.session_id);

    // §3.4 推送 PaneAdded 通知
    let notify = Notification {
        event: Some(mux_protocol::notification::Event::PaneAdded(
            mux_protocol::PaneAdded {
                pane_id: pane_id.clone(),
                tab_id: req.tab_id.clone(),
            },
        )),
    };
    let _ = send_notification_envelope(outbound_tx, notify);

    Ok(ResponseBody::PaneId(pane_id))
}
/// §3.10 分割 pane — split layout + spawn new pane with parent's cwd
async fn handle_split_pane(
    req: &SplitPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
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

            // §3.10 spawn 新 pane, 继承 parent pane 的 cwd
            let parent_cwd = session.panes.read().get(&req.pane_id).map(|p| p.cwd.clone()).unwrap_or_default();
            let parent_cols = session.panes.read().get(&req.pane_id).map(|p| p.get_cols()).unwrap_or(80);
            let parent_rows = session.panes.read().get(&req.pane_id).map(|p| p.get_rows()).unwrap_or(24);

            let pane = crate::pane::Pane::spawn(
                new_pane_id.clone(),
                parent_cwd,
                parent_cols,
                parent_rows,
                None,
            )?;
            session.panes.write().insert(new_pane_id.clone(), pane);
            session.set_focused_pane(new_pane_id.clone());

            // §3.4 给新 pane 注册当前连接的 subscriber
            let pane_ref = session.panes.read().get(&new_pane_id).cloned();
            if let Some(pane) = pane_ref {
                let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<Notification>();
                pane.add_subscriber(inner_tx);
                let forward_tx = outbound_tx.clone();
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

            // §3.4 PaneAdded 通知
            let notify = Notification {
                event: Some(mux_protocol::notification::Event::PaneAdded(
                    mux_protocol::PaneAdded {
                        pane_id: new_pane_id.clone(),
                        tab_id: String::new(),
                    },
                )),
            };
            let _ = send_notification_envelope(outbound_tx, notify);
        }
    }

    Ok(ResponseBody::PaneId(new_pane_id))
}

/// §3.10 关闭 pane
async fn handle_close_pane(
    req: &ClosePaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    for session in sessions_w.iter_mut() {
        if let Err(e) = session.layout.remove_pane(&req.pane_id) {
            tracing::error!(error = ?e, pane_id = %req.pane_id, "failed to remove pane from layout");
        }
        // §3.10 从 panes registry 移除 (drop 触发 child kill)
        let mut panes = session.panes.write();
        if panes.remove(&req.pane_id).is_some() {
            zlog::info!("pane closed: id={}", req.pane_id);
        }
    }

    // §3.4 推送 PaneRemoved 通知
    let notify = Notification {
        event: Some(mux_protocol::notification::Event::PaneRemoved(
            mux_protocol::PaneRemoved {
                pane_id: req.pane_id.clone(),
                exit_code: 0,
            },
        )),
    };
    let _ = send_notification_envelope(outbound_tx, notify);

    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 聚焦 pane
async fn handle_focus_pane(
    req: &FocusPaneRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    _outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    let mut sessions_w = sessions.write();
    for session in sessions_w.iter_mut() {
        if session.layout.root.find_pane(&req.pane_id).is_some() {
            session.set_focused_pane(req.pane_id.clone());
        }
    }
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
            pane.resize(req.cols, req.rows);
        }
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 发送输入 + §16.6 OSC 52 剪贴板拦截
async fn handle_send_input(
    req: &SendInputRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    // §16.6 解析 OSC 52 序列: ESC ] 52 ; c ; <base64> BEL/ST
    let mut osc52_parser = crate::clipboard::Osc52Parser::new();
    if let Some(base64_content) = osc52_parser.feed(&req.data) {
        // §16.6 OSC 52 触发剪贴板更新并通知所有客户端
        let origin_host = std::env::var("HOSTNAME")
            .unwrap_or_else(|_| "z3rm-server".to_string());
        clipboard.set_from_osc52(&base64_content, origin_host, outbound_tx)?;
        // OSC 52 序列已被消费, 不转发到 PTY
        return Ok(ResponseBody::Error(String::new()));
    }

    // §16.6 检查 bracketed paste 模式切换序列
    // ESC [ ? 2004 h (enable) / ESC [ ? 2004 l (disable)
    const BRACKETED_PASTE_ENABLE: &[u8] = &[0x1B, b'[', b'?', b'2', b'0', b'0', b'4', b'h'];
    const BRACKETED_PASTE_DISABLE: &[u8] = &[0x1B, b'[', b'?', b'2', b'0', b'0', b'4', b'l'];
    if req.data == BRACKETED_PASTE_ENABLE {
        // §16.6 启用 bracketed paste
        let sessions_r = sessions.read();
        for session in sessions_r.iter() {
            let panes = session.panes.clone();
            if let Some(pane) = panes.read().get(&req.pane_id) {
                pane.set_bracketed_paste_mode(true);
            }
        }
        return Ok(ResponseBody::Error(String::new()));
    }
    if req.data == BRACKETED_PASTE_DISABLE {
        // §16.6 禁用 bracketed paste
        let sessions_r = sessions.read();
        for session in sessions_r.iter() {
            let panes = session.panes.clone();
            if let Some(pane) = panes.read().get(&req.pane_id) {
                pane.set_bracketed_paste_mode(false);
            }
        }
        return Ok(ResponseBody::Error(String::new()));
    }

    // §3.10 普通输入: 转发到 PTY
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            pane.write_input(&req.data)?;
        }
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §3.10 粘贴文本 — 调用 pane.paste (内部处理 bracketed paste markers)
async fn handle_paste(
    req: &PasteRequest,
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) -> anyhow::Result<ResponseBody> {
    let sessions_r = sessions.read();
    for session in sessions_r.iter() {
        let panes = session.panes.clone();
        if let Some(pane) = panes.read().get(&req.pane_id) {
            pane.paste(&req.text)?;
        }
    }
    Ok(ResponseBody::Error(String::new()))
}

/// §16.6 设置剪贴板
async fn handle_set_clipboard(
    req: &SetClipboardRequest,
    clipboard: &Arc<crate::clipboard::ServerClipboard>,
    outbound_tx: &mpsc::UnboundedSender<Envelope>,
) -> anyhow::Result<ResponseBody> {
    // §16.6 从 proto 消息转换并设置剪贴板
    let entry = match &req.entry {
        Some(proto_entry) => crate::clipboard::ClipboardEntry::from_proto(proto_entry),
        None => {
            return Ok(ResponseBody::Error("empty clipboard entry".to_string()));
        }
    };
    clipboard.set_clipboard(entry, outbound_tx);
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
        None => {
            Ok(ResponseBody::Clipboard(GetClipboardResponse {
                entry: Some(mux_protocol::proto::ClipboardEntry {
                    content_type: mux_protocol::proto::clipboard_entry::ClipboardContentType::Text as i32,
                    data: Vec::new(),
                    origin_host: String::new(),
                }),
            }))
        }
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
            let (lines, total, version) = pane.fetch_scrollback(
                req.from_line,
                req.direction,
                req.count,
            );
            let resp = FetchScrollbackResponse {
                lines: lines
                    .into_iter()
                    .map(|r| RowChange {
                        row: r.row,
                        cells: r.cells
                            .into_iter()
                            .map(|c| Cell {
                                char: c.character,
                                style: Some(CellStyle {
                                    bold: c.style.bold,
                                    italic: c.style.italic,
                                    underline: c.style.underline,
                                    strikethrough: c.style.strikethrough,
                                    dim: c.style.dim,
                                    reverse: c.style.reverse,
                                }),
                                foreground: c.foreground,
                                background: c.background,
                            })
                            .collect(),
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
            let (matches, version) = pane.search_scrollback(
                &req.regex,
                req.from_line,
                req.direction,
                req.max_results,
            );
            let resp = SearchScrollbackResponse {
                matches: matches
                    .into_iter()
                    .map(|(line_num, row)| SearchMatch {
                        line_number: line_num,
                        context: row.cells
                            .into_iter()
                            .map(|c| Cell {
                                char: c.character,
                                style: Some(CellStyle {
                                    bold: c.style.bold,
                                    italic: c.style.italic,
                                    underline: c.style.underline,
                                    strikethrough: c.style.strikethrough,
                                    dim: c.style.dim,
                                    reverse: c.style.reverse,
                                }),
                                foreground: c.foreground,
                                background: c.background,
                            })
                            .collect(),
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
                    update: Some(
                        FetchGridUpdateResponseUpdate::Diff(GridDiff {
                            rows: diff
                                .rows
                                .into_iter()
                                .map(|r| RowChange {
                                    row: r.row,
                                    cells: r.cells.into_iter().map(|c| Cell {
                                        char: c.character,
                                        style: Some(CellStyle {
                                            bold: c.style.bold,
                                            italic: c.style.italic,
                                            underline: c.style.underline,
                                            strikethrough: c.style.strikethrough,
                                            dim: c.style.dim,
                                            reverse: c.style.reverse,
                                        }),
                                        foreground: c.foreground,
                                        background: c.background,
                                    })
                                    .collect(),
                                })
                                .collect(),
                        }),
                    ),
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
                            cells: snapshot
                                .cells
                                .into_iter()
                                .map(|c| Cell {
                                    char: c.character,
                                    style: Some(CellStyle {
                                        bold: c.style.bold,
                                        italic: c.style.italic,
                                        underline: c.style.underline,
                                        strikethrough: c.style.strikethrough,
                                        dim: c.style.dim,
                                        reverse: c.style.reverse,
                                    }),
                                    foreground: c.foreground,
                                    background: c.background,
                                })
                                .collect(),
                            cursor: Some(CursorState {
                                col: snapshot.cursor.col,
                                row: snapshot.cursor.row,
                                style: match snapshot.cursor.style {
                                    crate::grid_sync::CursorShape::Block => 1,
                                    crate::grid_sync::CursorShape::Bar => 2,
                                    crate::grid_sync::CursorShape::Underline => 3,
                                },
                                visible: snapshot.cursor.visible,
                            }),
                            alternate_screen: snapshot.alternate_screen,
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
    Some(mux_protocol::PaneInfo {
        id: pane.id.clone(),
        cwd: pane.cwd.clone(),
        title: pane.title.read().clone(),
        command: pane.command.clone().unwrap_or_default(),
        generation: pane.generation.load(std::sync::atomic::Ordering::Relaxed),
        size: Some(mux_protocol::TerminalSize {
            cols: pane.cols.load(std::sync::atomic::Ordering::Relaxed) as u32,
            rows: pane.rows.load(std::sync::atomic::Ordering::Relaxed) as u32,
        }),
        is_alive: pane.alive.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// §15.4 / §16.9 把内部 LayoutTree 转成 proto LayoutTree。
/// 空根 (session 初始状态) 转 `root: None`。
fn layout_tree_to_proto(
    tree: &crate::layout::LayoutTree,
) -> mux_protocol::LayoutTree {
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
                let proto_children: Vec<ProtoNode> =
                    children.iter().filter_map(convert).collect();
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
