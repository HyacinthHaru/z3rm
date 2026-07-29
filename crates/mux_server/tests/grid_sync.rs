//! # Grid Sync 测试
//!
//! §3.3 生成计数器逻辑、diff ring 操作、diff 应用正确性测试。

use mux_server::grid_sync::*;

// ============================================================
// §3.3 生成计数器逻辑
// ============================================================

/// §3.3 构建空快照
#[test]
fn test_build_empty_snapshot() {
    let snap = build_empty_snapshot(80, 24);

    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);
    assert_eq!(snap.cells.len(), 80 * 24);
    assert_eq!(snap.cursor.col, 0);
    assert_eq!(snap.cursor.row, 0);
    assert!(!snap.alternate_screen);
}

/// §3.3 GridDiff 默认值
#[test]
fn test_grid_diff_default() {
    let diff = GridDiff::default();
    assert!(diff.rows.is_empty());
}

/// §3.3 RowChange 构建
#[test]
fn test_row_change() {
    let row = RowChange {
        row: 5,
        cells: vec![
            Cell {
                character: "H".into(),
                style: CellStyle {
                    bold: true,
                    ..Default::default()
                },
                foreground: 0xFFFFFF,
                ..Default::default()
            },
            Cell {
                character: "i".into(),
                ..Default::default()
            },
        ],
    };

    assert_eq!(row.row, 5);
    assert_eq!(row.cells.len(), 2);
    assert!(row.cells[0].style.bold);
}

/// §3.3 CellStyle 所有标志
#[test]
fn test_cell_style_flags() {
    let style = CellStyle {
        bold: true,
        italic: true,
        underline: UnderlineStyle::Single,
        strikethrough: true,
        dim: true,
        reverse: true,
        ..Default::default()
    };

    assert!(style.bold);
    assert!(style.italic);
    assert_eq!(style.underline, UnderlineStyle::Single);
    assert!(style.strikethrough);
    assert!(style.dim);
    assert!(style.reverse);
}

/// §3.3 CursorShape 枚举
#[test]
fn test_cursor_shapes() {
    let _block = CursorShape::Block;
    let _bar = CursorShape::Bar;
    let _underline = CursorShape::Underline;
    // 验证枚举可创建
}

// ============================================================
// §3.3 Diff Ring 操作
// ============================================================

/// §3.3 Diff ring 创建与推送
#[test]
fn test_diff_ring_push() {
    let mut ring = GridDiffRing::new(4);
    assert!(ring.is_empty());
    assert_eq!(ring.len(), 0);
    for generation in 1..=4u64 {
        ring.push(
            generation,
            GridDiff {
                rows: vec![RowChange {
                    row: generation as u32,
                    cells: vec![Cell::default()],
                }],
            },
        );
    }

    assert_eq!(ring.len(), 4);
    assert!(!ring.is_empty());
}

#[test]
fn test_diff_ring_overflow() {
    let mut ring = GridDiffRing::new(3);
    for generation in 1..=5u64 {
        ring.push(
            generation,
            GridDiff {
                rows: vec![RowChange {
                    row: generation as u32,
                    cells: vec![Cell::default()],
                }],
            },
        );
    }

    assert_eq!(ring.len(), 3);
}

/// §3.3 Diff ring 大容量 (64 entries)
#[test]
fn test_diff_ring_capacity_64() {
    let mut ring = GridDiffRing::new(64);

    for generation in 1..=80u64 {
        ring.push(
            generation,
            GridDiff {
                rows: vec![RowChange {
                    row: (generation % 24) as u32,
                    cells: vec![Cell::default()],
                }],
            },
        );
    }

    assert_eq!(ring.len(), 64);
}

// ============================================================
// §3.3 Diff 应用正确性
// ============================================================

