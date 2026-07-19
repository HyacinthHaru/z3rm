//! # Protocol compile-time check
//!
//! 这不是端到端集成测试。它只验证 mux_protocol 类型可以构造、字段名匹配,
//! 以及 frame/unframe 序列化往返正确。真正的 e2e 测试在 crates/mux/tests/e2e.rs。
//!
//! 保留这个文件作为 protobuf schema 回归测试:proto 字段重命名或类型
//! 变化会让本测试编译失败,提前捕获。

use anyhow::Result;

// ============================================================
// §3.10 完整会话生命周期测试 (编译时验证)
// ============================================================

/// §3.10 会话生命周期 API 签名验证
#[test]
fn test_session_lifecycle_api() -> Result<()> {
    // 步骤 2: 创建会话请求
    let create_req = mux_protocol::CreateSessionRequest {
        name: "test".into(),
        cwd: "/tmp".into(),
    };
    assert_eq!(create_req.name, "test");
    assert_eq!(create_req.cwd, "/tmp");

    // 步骤 3: 创建 Pane 请求
    let spawn_pane_req = mux_protocol::SpawnPaneRequest {
        session_id: "test-session".into(),
        tab_id: "tab-1".into(),
        cwd: "/tmp".into(),
        command: Some(mux_protocol::proto::ShellCommand {
            program: "/bin/bash".into(),
            args: vec!["-l".into()],
            env: vec![],
        }),
    };
    assert_eq!(spawn_pane_req.session_id, "test-session");

    // 步骤 4: 发送输入请求
    let input_req = mux_protocol::SendInputRequest {
        pane_id: "p1".into(),
        data: "echo hello\n".as_bytes().to_vec(),
    };
    assert_eq!(String::from_utf8(input_req.data.clone())?, "echo hello\n");

    // 步骤 5: 获取网格更新请求
    let grid_req = mux_protocol::FetchGridUpdateRequest {
        pane_id: "p1".into(),
        since_generation: 0,
    };
    assert_eq!(grid_req.since_generation, 0);

    // 步骤 6: 分割 Pane 请求
    let split_req = mux_protocol::SplitPaneRequest {
        pane_id: "p1".into(),
        direction: mux_protocol::proto::split_pane_request::SplitDirection::Right as i32,
        command: None,
    };
    assert_eq!(split_req.pane_id, "p1");

    // 步骤 8: 聚焦 Pane 请求
    let focus_req = mux_protocol::FocusPaneRequest { pane_id: "p2".into() };
    assert_eq!(focus_req.pane_id, "p2");

    // 步骤 9: 断开请求
    let _detach_req = mux_protocol::DetachRequest {};

    // 步骤 13: 关闭会话请求
    let kill_req = mux_protocol::KillSessionRequest { id: "test-session".into() };
    assert_eq!(kill_req.id, "test-session");

    // 验证 Envelope 构造
    let envelope = mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(
            mux_protocol::Request {
                request_id: 1,
                body: Some(mux_protocol::proto::request::Body::CreateSession(create_req)),
            },
        )),
    };

    let framed = mux_protocol::frame(&envelope)?;
    let (decoded, consumed) = mux_protocol::unframe(&framed)?;
    assert_eq!(consumed, framed.len());
    assert!(decoded.version.is_some());
    Ok(())
}

/// §3.10 PaneInfo 字段验证
#[test]
fn test_pane_info_fields() -> Result<()> {
    let info = mux_protocol::PaneInfo {
        id: "w1:p1".into(), title: "bash".into(), cwd: "/tmp".into(),
        command: Some("/bin/bash -l".into()), generation: 100,
        cols: 80, rows: 24, alive: true,
    };
    assert_eq!(info.id, "w1:p1");
    assert_eq!(info.generation, 100);
    assert!(info.alive);

    let mut buf = Vec::new();
    info.encode(&mut buf)?;
    let decoded = mux_protocol::PaneInfo::decode(buf.as_slice())?;
    assert_eq!(decoded.id, "w1:p1");
    Ok(())
}

/// §3.10 SessionSnapshot 结构验证
#[test]
fn test_session_snapshot_structure() -> Result<()> {
    let snapshot = mux_protocol::SessionSnapshot {
        session_id: "sess-1".into(),
        tabs: vec![mux_protocol::TabInfo {
            id: "tab-1".into(), title: "工作区".into(),
            panes: vec!["p1".into(), "p2".into()],
        }],
        layout: Some(mux_protocol::LayoutTree {
            root: Some(mux_protocol::proto::LayoutNode {
                id: "s1".into(),
                node: Some(mux_protocol::proto::layout_node::Node::Split(
                    mux_protocol::proto::SplitNode {
                        direction: mux_protocol::proto::split_node::SplitDirection::Horizontal as i32,
                        children: vec![
                            mux_protocol::proto::LayoutNode {
                                id: "p1n".into(),
                                node: Some(mux_protocol::proto::layout_node::Node::Pane(
                                    mux_protocol::proto::PaneLeaf { pane_id: "p1".into() },
                                )),
                            },
                            mux_protocol::proto::LayoutNode {
                                id: "p2n".into(),
                                node: Some(mux_protocol::proto::layout_node::Node::Pane(
                                    mux_protocol::proto::PaneLeaf { pane_id: "p2".into() },
                                )),
                            },
                        ],
                        ratios: vec![0.5, 0.5],
                    },
                )),
            }),
        }),
        focused_pane: "p1".into(),
    };

    assert_eq!(snapshot.session_id, "sess-1");
    assert_eq!(snapshot.tabs.len(), 1);
    assert_eq!(snapshot.tabs[0].panes.len(), 2);

    let mut buf = Vec::new();
    snapshot.encode(&mut buf)?;
    let decoded = mux_protocol::SessionSnapshot::decode(buf.as_slice())?;
    assert_eq!(decoded.session_id, "sess-1");
    Ok(())
}

