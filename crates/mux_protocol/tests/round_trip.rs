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
                    underline: true,
                    underline_style: proto::cell_style::UnderlineStyle::Curly as i32,
                    underline_color: Some(0x123456),
                    wide_char: true,
                    wrapline: true,
                    ..Default::default()
                }),
                foreground: 0xFFFFFF,
                background: 0x000000,
                zerowidth: "\u{301}".into(),
                hyperlink: Some(Hyperlink {
                    id: "link-id".into(),
                    uri: "https://example.com".into(),
                }),
            }],
        }],
    };

    let mut buf = Vec::new();
    diff.encode(&mut buf).unwrap();
    let decoded = GridDiff::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.rows.len(), 1);
    assert_eq!(decoded.rows[0].row, 5);
    let cell = &decoded.rows[0].cells[0];
    assert_eq!(cell.zerowidth, "\u{301}");
    assert_eq!(
        cell.hyperlink.as_ref().map(|link| link.uri.as_str()),
        Some("https://example.com")
    );
    let style = cell
        .style
        .as_ref()
        .unwrap_or_else(|| panic!("cell style missing"));
    assert_eq!(
        style.underline_style,
        proto::cell_style::UnderlineStyle::Curly as i32
    );
    assert_eq!(style.underline_color, Some(0x123456));
    assert!(style.wide_char);
    assert!(style.wrapline);
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

#[test]
fn read_file_pagination_round_trip() {
    let request = ReadFileRequest {
        path: "large.bin".to_string(),
        offset_line: None,
        max_lines: None,
        offset_bytes: Some(4096),
        max_bytes: Some(1024),
    };
    let encoded = request.encode_to_vec();
    let decoded = ReadFileRequest::decode(encoded.as_slice()).expect("decode request");
    assert_eq!(decoded.offset_bytes, Some(4096));
    assert_eq!(decoded.max_bytes, Some(1024));

    let response = ReadFileResponse {
        content: vec![1, 2, 3],
        is_binary: true,
        encoding: "binary".to_string(),
        offset_line: 0,
        next_offset_line: None,
        total_lines: 0,
        offset_bytes: 4096,
        next_offset_bytes: Some(4099),
        total_bytes: 8192,
    };
    let encoded = response.encode_to_vec();
    let decoded = ReadFileResponse::decode(encoded.as_slice()).expect("decode response");
    assert_eq!(decoded.content, vec![1, 2, 3]);
    assert_eq!(decoded.next_offset_bytes, Some(4099));
    assert_eq!(decoded.total_bytes, 8192);
}

// §3.3 验证 FullGridSnapshot 大规模单元格编码/解码。
#[test]
fn test_full_snapshot_serialization() {
    let snap = FullGridSnapshot {
        cols: 80,
        rows: 24,
        cells: vec![
            Cell {
                char: " ".into(),
                ..Default::default()
            };
            80 * 24
        ],
        cursor: Some(CursorState {
            col: 0,
            row: 0,
            style: proto::cursor_state::CursorStyle::Hidden as i32,
            visible: false,
            blinking: true,
        }),
        alternate_screen: false,
        display_offset: 42,
        history_size: 37,
        history_version: 19,
        modes: Some(terminal_mode::APP_CURSOR | terminal_mode::BRACKETED_PASTE),
    };

    let mut buf = Vec::new();
    snap.encode(&mut buf).unwrap();
    let decoded = FullGridSnapshot::decode(buf.as_slice()).unwrap();
    assert_eq!(decoded.cols, 80);
    assert_eq!(decoded.rows, 24);
    assert_eq!(decoded.cells.len(), 80 * 24);
    // §15.12 display_offset (field 6) survives encode/decode as a nonzero value.
    assert_eq!(decoded.display_offset, 42);
    let cursor = decoded.cursor.unwrap_or_else(|| panic!("cursor missing"));
    assert_eq!(
        cursor.style,
        proto::cursor_state::CursorStyle::Hidden as i32
    );
    assert!(cursor.blinking);
    assert_eq!(
        decoded.modes,
        Some(terminal_mode::APP_CURSOR | terminal_mode::BRACKETED_PASTE)
    );
}