/// §3.3 将 GridDiff 应用到 FullGridSnapshot，验证结果匹配预期网格状态
#[test]
fn test_diff_application_correctness() {
    let mut snap = build_empty_snapshot(80, 24);

    let diff = GridDiff {
        rows: vec![RowChange {
            row: 5,
            cells: vec![Cell {
                character: "X".into(),
                style: CellStyle {
                    bold: true,
                    ..Default::default()
                },
                foreground: 0xFF0000,
                ..Default::default()
            }],
        }],
    };

    for row_change in &diff.rows {
        let row_start = row_change.row as usize * snap.cols as usize;
        for (i, cell) in row_change.cells.iter().enumerate() {
            let idx = row_start + i;
            if idx < snap.cells.len() {
                snap.cells[idx].character = cell.character.clone();
                snap.cells[idx].style = cell.style;
                snap.cells[idx].foreground = cell.foreground;
                snap.cells[idx].background = cell.background;
            }
        }
    }

    let modified_idx = 5 * 80 + 0;
    assert_eq!(snap.cells[modified_idx].character, "X");
    assert!(snap.cells[modified_idx].style.bold);
    assert_eq!(snap.cells[modified_idx].foreground, 0xFF0000);
}

/// §3.3 多行 diff 应用
#[test]
fn test_multi_row_diff_application() {
    let mut snap = build_empty_snapshot(10, 5);

    let diff = GridDiff {
        rows: vec![
            RowChange {
                row: 0,
                cells: vec![Cell {
                    character: "A".into(),
                    ..Default::default()
                }],
            },
            RowChange {
                row: 2,
                cells: vec![Cell {
                    character: "B".into(),
                    ..Default::default()
                }],
            },
            RowChange {
                row: 4,
                cells: vec![Cell {
                    character: "C".into(),
                    ..Default::default()
                }],
            },
        ],
    };

    for row_change in &diff.rows {
        let row_start = row_change.row as usize * snap.cols as usize;
        for (i, cell) in row_change.cells.iter().enumerate() {
            let idx = row_start + i;
            if idx < snap.cells.len() {
                snap.cells[idx].character = cell.character.clone();
            }
        }
    }

    assert_eq!(snap.cells[0].character, "A");
    assert_eq!(snap.cells[20].character, "B");
    assert_eq!(snap.cells[40].character, "C");
}

// ============================================================
// §16.9 Scrollback Buffer 测试
// ============================================================

/// §16.9 ScrollbackBuffer 创建与推入
#[test]
fn test_scrollback_buffer_push() {
    let mut buf = ScrollbackBuffer::new(100);

    for i in 0..50u32 {
        buf.push_row(RowChange {
            row: i,
            cells: vec![Cell {
                character: "X".into(),
                ..Default::default()
            }],
        });
    }

    assert_eq!(buf.total_lines(), 50);
    assert!(!buf.is_full());
}

/// §16.9 ScrollbackBuffer 容量溢出
#[test]
fn test_scrollback_buffer_capacity() {
    let mut buf = ScrollbackBuffer::new(5);

    for i in 0..10u32 {
        buf.push_row(RowChange {
            row: i,
            cells: vec![Cell::default()],
        });
    }

    assert_eq!(buf.total_lines(), 5);
    assert!(buf.is_full());
}

#[test]
fn scrollback_fetch_lines_rejects_out_of_range_and_zero_count() {
    let mut buf = ScrollbackBuffer::new(10);
    for row in 0..3u32 {
        buf.push_row(RowChange {
            row,
            cells: vec![Cell::default()],
        });
    }

    assert!(buf.fetch_lines(3, 1, 0).is_empty());
    assert!(buf.fetch_lines(u32::MAX, 1, 0).is_empty());
    assert!(buf.fetch_lines(1, 0, 0).is_empty());
    assert!(buf.fetch_lines(1, 0, 1).is_empty());
}

#[test]
fn scrollback_fetch_lines_handles_extreme_counts_without_panic() {
    let mut buf = ScrollbackBuffer::new(10);
    for row in 0..3u32 {
        buf.push_row(RowChange {
            row,
            cells: vec![Cell::default()],
        });
    }

    let upward = buf.fetch_lines(2, u32::MAX, 0);
    assert_eq!(upward.len(), 3);
    assert_eq!(upward[0].row, 0);
    assert_eq!(upward[2].row, 2);

    let downward = buf.fetch_lines(0, u32::MAX, 1);
    assert_eq!(downward.len(), 3);
    assert_eq!(downward[0].row, 0);
    assert_eq!(downward[2].row, 2);
}

