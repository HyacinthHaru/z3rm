// §9 / §3.3 mux_protocol 序列化与帧 round-trip 单元测试。
use mux_protocol::*;
use prost::Message;

// §3.3 验证 GridDiff 可以直接编码/解码。
#[test]
fn test_grid_diff_round_trip() {
    let diff = GridDiff {
        rows: vec![RowChange {
            row: 5,
            cells: vec![Cell {
                char: "H".into(),
                style: Some(CellStyle {
                    bold: true,
                    ..Default::default()
                }),
                foreground: 0xFFFFFF,
                background: 0x000000,
            }],
        }],
    };

    let mut buf = Vec::new();
    diff.encode(&mut buf).unwrap();
    let decoded = GridDiff::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.rows.len(), 1);
    assert_eq!(decoded.rows[0].row, 5);
}

// §9 验证 Envelope 的 frame / unframe 往返。
#[test]
fn test_frame_unframe_round_trip() {
    let env = Envelope {
        version: Some(PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Notification(Notification {
            event: Some(proto::notification::Event::PaneDirty(PaneDirty {
                pane_id: "w1:p1".into(),
            })),
        })),
    };

    let framed = frame(&env).unwrap();
    let (decoded, consumed) = unframe(&framed).unwrap();
    assert_eq!(consumed, framed.len());
    assert!(matches!(
        decoded.payload,
        Some(proto::envelope::Payload::Notification(_))
    ));
}

// §3.3 验证 FullGridSnapshot 大规模单元格编码/解码。
#[test]
fn test_full_snapshot_serialization() {
    let snap = FullGridSnapshot {
        cols: 80,
        rows: 24,
        cells: vec![Cell {
            char: " ".into(),
            style: None,
            foreground: 0,
            background: 0,
        }; 80 * 24],
        cursor: Some(CursorState {
            col: 0,
            row: 0,
            style: proto::cursor_state::CursorStyle::Block as i32,
            visible: true,
        }),
        alternate_screen: false,
        display_offset: 42,
    };

    let mut buf = Vec::new();
    snap.encode(&mut buf).unwrap();
    let decoded = FullGridSnapshot::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.cols, 80);
    assert_eq!(decoded.rows, 24);
    assert_eq!(decoded.cells.len(), 80 * 24);
    // §15.12 display_offset (field 6) survives encode/decode as a nonzero value.
    assert_eq!(decoded.display_offset, 42);
}

// §3.3 验证 ClientIdentity 编码/解码 (Plan 33)
#[test]
fn test_client_identity_round_trip() {
    let identity = ClientIdentity {
        client_id: "test-uuid-123".to_string(),
        role: proto::ClientRole::Admin as i32,
    };

    let mut buf = Vec::new();
    identity.encode(&mut buf).unwrap();
    let decoded = ClientIdentity::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.client_id, "test-uuid-123");
    assert_eq!(decoded.role, proto::ClientRole::Admin as i32);
}

// §3.3 验证 AttachRequest 含 ClientIdentity 的 round-trip (Plan 33)
#[test]
fn test_attach_request_with_identity() {
    let req = AttachRequest {
        session_id: "session-1".to_string(),
        mode: proto::attach_request::AttachMode::Shared as i32,
        window_id: "win-1".to_string(),
        identity: Some(ClientIdentity {
            client_id: "client-abc".to_string(),
            role: proto::ClientRole::ReadOnly as i32,
        }),
    };

    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    let decoded = AttachRequest::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.session_id, "session-1");
    assert_eq!(decoded.window_id, "win-1");
    assert!(decoded.identity.is_some());
    let identity = decoded.identity.unwrap();
    assert_eq!(identity.client_id, "client-abc");
    assert_eq!(identity.role, proto::ClientRole::ReadOnly as i32);
}

// §9 验证 PaneTitleChanged Notification round-trip。
#[test]
fn test_pane_title_changed_notification() {
    let env = Envelope {
        version: Some(PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Notification(Notification {
            event: Some(proto::notification::Event::PaneTitleChanged(
                PaneTitleChanged {
                    pane_id: "p1".into(),
                    title: "my title".into(),
                },
            )),
        })),
    };

    let framed = frame(&env).unwrap();
    let (decoded, consumed) = unframe(&framed).unwrap();
    assert_eq!(consumed, framed.len());
    assert!(matches!(
        decoded.payload,
        Some(proto::envelope::Payload::Notification(_))
    ));
}

// §9 验证 PaneBell Notification round-trip。
#[test]
fn test_pane_bell_notification() {
    let env = Envelope {
        version: Some(PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Notification(Notification {
            event: Some(proto::notification::Event::PaneBell(PaneBell {
                pane_id: "p1".into(),
            })),
        })),
    };

    let framed = frame(&env).unwrap();
    let (decoded, consumed) = unframe(&framed).unwrap();
    assert_eq!(consumed, framed.len());
    assert!(matches!(
        decoded.payload,
        Some(proto::envelope::Payload::Notification(_))
    ));
}

// §4 验证 FileVersion 消息直接编码/解码（version_id / seq_no / trigger）。
#[test]
fn test_file_version_round_trip() {
    let version = FileVersion {
        version_id: 42,
        seq_no: 7,
        trigger: "edit".to_string(),
    };

    let mut buf = Vec::new();
    version.encode(&mut buf).unwrap();
    let decoded = FileVersion::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.version_id, 42);
    assert_eq!(decoded.seq_no, 7);
    assert_eq!(decoded.trigger, "edit");
}

// §4 验证 ListFileVersionsRequest 经由 Envelope frame/unframe 往返，
// 且 Request/Response oneof 新字段（30-32 / 19-21）正确解码。
#[test]
fn test_shadow_file_version_envelope_round_trip() {
    let env = Envelope {
        version: Some(PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Request(Request {
            request_id: 99,
            body: Some(proto::request::Body::ListFileVersions(
                ListFileVersionsRequest {
                    session_id: "s1".to_string(),
                    path: "/tmp/notes.md".to_string(),
                },
            )),
        })),
    };

    let framed = frame(&env).unwrap();
    let (decoded, consumed) = unframe(&framed).unwrap();
    assert_eq!(consumed, framed.len());

    let req = match decoded.payload {
        Some(proto::envelope::Payload::Request(Request {
            body: Some(proto::request::Body::ListFileVersions(req)),
            ..
        })) => req,
        _ => panic!("expected ListFileVersions request after round-trip"),
    };
    assert_eq!(req.session_id, "s1");
    assert_eq!(req.path, "/tmp/notes.md");
}