#[test]
fn test_scrollback_request_response_round_trip() {
    let request = FetchScrollbackRequest {
        pane_id: "session:pane".into(),
        from_line: 12,
        direction: 1,
        count: 3,
    };
    let mut encoded = Vec::new();
    request.encode(&mut encoded).unwrap();
    let decoded = FetchScrollbackRequest::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.pane_id, request.pane_id);
    assert_eq!(decoded.from_line, 12);
    assert_eq!(decoded.direction, 1);
    assert_eq!(decoded.count, 3);

    let response = FetchScrollbackResponse {
        lines: vec![
            RowChange {
                row: 12,
                cells: vec![Cell { char: "a".into(), ..Default::default() }],
            },
            RowChange {
                row: 13,
                cells: vec![Cell { char: "b".into(), ..Default::default() }],
            },
        ],
        total_lines: 37,
        scrollback_version: 19,
    };
    encoded.clear();
    response.encode(&mut encoded).unwrap();
    let decoded = FetchScrollbackResponse::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded, response);
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

// §15.4 验证 SessionLayoutChanged 的 snapshot 字段往返: 重连合成广播携带
// 完整权威快照, 普通布局 delta 保持 snapshot 缺省 (None), 双向兼容。
#[test]
fn session_layout_changed_snapshot_round_trip() {
    let snapshot = SessionSnapshot {
        tabs: vec![TabInfo {
            id: "tab-1".to_string(),
            title: "editor".to_string(),
            panes: vec![PaneInfo {
                id: "pane-1".to_string(),
                title: "vim".to_string(),
                generation: 7,
                zoomed: true,
                ..Default::default()
            }],
        }],
        focused_pane_id: "pane-1".to_string(),
        focused_tab_id: "tab-1".to_string(),
        session_id: "session-1".to_string(),
        ..Default::default()
    };
    let with_snapshot = Notification {
        event: Some(proto::notification::Event::SessionLayoutChanged(
            SessionLayoutChanged {
                layout: Some(LayoutTree {
                    root: Some(LayoutNode {
                        id: "root".to_string(),
                        node: Some(proto::layout_node::Node::Pane(PaneLeaf {
                            pane_id: "pane-1".to_string(),
                        })),
                    }),
                }),
                snapshot: Some(snapshot),
            },
        )),
    };

    let framed = frame(&Envelope {
        version: Some(PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Notification(with_snapshot)),
    })
    .expect("frame snapshot-carrying layout change");
    let (decoded, consumed) = unframe(&framed).expect("unframe snapshot-carrying layout change");
    assert_eq!(consumed, framed.len());
    let changed = match decoded.payload {
        Some(proto::envelope::Payload::Notification(Notification {
            event: Some(proto::notification::Event::SessionLayoutChanged(changed)),
        })) => changed,
        payload => panic!("expected SessionLayoutChanged, got {payload:?}"),
    };
    let decoded_snapshot = changed
        .snapshot
        .expect("reconnect resync must carry the snapshot");
    assert_eq!(decoded_snapshot.focused_pane_id, "pane-1");
    assert_eq!(decoded_snapshot.tabs[0].panes[0].generation, 7);
    assert!(decoded_snapshot.tabs[0].panes[0].zoomed);

    // §15.4 ordinary server layout notifications keep the field absent, so a
    // reconnect resync stays distinguishable from a plain layout delta.
    let delta_only = SessionLayoutChanged {
        layout: Some(LayoutTree { root: None }),
        snapshot: None,
    };
    let mut encoded = Vec::new();
    delta_only
        .encode(&mut encoded)
        .expect("encode layout-only change");
    let decoded = SessionLayoutChanged::decode(encoded.as_slice())
        .expect("decode layout-only change");
    assert!(decoded.snapshot.is_none(), "layout delta must stay snapshot-free");

    // Old peers that never set the field decode as None too (wire default).
    let legacy = SessionLayoutChanged {
        layout: Some(LayoutTree { root: None }),
        snapshot: None,
    };
    assert_eq!(
        legacy.snapshot,
        None,
        "unset snapshot field defaults to None on the wire"
    );
}