/// §16.9 ScrollbackVersion bump
#[test]
fn test_scrollback_version_bump() {
    let mut ver = ScrollbackVersion::default();
    assert_eq!(ver.counter, 0);

    ver.bump();
    assert_eq!(ver.counter, 1);

    ver.bump();
    assert_eq!(ver.counter, 2);
}

/// §16.9 ScrollbackVersion encode/decode
#[test]
fn test_scrollback_version_round_trip() {
    let mut ver = ScrollbackVersion::new();
    ver.bump();

    let encoded = ver.encode();
    let decoded = ScrollbackVersion::decode(encoded);
    assert_eq!(decoded.counter, ver.counter);
    assert_eq!(decoded.timestamp, ver.timestamp);
}

/// §16.9 ScrollbackVersion 匹配检查 (counter 相同即匹配)
#[test]
fn test_scrollback_version_counter_match() {
    let v1 = ScrollbackVersion {
        counter: 1,
        timestamp: 1000,
    };
    let v2 = ScrollbackVersion {
        counter: 1,
        timestamp: 2000,
    };
    let v3 = ScrollbackVersion {
        counter: 2,
        timestamp: 1000,
    };

    // 相同 counter → 匹配
    assert!(v1.counter == v2.counter, "相同 counter 应匹配");

    // 不同 counter → 不匹配
    assert!(v1.counter != v3.counter, "不同 counter 不应匹配");
}

// ============================================================
// §15.12 FullGridSnapshot.display_offset 服务端捕获
// ============================================================

/// §15.12 `snapshot_from_term` 必须捕获 alacritty grid 的真实 display_offset,
/// 而非固定 0。构造带滚动历史的真实 Term, 滚动后断言快照值与 grid 一致。
#[test]
fn test_snapshot_from_term_captures_display_offset() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::{Dimensions as _, Scroll as AlacScroll};
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use alacritty_terminal::vte::ansi::Processor;

    // 20 列 × 5 行视口, 允许 100 行历史。
    let size = TermSize::new(20, 5);
    let config = TermConfig {
        scrolling_history: 100,
        ..TermConfig::default()
    };
    let mut term: Term<VoidListener> = Term::new(config, &size, VoidListener);

    // 喂 30 行 (远多于 5 行视口) → 产生滚动历史。
    let mut processor = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    let mut bytes = Vec::new();
    for index in 0..30u32 {
        bytes.extend_from_slice(format!("line {}\r\n", index).as_bytes());
    }
    processor.advance(&mut term, &bytes);

    let history = term.history_size();
    assert!(
        history > 0,
        "前置条件: 写入超过视口的行后必须产生滚动历史, got history={}",
        history
    );

    // 滚到顶部 → grid display_offset == history_size (> 0)。
    term.scroll_display(AlacScroll::Top);
    let grid_offset = term.grid().display_offset();
    assert!(
        grid_offset > 0,
        "前置条件: 滚动后 grid display_offset 必须非零"
    );

    let snapshot = snapshot_from_term(&term);
    assert_eq!(
        snapshot.display_offset, grid_offset,
        "snapshot_from_term 必须携带服务端 grid 的 display_offset"
    );
    assert!(
        snapshot.display_offset > 0,
        "捕获的 display_offset 必须是非零滚动位置"
    );
}

#[test]
fn cell_only_generations_merge_without_full_snapshot() {
    let mut ring = GridDiffRing::new(4);
    ring.push(
        2,
        GridDiff {
            rows: vec![RowChange {
                row: 0,
                cells: vec![Cell {
                    character: "X".into(),
                    ..Default::default()
                }],
            }],
        },
    );

    let update = ring.fetch_update(1, 2, || build_empty_snapshot(1, 1));
    match update {
        GridUpdate::Diff {
            from_generation,
            to_generation,
            diff,
        } => {
            assert_eq!(from_generation, 1);
            assert_eq!(to_generation, 2);
            assert_eq!(diff.rows.len(), 1);
            assert_eq!(diff.rows[0].cells[0].character, "X");
        }
        other => panic!("expected merged row diff, got {other:?}"),
    }
}

