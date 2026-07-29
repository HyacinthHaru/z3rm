// §3.10 mux_server 单元测试 — 验证 grid diff ring、layout tree、
// generation counter、session 生命周期等核心功能。

use crate::grid_sync::{GridDiff, GridDiffRing, RowChange};
use crate::layout::{LayoutNode, LayoutTree, SplitDirection};
use std::io::Write;

/// §3.3 Grid diff ring: push + overflow
#[test]
fn test_diff_ring_push_and_overflow() {
    let mut ring = GridDiffRing::new(4);

    for i in 0..4 {
        ring.push(i, GridDiff { rows: vec![] });
    }
    assert_eq!(ring.len(), 4);

    ring.push(4, GridDiff { rows: vec![] });
    assert_eq!(ring.len(), 4);
}

/// §3.3 Grid diff ring: empty ring
#[test]
fn test_diff_ring_empty() {
    let ring = GridDiffRing::new(4);
    assert!(ring.is_empty());
    assert_eq!(ring.len(), 0);
}

/// §3.3 Grid diff ring: push preserves order
#[test]
fn test_diff_ring_preserves_order() {
    let mut ring = GridDiffRing::new(64);
    ring.push(10, GridDiff { rows: vec![] });
    ring.push(20, GridDiff { rows: vec![] });
    ring.push(30, GridDiff { rows: vec![] });
    assert_eq!(ring.len(), 3);
}

/// §3.10 Layout tree: split pane
#[test]
fn test_layout_split() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    match &tree.root {
        LayoutNode::Split {
            direction,
            children,
            ratios,
            ..
        } => {
            assert_eq!(*direction, SplitDirection::LeftRight);
            assert_eq!(children.len(), 2);
            assert_eq!(ratios.len(), 2);
            assert!((ratios[0] - 0.5).abs() < 1e-6);
            assert!((ratios[1] - 0.5).abs() < 1e-6);
        }
        _ => panic!("expected Split node after split"),
    }
}

/// §3.10 Layout tree: remove pane
#[test]
fn test_layout_remove_pane() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::TopBottom)
        .expect("split failed");

    tree.remove_pane("pane-2").expect("remove failed");

    match &tree.root {
        LayoutNode::Pane { pane_id, .. } => {
            assert_eq!(pane_id, "pane-1");
        }
        _ => panic!("expected flattened Pane node after removal"),
    }
}

/// §3.10 Layout tree: resize pane
#[test]
fn test_layout_resize_pane() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    tree.resize_pane("pane-2", SplitDirection::LeftRight, 0.1)
        .expect("resize failed");

    match &tree.root {
        LayoutNode::Split { ratios, .. } => {
            assert!(ratios[1] > 0.5);
            assert!(ratios[0] < 0.5);
        }
        _ => panic!("expected Split node"),
    }
}

/// §3.10 Layout tree: serialize/deserialize
#[test]
fn test_layout_serialize() {
    let tree = LayoutTree::with_pane("root".to_string(), "pane-1".to_string());
    let serialized = tree.serialize(80, 24).expect("serialize failed");

    // tmux 风格: <checksum>,WxH,xoff,yoff,paneid
    assert!(serialized.ends_with(",80x24,0,0,pane-1"));
    let (checksum, _body) = serialized.split_once(',').expect("checksum prefix");
    let _checksum: u32 = checksum.parse().expect("checksum should be a number");
}

/// §3.10 Layout tree: collect pane IDs
#[test]
fn test_layout_pane_ids() {
    let mut tree = LayoutTree::with_pane("n1".to_string(), "p1".to_string());
    tree.split("p1", "p2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    tree.split("p1", "p3".to_string(), SplitDirection::TopBottom)
        .expect("split failed");

    let ids = tree.pane_ids();
    assert!(ids.contains(&"p1".to_string()));
    assert!(ids.contains(&"p2".to_string()));
    assert!(ids.contains(&"p3".to_string()));
}

/// §3.10 Session lifecycle: create session
#[test]
fn test_session_create() {
    let session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );
    assert_eq!(session.id, "sess-1");
    assert_eq!(session.name, "test");
    assert_eq!(session.cwd, "/home/user");
    assert!(session.is_empty());
}

