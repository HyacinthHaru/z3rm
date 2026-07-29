//! # mux domain compile tests
//!
//! 编译时测试：验证 MuxDomain API 签名、类型兼容性、proto 类型使用。
//! 真正的 e2e 测试在 crates/mux/tests/e2e.rs。

use mux::{MuxDomain, MuxNotification, MuxTransport};
use mux_protocol::*;

/// §9 编译时测试：MuxDomain 方法签名。
#[test]
fn test_compile_api_signatures() {
    // §9 验证 MuxDomain 是 Send + Sync（可在多线程间共享）。
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MuxDomain>();

    // §9 验证 MuxNotification 是 Send + Sync。
    assert_send_sync::<MuxNotification>();
}

/// §3.10 编译时测试：SessionInfo 字段访问。
#[test]
fn test_session_info_fields() {
    let info = SessionInfo {
        id: "test".to_string(),
        name: "default".to_string(),
        cwd: "/tmp".to_string(),
        created_timestamp: 0,
        attached_clients: 1,
    };
    assert_eq!(info.id, "test");
}

/// §3.3 编译时测试：TerminalSize 字段访问。
#[test]
fn test_terminal_size_fields() {
    let size = TerminalSize { cols: 80, rows: 24 };
    assert_eq!(size.cols, 80);
    assert_eq!(size.rows, 24);
}

/// §3.3 编译时测试：FetchGridUpdateResponse Update 枚举。
#[test]
fn test_grid_update_response() {
    let diff = GridDiff { rows: vec![] };
    let update1 = FetchGridUpdateResponse {
        from_generation: 1,
        to_generation: 2,
        update: Some(fetch_grid_update_response::Update::Diff(diff)),
    };
    assert!(matches!(
        update1.update,
        Some(fetch_grid_update_response::Update::Diff(_))
    ));

    let snapshot = FullGridSnapshot {
        cols: 80,
        rows: 24,
        cells: vec![],
        cursor: Some(CursorState {
            col: 0,
            row: 0,
            style: 0,
            visible: true,
            blinking: false,
        }),
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    let update2 = FetchGridUpdateResponse {
        from_generation: 100,
        to_generation: 105,
        update: Some(fetch_grid_update_response::Update::FullSnapshot(snapshot)),
    };
    assert!(matches!(
        update2.update,
        Some(fetch_grid_update_response::Update::FullSnapshot(_))
    ));
}

/// §3.10 编译时测试：AttachMode 枚举。
#[test]
fn test_attach_mode() {
    let shared = attach_request::AttachMode::Shared as i32;
    let steal = attach_request::AttachMode::Steal as i32;
    let read_only = attach_request::AttachMode::ReadOnly as i32;
    assert_eq!(shared, 1);
    assert_eq!(steal, 2);
    assert_eq!(read_only, 3);
}

/// §3.10 编译时测试：SplitDirection 枚举。
#[test]
fn test_split_direction() {
    let left_right = split_node::SplitDirection::LeftRight as i32;
    let top_bottom = split_node::SplitDirection::TopBottom as i32;
    assert_eq!(left_right, 1);
    assert_eq!(top_bottom, 2);
}

/// §9 编译时测试：Request/Response 消息构建。
#[test]
fn test_request_construction() {
    let list_req = Request {
        request_id: 1,
        body: Some(request::Body::ListSessions(ListSessionsRequest {})),
    };
    assert_eq!(list_req.request_id, 1);

    let create_req = Request {
        request_id: 2,
        body: Some(request::Body::CreateSession(CreateSessionRequest {
            name: "test".to_string(),
            cwd: "/tmp".to_string(),
        })),
    };
    assert_eq!(create_req.request_id, 2);
}

/// §9 编译时测试：Notification 事件类型。
#[test]
fn test_notification_events() {
    let dirty = Notification {
        event: Some(notification::Event::PaneDirty(PaneDirty {
            pane_id: "p1".to_string(),
        })),
    };
    assert!(matches!(
        dirty.event,
        Some(notification::Event::PaneDirty(_))
    ));

    let added = Notification {
        event: Some(notification::Event::PaneAdded(PaneAdded {
            pane_id: "p2".to_string(),
            tab_id: "t1".to_string(),
        })),
    };
    assert!(matches!(
        added.event,
        Some(notification::Event::PaneAdded(_))
    ));
}

/// §9 编译时测试：MuxTransport 枚举。
#[test]
fn test_transport_enum() {
    let _local = MuxTransport::Local;
}

/// §9 编译时测试：frame/unframe 函数。
#[test]
fn test_frame_unframe() {
    let request = Request {
        request_id: 1,
        body: Some(request::Body::ListSessions(ListSessionsRequest {})),
    };
    let envelope = Envelope {
        version: Some(PROTOCOL_VERSION),
        payload: Some(envelope::Payload::Request(request)),
    };

    let framed = frame(&envelope).expect("frame failed");
    let (decoded, consumed) = mux_protocol::unframe(&framed).expect("unframe failed");

    assert_eq!(consumed, framed.len());
    assert!(decoded.payload.is_some());
}