#[test]
fn state_generation_forces_full_snapshot() {
    let mut ring = GridDiffRing::new(4);
    ring.push_requiring_full_snapshot(2, GridDiff::default());

    let update = ring.fetch_update(1, 2, || build_empty_snapshot(7, 3));
    match update {
        GridUpdate::FullSnapshot {
            to_generation,
            snapshot,
        } => {
            assert_eq!(to_generation, 2);
            assert_eq!((snapshot.cols, snapshot.rows), (7, 3));
        }
        other => panic!("state-only generation must force a snapshot, got {other:?}"),
    }
}

#[test]
fn ring_boundary_uses_diff_when_next_generation_is_present() {
    let mut ring = GridDiffRing::new(2);
    ring.push(2, GridDiff::default());
    ring.push(3, GridDiff::default());
    assert!(matches!(
        ring.fetch_update(1, 3, || build_empty_snapshot(1, 1)),
        GridUpdate::Diff { .. }
    ));

    ring.push(4, GridDiff::default());
    assert!(matches!(
        ring.fetch_update(1, 4, || build_empty_snapshot(1, 1)),
        GridUpdate::FullSnapshot { .. }
    ));
}

#[test]
fn future_checkpoint_forces_authoritative_full_snapshot() {
    let ring = GridDiffRing::new(4);
    assert!(matches!(
        ring.fetch_update(99, 3, || build_empty_snapshot(2, 1)),
        GridUpdate::FullSnapshot {
            to_generation: 3,
            ..
        }
    ));
}

#[test]
fn inverse_cell_preserves_source_colors_and_reverse_flag() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use alacritty_terminal::vte::ansi::Processor;

    let size = TermSize::new(2, 1);
    let mut term: Term<VoidListener> = Term::new(TermConfig::default(), &size, VoidListener);
    let mut processor = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    processor.advance(&mut term, b"\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m\x1b[7mX");

    let snapshot = snapshot_from_term(&term);
    let cell = snapshot
        .cells
        .iter()
        .find(|cell| cell.character == "X")
        .unwrap_or_else(|| panic!("inverse cell missing from snapshot"));
    assert_eq!(cell.foreground, 0x010203);
    assert_eq!(cell.background, 0x040506);
    assert!(cell.style.reverse);
}

#[test]
fn rich_cell_snapshot_preserves_vte_semantics() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use alacritty_terminal::vte::ansi::Processor;

    let size = TermSize::new(4, 1);
    let mut term: Term<VoidListener> = Term::new(TermConfig::default(), &size, VoidListener);
    let mut processor = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
    processor.advance(
        &mut term,
        b"\x1b]8;id=rich-link;https://example.com\x1b\\\x1b[4:3m\x1b[58;2;7;8;9mA\xcc\x81\xe7\x95\x8c\x1b]8;;\x1b\\",
    );

    let snapshot = snapshot_from_term(&term);
    let cell = snapshot
        .cells
        .iter()
        .find(|cell| cell.character == "A")
        .unwrap_or_else(|| panic!("rich cell missing from snapshot"));
    assert_eq!(cell.zerowidth, "\u{301}");
    assert_eq!(cell.style.underline, UnderlineStyle::Curly);
    assert_eq!(cell.style.underline_color, Some(0x070809));
    let hyperlink = cell
        .hyperlink
        .as_ref()
        .unwrap_or_else(|| panic!("rich cell hyperlink missing"));
    assert_eq!(hyperlink.uri, "https://example.com");

    let wide = snapshot
        .cells
        .iter()
        .find(|cell| cell.character == "界")
        .unwrap_or_else(|| panic!("wide cell missing from snapshot"));
    assert!(wide.style.wide_char);
    assert!(
        snapshot
            .cells
            .iter()
            .any(|cell| cell.style.wide_char_spacer)
    );
}