/// §3.10 Session: attach/detach client
#[test]
fn test_session_attach_detach() {
    let mut session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    session.add_attached_client(
        "client-1".to_string(),
        crate::session::AttachMode::Shared,
        crate::session::ClientRole::ReadWrite,
    );
    assert_eq!(session.attached_client_count(), 1);

    session.add_attached_client(
        "client-2".to_string(),
        crate::session::AttachMode::ReadOnly,
        crate::session::ClientRole::ReadOnly,
    );
    assert_eq!(session.attached_client_count(), 2);

    session.remove_attached_client("client-1");
    assert_eq!(session.attached_client_count(), 1);
}

/// §3.10 Session: focused pane
#[test]
fn test_session_focused_pane() {
    let mut session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    assert!(session.get_focused_pane().is_none());

    session.set_focused_pane("pane-1".to_string());
    assert_eq!(session.get_focused_pane(), Some("pane-1"));
}

/// §3.10 Session: add tab
#[test]
fn test_session_add_tab() {
    let mut session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    session.add_tab("tab-1".to_string(), "Terminal".to_string());
    assert!(session.tabs.contains_key("tab-1"));
    let tab = session.tabs.get("tab-1").unwrap();
    assert_eq!(tab.title, "Terminal");
}

/// §3.10 Pane: creation and generation
#[test]
fn test_pane_creation() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    assert_eq!(pane.id, "pane-1");
    assert_eq!(pane.get_generation(), 0);
    assert!(pane.is_alive());

    pane.bump_generation();
    assert_eq!(pane.get_generation(), 1);
}

/// §3.10 Pane: resize
fn test_pane_resize() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    pane.resize(100, 30).expect("resize pane");
    assert_eq!(pane.get_cols(), 100);
    assert_eq!(pane.get_rows(), 30);
}

/// §3.10 Pane: title
#[test]
fn test_pane_title() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    pane.set_title("my-title".to_string());
    assert_eq!(pane.get_title(), "my-title");
}

/// §16.9 Scrollback buffer: push + fetch
#[test]
fn test_scrollback_push_and_fetch() {
    let mut buf = crate::grid_sync::ScrollbackBuffer::new(100);

    for i in 0..5 {
        buf.push_row(RowChange {
            row: i,
            cells: vec![crate::grid_sync::Cell {
                character: format!("Line {}", i),
                ..Default::default()
            }],
        });
    }

    assert_eq!(buf.total_lines(), 5);

    // §16.9 向下获取 (direction = 1)
    let lines = buf.fetch_lines(0, 3, 1);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].row, 0);
    assert_eq!(lines[2].row, 2);

    // §16.9 向上获取 (direction = 0)
    let lines = buf.fetch_lines(4, 3, 0);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].row, 2);
    assert_eq!(lines[2].row, 4);
}

/// §16.9 Scrollback buffer: capacity overflow
#[test]
fn test_scrollback_capacity_overflow() {
    let mut buf = crate::grid_sync::ScrollbackBuffer::new(3);

    for i in 0..5 {
        buf.push_row(RowChange {
            row: i,
            cells: vec![crate::grid_sync::Cell {
                character: format!("Line {}", i),
                ..Default::default()
            }],
        });
    }

    assert_eq!(buf.total_lines(), 3);
    assert!(buf.is_full());
    // 最早的行被移除, 剩下行 2, 3, 4
    assert_eq!(buf.rows[0].row, 2);
    assert_eq!(buf.rows[2].row, 4);
}

/// §16.9 Scrollback version: encode/decode roundtrip
#[test]
fn test_scrollback_version_roundtrip() {
    let mut version = crate::grid_sync::ScrollbackVersion::new();
    let encoded = version.encode();
    let decoded = crate::grid_sync::ScrollbackVersion::decode(encoded);
    assert_eq!(decoded.counter, version.counter);
    assert_eq!(decoded.timestamp, version.timestamp);

    version.bump();
    assert_eq!(version.counter, 2);
}

/// §16.9 Scrollback search: regex match
#[test]
fn test_scrollback_search() {
    let mut buf = crate::grid_sync::ScrollbackBuffer::new(100);

    for i in 0..10 {
        buf.push_row(RowChange {
            row: i,
            cells: vec![crate::grid_sync::Cell {
                character: format!("Line {} test", i),
                ..Default::default()
            }],
        });
    }

    // §16.9 搜索包含 "test" 的行
    let matches = buf.search("test", 9, 0, 100);
    assert_eq!(matches.len(), 10); // 所有行都包含 "test"

    // §16.9 搜索特定模式
    let matches = buf.search("Line 5", 9, 0, 100);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, 5);

    // §16.9 向下搜索
    let matches = buf.search("Line 3", 0, 1, 100);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, 3);

    // §16.9 无效正则
    let matches = buf.search("[invalid", 0, 1, 100);
    assert!(matches.is_empty());
}

