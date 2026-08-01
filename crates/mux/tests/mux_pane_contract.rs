//! # MuxPaneView render contract tests
//!
//! MuxPaneView 是 GPUI view,无法在 headless 测试中实例化 (需要 windowing +
//! editor/workspace test-support 链,后者在迁移期间有编译错误)。
//! 但 §3.3 的核心数据契约 —— fetch_grid_update → FullGridSnapshot / GridDiff
//! → row-major flat cells array → 文本渲染 —— 是纯数据转换,可以单独测试。
//!
//! 这套测试钉死:
//! 1. apply_diff_to_snapshot: row-major 索引正确性、越界保护
//! 2. snapshot_to_text: MuxPaneView::render 的文本输出契约
//! 3. fetch contract: 真 mux_server spawn → 输入 → fetch → 内容匹配
//!    snapshot_to_text 的输出 (证明 GUI 看到的就是 server state)

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::MuxDomain;
use mux_protocol::proto::{
    self, Cell, GridDiff, RowChange, TerminalSize,
    fetch_grid_update_response::Update as FetchUpdate,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use terminal_view::mux_pane::{apply_diff_to_snapshot, snapshot_to_text};

// ============================================================================
// §3.3 纯逻辑单元测试
// ============================================================================

#[test]
fn snapshot_to_text_empty_grid() {
    let snap = proto::FullGridSnapshot {
        cols: 0,
        rows: 0,
        cells: vec![],
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    assert_eq!(snapshot_to_text(&snap), "");
}

#[test]
fn snapshot_to_text_single_row() {
    let snap = proto::FullGridSnapshot {
        cols: 5,
        rows: 1,
        cells: vec![cell('h'), cell('e'), cell('l'), cell('l'), cell('o')],
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    assert_eq!(snapshot_to_text(&snap), "hello");
}

#[test]
fn snapshot_to_text_multi_row_no_trailing_newline() {
    // §3.3 契约:行间 \n,最后一行无 \n
    let snap = proto::FullGridSnapshot {
        cols: 2,
        rows: 3,
        cells: vec![
            cell('a'),
            cell('b'),
            cell('c'),
            cell('d'),
            cell('e'),
            cell('f'),
        ],
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    assert_eq!(snapshot_to_text(&snap), "ab\ncd\nef");
}

#[test]
fn snapshot_to_text_missing_cells_filled_with_space() {
    // §3.3 cells.get(flat).unwrap_or(' ') — 缺失 cell 用空格占位
    let snap = proto::FullGridSnapshot {
        cols: 3,
        rows: 2,
        cells: vec![cell('x')], // 只有 1 cell,期望 6
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    let text = snapshot_to_text(&snap);
    assert_eq!(text, "x  \n   ");
}

#[test]
fn apply_diff_overwrites_complete_row() {
    let mut snap = proto::FullGridSnapshot {
        cols: 3,
        rows: 2,
        cells: vec![cell('a'); 6],
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    let diff = GridDiff {
        rows: vec![RowChange {
            row: 1,
            cells: vec![cell('X'), cell('Y'), cell('Z')],
        }],
    };
    apply_diff_to_snapshot(&mut snap, &diff).expect("complete row diff should apply");
    assert_eq!(snapshot_to_text(&snap), "aaa\nXYZ");
}

#[test]
fn apply_diff_rejects_overlong_row_without_mutation() {
    // Row width is an authoritative protocol invariant; malformed rows are rejected.
    let mut snap = proto::FullGridSnapshot {
        cols: 2,
        rows: 1,
        cells: vec![cell('a'); 2],
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    let diff = GridDiff {
        rows: vec![RowChange {
            row: 0,
            cells: vec![cell('X'), cell('Y'), cell('Z'), cell('W')],
        }],
    };
    assert!(apply_diff_to_snapshot(&mut snap, &diff).is_err());
    assert_eq!(snapshot_to_text(&snap), "aa");
}

#[test]
fn apply_diff_rejects_out_of_bounds_row_without_mutation() {
    // Out-of-bounds rows are malformed and must not partially mutate the cache.
    let mut snap = proto::FullGridSnapshot {
        cols: 2,
        rows: 1,
        cells: vec![cell('a'); 2],
        cursor: None,
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: None,
    };
    let diff = GridDiff {
        rows: vec![RowChange {
            row: 99,
            cells: vec![cell('X'), cell('Y')],
        }],
    };
    assert!(apply_diff_to_snapshot(&mut snap, &diff).is_err());
    assert_eq!(snapshot_to_text(&snap), "aa");
}

fn cell(ch: char) -> Cell {
    Cell {
        char: ch.to_string(),
        style: None,
        foreground: 0,
        background: 0,
        zerowidth: String::new(),
        hyperlink: None,
    }
}

// ============================================================================
// §3.3 真 mux_server fetch contract
// ============================================================================

struct TestServer {
    child: std::process::Child,
    socket_path: PathBuf,
    _tmp: TempDir,
}

impl TestServer {
    fn spawn() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let socket_path = tmp.path().join("mux.sock");
        let db_path = tmp.path().join("mux.db");

        let target_dir = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("../../target/debug");
        let exe = std::env::var("Z3RM_SERVER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| target_dir.join("z3rm-server"));
        anyhow::ensure!(exe.exists(), "z3rm-server not found at {}", exe.display());

        let child = std::process::Command::new(&exe)
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket_path.exists() {
            if Instant::now() >= deadline {
                anyhow::bail!("server didn't bind socket");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));
        Ok(Self {
            child,
            socket_path,
            _tmp: tmp,
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// §3.3 fetch 契约:server 把 cat 输出收集到 grid,客户端 fetch 回来,
/// snapshot_to_text 的输出必须包含 PTY 写入的可见内容。
/// 这是 MuxPaneView::render 看到的字符串的等价物。
#[tokio::test(flavor = "multi_thread")]
async fn fetch_grid_update_then_render_contains_input() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = Arc::new(mux::connect_local(Some(server.socket_path.as_path())).await?);

    let session = domain
        .create_session("render-test", &PathBuf::from("/"))
        .await?;
    let attach = domain.attach(&session, mux::AttachMode::Shared).await?;
    let tab_id = &attach.snapshot.as_ref().context("no snapshot")?.tabs[0].id;

    let pane = domain
        .spawn_pane(
            &session,
            tab_id,
            TerminalSize { cols: 20, rows: 5 },
            Some(proto::ShellCommand {
                program: "/bin/cat".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await?;

    // 给 cat 输入,期望被回显
    domain.send_input(&pane, b"RENDER_MARKER\n").await?;

    // 轮询 fetch_grid_update 直到看到 marker
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rendered = String::new();
    loop {
        let resp = domain.fetch_grid_update(&pane, 0).await?;
        if let Some(update) = resp.update {
            let snap = match update {
                FetchUpdate::FullSnapshot(s) => s,
                FetchUpdate::Diff(_) => continue, // 等 full snapshot 路径
            };
            rendered = snapshot_to_text(&snap);
            if rendered.contains("RENDER_MARKER") {
                break;
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timeout waiting for RENDER_MARKER in fetched grid. Last render:\n{}",
                rendered
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    domain.kill_session(&session).await?;
    Ok(())
}