#[test]
fn recovery_messages_round_trip() {
    let request = Request {
        request_id: 42,
        body: Some(proto::request::Body::ConfirmRecovery(
            ConfirmRecoveryRequest {
                session_id: "session-1".to_string(),
            },
        )),
    };
    let mut encoded = Vec::new();
    request
        .encode(&mut encoded)
        .expect("encode recovery request");
    let decoded = Request::decode(encoded.as_slice()).expect("decode recovery request");
    match decoded.body {
        Some(proto::request::Body::ConfirmRecovery(request)) => {
            assert_eq!(request.session_id, "session-1");
        }
        body => panic!("unexpected recovery request body: {body:?}"),
    }

    let response = Response {
        request_id: 42,
        body: Some(proto::response::Body::RecoveryCandidates(
            ListRecoveryCandidatesResponse {
                candidates: vec![RecoveryCandidateInfo {
                    id: "session-1".to_string(),
                    name: "shells".to_string(),
                    cwd: "/tmp".to_string(),
                    metadata_complete: true,
                    pane_ids: vec!["pane-1".to_string()],
                }],
                rejected: vec!["invalid persisted layout for session old-1".to_string()],
            },
        )),
    };
    encoded.clear();
    response
        .encode(&mut encoded)
        .expect("encode recovery response");
    let decoded = Response::decode(encoded.as_slice()).expect("decode recovery response");
    match decoded.body {
        Some(proto::response::Body::RecoveryCandidates(response)) => {
            assert_eq!(response.candidates[0].pane_ids, vec!["pane-1"]);
            assert_eq!(
                response.rejected,
                vec!["invalid persisted layout for session old-1"]
            );
        }
        body => panic!("unexpected recovery response body: {body:?}"),
    }
}

#[test]
fn file_review_messages_round_trip() {
    let review_state = GetFileReviewStateResponse {
        versions: vec![FileVersion {
            version_id: 7,
            seq_no: 11,
            trigger: "create".to_string(),
        }],
        latest_seq_no: 11,
        current_exists: true,
        current_size: 3,
        current_sha256: vec![9; 32],
        current_state: FileContentState::Text as i32,
        current_content: b"new".to_vec(),
    };
    let response = Response {
        request_id: 41,
        body: Some(proto::response::Body::FileReviewState(
            review_state.clone(),
        )),
    };
    let decoded = Response::decode(response.encode_to_vec().as_slice()).expect("decode");
    match decoded.body {
        Some(proto::response::Body::FileReviewState(decoded)) => {
            assert_eq!(decoded, review_state);
        }
        body => panic!("unexpected review-state response body: {body:?}"),
    }

    let decline = DeclineFileVersionRequest {
        session_id: "session".to_string(),
        path: "file.txt".to_string(),
        version_id: 7,
        expected_latest_seq_no: 11,
        expected_current_exists: true,
        expected_current_sha256: vec![9; 32],
    };
    let request = Request {
        request_id: 42,
        body: Some(proto::request::Body::DeclineFileVersion(decline.clone())),
    };
    let decoded = Request::decode(request.encode_to_vec().as_slice()).expect("decode");
    match decoded.body {
        Some(proto::request::Body::DeclineFileVersion(decoded)) => {
            assert_eq!(decoded, decline);
        }
        body => panic!("unexpected decline request body: {body:?}"),
    }

    let historical = GetFileVersionResponse {
        content: Vec::new(),
        total_bytes: 4,
        state: FileContentState::Binary as i32,
        content_available: false,
    };
    let decoded =
        GetFileVersionResponse::decode(historical.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(decoded, historical);

    let restored = DeclineFileVersionResponse {
        restored: true,
        restored_version_id: 9,
        restored_seq_no: 13,
    };
    let decoded =
        DeclineFileVersionResponse::decode(restored.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(decoded, restored);
}