/// §3.10 Grid Update Response 验证
#[test]
fn test_grid_update_response() -> Result<()> {
    let diff_response = mux_protocol::FetchGridUpdateResponse {
        update: Some(
            mux_protocol::proto::fetch_grid_update_response::Update::Diff(
                mux_protocol::GridDiff {
                    rows: vec![mux_protocol::RowChange {
                        row: 0,
                        cells: vec![mux_protocol::Cell {
                            char: "h".into(), style: None,
                            foreground: 0, background: 0,
                        }],
                    }],
                },
            ),
        ),
    };

    let _snapshot_response = mux_protocol::FetchGridUpdateResponse {
        update: Some(
            mux_protocol::proto::fetch_grid_update_response::Update::Snapshot(
                mux_protocol::FullGridSnapshot {
                    cols: 80, rows: 24, cells: vec![],
                    cursor: None, alternate_screen: false,
                },
            ),
        ),
    };

    let mut buf = Vec::new();
    diff_response.encode(&mut buf)?;
    let _decoded = mux_protocol::FetchGridUpdateResponse::decode(buf.as_slice())?;
    Ok(())
}

/// §9 AttachRequest AttachMode 验证
#[test]
fn test_attach_modes() -> Result<()> {
    let shared = mux_protocol::AttachRequest {
        session_id: "sess-1".into(),
        mode: Some(mux_protocol::AttachMode::Shared as i32),
    };
    assert_eq!(shared.session_id, "sess-1");

    let mut buf = Vec::new();
    shared.encode(&mut buf)?;
    let decoded = mux_protocol::AttachRequest::decode(buf.as_slice())?;
    assert_eq!(decoded.session_id, "sess-1");
    Ok(())
}

/// §3.10 通知事件类型验证
#[test]
fn test_notification_events() -> Result<()> {
    let dirty = mux_protocol::Notification {
        event: Some(mux_protocol::proto::notification::Event::PaneDirty(
            mux_protocol::PaneDirty { pane_id: "p1".into() },
        )),
    };

    let added = mux_protocol::Notification {
        event: Some(mux_protocol::proto::notification::Event::PaneAdded(
            mux_protocol::PaneAdded { pane_id: "p2".into(), session_id: "sess-1".into() },
        )),
    };

    let removed = mux_protocol::Notification {
        event: Some(mux_protocol::proto::notification::Event::PaneRemoved(
            mux_protocol::PaneRemoved { pane_id: "p1".into(), exit_code: Some(0) },
        )),
    };

    let focused = mux_protocol::Notification {
        event: Some(mux_protocol::proto::notification::Event::PaneFocused(
            mux_protocol::PaneFocused { pane_id: "p2".into() },
        )),
    };

    let layout = mux_protocol::Notification {
        event: Some(mux_protocol::proto::notification::Event::LayoutChanged(
            mux_protocol::SessionLayoutChanged {
                layout: Some(mux_protocol::LayoutTree { root: None }),
            },
        )),
    };

    let notifications = [dirty, added, removed, focused, layout];
    for notif in &notifications {
        let mut buf = Vec::new();
        notif.encode(&mut buf)?;
        let _decoded = mux_protocol::Notification::decode(buf.as_slice())?;
    }
    Ok(())
}

/// §9 Request/Response 消息构建
#[test]
fn test_request_response_flow() -> Result<()> {
    let request = mux_protocol::Request {
        request_id: 42,
        body: Some(mux_protocol::proto::request::Body::SpawnPane(
            mux_protocol::SpawnPaneRequest {
                session_id: "sess-1".into(), tab_id: "tab-1".into(),
                cwd: "/tmp".into(), command: None,
            },
        )),
    };

    let response = mux_protocol::Response {
        request_id: 42, error: "".into(),
        body: Some(mux_protocol::proto::response::Body::SpawnPane(
            mux_protocol::PaneInfo {
                id: "w1:p2".into(), title: "bash".into(), cwd: "/tmp".into(),
                command: None, generation: 0, cols: 80, rows: 24, alive: true,
            },
        )),
    };

    assert_eq!(request.request_id, response.request_id);

    let req_envelope = mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(request)),
    };

    let resp_envelope = mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Response(response)),
    };

    let req_frame = mux_protocol::frame(&req_envelope)?;
    let (req_decoded, _) = mux_protocol::unframe(&req_frame)?;
    assert!(matches!(
        req_decoded.payload,
        Some(mux_protocol::proto::envelope::Payload::Request(_))
    ));

    let resp_frame = mux_protocol::frame(&resp_envelope)?;
    let (resp_decoded, _) = mux_protocol::unframe(&resp_frame)?;
    assert!(matches!(
        resp_decoded.payload,
        Some(mux_protocol::proto::envelope::Payload::Response(_))
    ));
    Ok(())
}