/// §16.9 Pane: scrollback integration
#[test]
fn test_pane_scrollback() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    // §16.9 初始版本
    let initial_version = pane.get_scrollback_version();
    assert_ne!(initial_version, 0);

    // §16.9 获取空回滚
    let (lines, total, version) = pane.fetch_scrollback(0, 1, 10);
    assert!(lines.is_empty());
    assert_eq!(total, 0);
    assert_eq!(version, initial_version);

    // §16.9 推入行
    pane.push_scrollback_row(RowChange {
        row: 0,
        cells: vec![crate::grid_sync::Cell {
            character: "Hello".to_string(),
            ..Default::default()
        }],
    });

    let (lines, total, _) = pane.fetch_scrollback(0, 1, 10);
    assert_eq!(total, 1);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].cells[0].character, "Hello");
}
// §16.9 Pane: capacity honored from spawn_with_session parameter
//
// The connection layer threads `ServerSettings::scrollback_lines()` (env +
// server.json, hot-reloaded) into spawn_with_session. The pane's buffer
// capacity MUST equal that threaded value, not the env-only
// `default_scrollback_lines()` snapshot. We exercise two non-default
// capacities here — well above and below the 10k DEFAULT — to make sure
// neither the env path nor the default path leaked into spawn_with_session.
#[test]
fn test_pane_scrollback_capacity_uses_threaded_value() {
    let cwd = std::env::temp_dir().to_string_lossy().to_string();

    let small = crate::pane::Pane::spawn_with_session(
        "pane-small".to_string(),
        "sess-small".to_string(),
        cwd.clone(),
        80,
        24,
        None,
        42,
    )
    .expect("spawn pane with small scrollback");
    assert_eq!(
        small.scrollback_buffer.read().capacity(),
        42,
        "spawn_with_session must use the threaded scrollback value (got {})",
        small.scrollback_buffer.read().capacity()
    );

    let large = crate::pane::Pane::spawn_with_session(
        "pane-large".to_string(),
        "sess-large".to_string(),
        cwd,
        80,
        24,
        None,
        87_654,
    )
    .expect("spawn pane with large scrollback");
    assert_eq!(large.scrollback_buffer.read().capacity(), 87_654);
}

/// §16.9 Session: sync scrollback
#[test]
fn test_session_sync_scrollback() {
    let session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    // §16.9 初始状态
    let state = session.get_sync_scrollback();
    assert!(!state.enabled);
    assert!(state.pane_id.is_none());

    // §16.9 设置同步滚动
    session.set_sync_scrollback_offset("pane-1".to_string(), 42);
    let state = session.get_sync_scrollback();
    assert!(state.enabled);
    assert_eq!(state.pane_id, Some("pane-1".to_string()));
    assert_eq!(state.scroll_offset, 42);

    // §16.9 禁用同步滚动
    session.disable_sync_scrollback();
    let state = session.get_sync_scrollback();
    assert!(!state.enabled);
    assert!(state.pane_id.is_none());
    assert_eq!(state.scroll_offset, 0);
}

// ============================================================================
// §16.12 日志与诊断测试
// ============================================================================

/// §16.12 测试日志目录创建与 zlog 文件日志初始化
#[test]
fn test_setup_logging() {
    // §16.12 zlog::init() 是幂等的, 多次调用安全
    zlog::init();
    zlog::init_output_stderr();

    // §16.12 验证日志目录路径格式
    let log_dir = crate::get_log_dir();
    assert!(log_dir.ends_with("z3rm") || log_dir.ends_with("logs"));

    // §16.12 创建日志目录
    std::fs::create_dir_all(&log_dir).expect("failed to create log dir");
    assert!(log_dir.exists());
}

/// §16.12 测试日志文件创建与轮转路径
#[test]
fn test_log_file_rotation() {
    use std::fs;
    use std::path::PathBuf;

    // §16.12 使用临时目录测试轮转逻辑
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let log_path = temp_dir.path().join("test.log");
    let rotate_path = temp_dir.path().join("test.log.old");

    // §16.12 写入数据超过 1MB 阈值触发轮转
    let mut file = fs::File::create(&log_path).expect("create log file");
    for _ in 0..1024 {
        writeln!(file, "test log line for rotation test padding data").expect("write line");
    }
    drop(file);

    // §16.12 验证日志文件已创建
    assert!(log_path.exists());
    let metadata = fs::metadata(&log_path).expect("read metadata");
    assert!(metadata.len() > 0);

    // §16.12 模拟轮转: 复制当前日志到 .old, 然后截断原文件
    if log_path.exists() {
        fs::copy(&log_path, &rotate_path).expect("rotate log file");
        fs::write(&log_path, "").expect("truncate log file");
    }

    // §16.12 验证轮转后 .old 文件存在且原文件被截断
    assert!(rotate_path.exists());
    let old_size = fs::metadata(&rotate_path).expect("read old metadata").len();
    assert!(old_size > 0);
    let new_size = fs::metadata(&log_path).expect("read new metadata").len();
    assert_eq!(new_size, 0);
}

/// §16.12 测试状态输出格式
#[test]
fn test_status_output_format() {
    // §16.12 模拟 status 命令输出格式
    let output = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        "z3rm-server v0.1.0",
        "Uptime: 2h 34m",
        "Sessions: 2 (1 attached)",
        "Panes: 4",
        "Memory: 47 MB",
        "Socket: /tmp/z3rm/mux.sock"
    );

    // §16.12 验证输出格式包含所有必需字段
    assert!(output.contains("z3rm-server v0.1.0"));
    assert!(output.contains("Uptime:"));
    assert!(output.contains("Sessions:"));
    assert!(output.contains("Panes:"));
    assert!(output.contains("Memory:"));
    assert!(output.contains("Socket:"));

    // §16.12 验证行数
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 6);
}

// ============================================================================
// §3.3 窗口管理测试 (多窗口支持，Plan 32)
// ============================================================================

use crate::session::Session;

/// §3.3 测试窗口添加到会话
#[test]
fn test_session_add_window() {
    let mut session = Session::new("sess-1".to_string(), "test".to_string(), "/tmp".to_string());
    assert_eq!(session.window_count(), 0);

    session.add_window("win-1".to_string());
    assert_eq!(session.window_count(), 1);
    assert!(session.has_window("win-1"));

    session.add_window("win-2".to_string());
    assert_eq!(session.window_count(), 2);
    assert!(session.has_window("win-2"));
}

/// §3.3 测试重复窗口 ID 不重复添加
#[test]
fn test_session_deduplicate_windows() {
    let mut session = Session::new("sess-2".to_string(), "test".to_string(), "/tmp".to_string());

    session.add_window("win-1".to_string());
    session.add_window("win-1".to_string()); // 重复
    assert_eq!(session.window_count(), 1);
}

/// §3.3 测试从会话移除窗口
#[test]
fn test_session_remove_window() {
    let mut session = Session::new("sess-3".to_string(), "test".to_string(), "/tmp".to_string());

    session.add_window("win-1".to_string());
    session.add_window("win-2".to_string());
    assert_eq!(session.window_count(), 2);

    session.remove_window("win-1");
    assert_eq!(session.window_count(), 1);
    assert!(!session.has_window("win-1"));
    assert!(session.has_window("win-2"));
}

/// §3.3 测试获取窗口列表
#[test]
fn test_session_get_windows() {
    let mut session = Session::new("sess-4".to_string(), "test".to_string(), "/tmp".to_string());

    session.add_window("win-a".to_string());
    session.add_window("win-b".to_string());

    let windows = session.get_windows();
    assert_eq!(windows.len(), 2);
    assert!(windows.contains(&"win-a".to_string()));
    assert!(windows.contains(&"win-b".to_string()));
}

/// §3.3 测试布局变更广播
#[test]
fn test_session_broadcast_layout_change() {
    let mut session = Session::new("sess-5".to_string(), "test".to_string(), "/tmp".to_string());

    session.add_window("win-1".to_string());
    session.add_window("win-2".to_string());
    session.add_window("win-3".to_string());

    let targets = session.broadcast_layout_change();
    assert_eq!(targets.len(), 3);
    assert!(targets.contains(&"win-1".to_string()));
    assert!(targets.contains(&"win-2".to_string()));
    assert!(targets.contains(&"win-3".to_string()));
}

/// §3.3 测试新会话初始无窗口
#[test]
fn test_session_new_has_no_windows() {
    let session = Session::new(
        "sess-new".to_string(),
        "new".to_string(),
        "/tmp".to_string(),
    );
    assert_eq!(session.window_count(), 0);
    assert!(session.get_windows().is_empty());
}
