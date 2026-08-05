// §3.3 Grid Sync 模块 — generation counter、diff ring、grid snapshot。
// 实现 pull-based grid 同步: 客户端基于 generation 拉取 diff 或全量快照。

use std::collections::VecDeque;

/// 网格行级差异 (§3.3 GridDiff)
#[derive(Clone, Debug, Default)]
pub struct GridDiff {
    /// 变更的行列表
    pub rows: Vec<RowChange>,
}

/// 单行变更 (§3.3 RowChange)
#[derive(Clone, Debug)]
pub struct RowChange {
    /// 行号 (从 0 开始)
    pub row: u32,
    /// 单元格列表
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub character: String,
    pub zerowidth: String,
    pub style: CellStyle,
    pub foreground: u32,
    pub background: u32,
    pub hyperlink: Option<Hyperlink>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hyperlink {
    pub id: String,
    pub uri: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: UnderlineStyle,
    pub underline_color: Option<u32>,
    pub strikethrough: bool,
    pub dim: bool,
    pub reverse: bool,
    pub wide_char: bool,
    pub wide_char_spacer: bool,
    pub leading_wide_char_spacer: bool,
    pub wrapline: bool,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CursorState {
    pub col: u32,
    pub row: u32,
    pub style: CursorShape,
    pub visible: bool,
    pub blinking: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
    HollowBlock,
    Hidden,
}

#[derive(Clone, Debug)]
pub struct FullGridSnapshot {
    pub cols: u32,
    pub rows: u32,
    pub cells: Vec<Cell>,
    pub cursor: CursorState,
    pub alternate_screen: bool,
    pub display_offset: usize,
    pub history_size: usize,
    pub history_version: u64,
    pub modes: u32,
}

/// Grid diff ring (§3.3 默认 64 entries)
pub struct GridDiffRing {
    /// 环形缓冲区
    entries: VecDeque<DiffEntry>,
    /// 容量
    capacity: usize,
}

/// Diff 条目: generation + diff
#[derive(Clone, Debug)]
struct DiffEntry {
    generation: u64,
    diff: GridDiff,
    /// Cursor/mode/offset/dimensions changed and cannot be represented by row diffs.
    requires_full_snapshot: bool,
}

/// Grid 更新结果 (§3.3 FetchGridUpdateResponse)
#[derive(Debug)]
pub enum GridUpdate {
    /// 增量 diff (§3.3 GridDiff)
    Diff {
        from_generation: u64,
        to_generation: u64,
        diff: GridDiff,
    },
    /// 全量快照 (§3.3 FullGridSnapshot)
    FullSnapshot {
        to_generation: u64,
        snapshot: FullGridSnapshot,
    },
    /// 无变化 (since_generation == current)
    NoChange(u64),
}

impl GridDiffRing {
    /// 创建 diff ring (§3.3 默认 64 entries)
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Publish a generation fully represented by its changed rows.
    pub fn push(&mut self, generation: u64, diff: GridDiff) {
        self.push_entry(generation, diff, false);
    }

    /// Publish a generation that also changed state absent from `GridDiff`.
    pub fn push_requiring_full_snapshot(&mut self, generation: u64, diff: GridDiff) {
        self.push_entry(generation, diff, true);
    }

    fn push_entry(&mut self, generation: u64, diff: GridDiff, requires_full_snapshot: bool) {
        self.entries.push_back(DiffEntry {
            generation,
            diff,
            requires_full_snapshot,
        });
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// §3.3 fetch_grid_update: return merged rows only when every generation
    /// since the client checkpoint is row-representable. Otherwise return the
    /// current full snapshot so non-cell render state is never skipped.
    pub fn fetch_update(
        &self,
        since_generation: u64,
        current: u64,
        full_snapshot: impl Fn() -> FullGridSnapshot,
    ) -> GridUpdate {
        if since_generation == 0 {
            return GridUpdate::FullSnapshot {
                to_generation: current,
                snapshot: full_snapshot(),
            };
        }
        if since_generation == current {
            return GridUpdate::NoChange(current);
        }
        if since_generation > current {
            return GridUpdate::FullSnapshot {
                to_generation: current,
                snapshot: full_snapshot(),
            };
        }

        if let Some(oldest) = self.entries.front()
            && since_generation.saturating_add(1) < oldest.generation
        {
            return GridUpdate::FullSnapshot {
                to_generation: current,
                snapshot: full_snapshot(),
            };
        }

        if self
            .entries
            .iter()
            .any(|entry| entry.generation > since_generation && entry.requires_full_snapshot)
        {
            return GridUpdate::FullSnapshot {
                to_generation: current,
                snapshot: full_snapshot(),
            };
        }

        let mut merged_diff = GridDiff::default();
        for entry in &self.entries {
            if entry.generation > since_generation {
                for row_change in &entry.diff.rows {
                    let pos = merged_diff
                        .rows
                        .iter()
                        .position(|row| row.row == row_change.row);
                    if let Some(index) = pos {
                        merged_diff.rows[index].cells = row_change.cells.clone();
                    } else {
                        merged_diff.rows.push(row_change.clone());
                    }
                }
            }
        }

        GridUpdate::Diff {
            from_generation: since_generation,
            to_generation: current,
            diff: merged_diff,
        }
    }

    /// 获取当前条目数
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 构建空快照 (§3.3)
pub fn build_empty_snapshot(cols: u32, rows: u32) -> FullGridSnapshot {
    let cell_count = cols as usize * rows as usize;
    FullGridSnapshot {
        cols,
        rows,
        cells: vec![Cell::default(); cell_count],
        cursor: CursorState {
            col: 0,
            row: 0,
            style: CursorShape::Block,
            visible: true,
            blinking: false,
        },
        alternate_screen: false,
        display_offset: 0,
        history_size: 0,
        history_version: 0,
        modes: mux_protocol::terminal_mode::SHOW_CURSOR,
    }
}

// ============================================================================
// §3.1 server-canonical: alacritty Term → z3rm grid 转换
// ============================================================================
//
use alacritty_terminal::event::{EventListener, VoidListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AlacColor, CursorShape as AlacCursorShape, NamedColor, Rgb,
};

/// §3.1 默认调色板 — 标准 xterm 256 色 + 默认 fg/bg。
///
/// 这个表与 `crates/terminal/src/terminal.rs:get_color_at_index` 在 GPUI 端
/// 的逻辑保持一致 (索引 0..15 + 16..231 立方体 + 232..255 灰阶 + 256..=268
/// 命名色), 但因为服务端没有 Theme, 我们用 xterm 经典默认值, 不读主题。
pub struct Palette;

impl Palette {
    /// 解析 alacritty `Color` 为 0xRRGGBB。
    pub fn resolve(color: AlacColor, colors: &alacritty_terminal::term::color::Colors) -> u32 {
        let rgb = match color {
            AlacColor::Named(named) => Self::named(named, colors),
            AlacColor::Spec(rgb) => rgb,
            AlacColor::Indexed(idx) => Self::indexed(idx, colors),
        };
        ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | (rgb.b as u32)
    }

    fn named(named: NamedColor, colors: &alacritty_terminal::term::color::Colors) -> Rgb {
        // 优先从 term.colors() 读取 (程序 OSC 4 ; may have overridden)
        let idx = named as usize;
        if idx < alacritty_terminal::term::color::COUNT && colors[idx].is_some() {
            return colors[idx].unwrap();
        }
        // 命名色 fallback 到 xterm 默认值 (16 色映射到 0..15)
        match named {
            NamedColor::Black => Rgb {
                r: 0x00,
                g: 0x00,
                b: 0x00,
            },
            NamedColor::Red => Rgb {
                r: 0xcc,
                g: 0x55,
                b: 0x55,
            },
            NamedColor::Green => Rgb {
                r: 0x55,
                g: 0xcc,
                b: 0x55,
            },
            NamedColor::Yellow => Rgb {
                r: 0xcd,
                g: 0xcd,
                b: 0x55,
            },
            NamedColor::Blue => Rgb {
                r: 0x55,
                g: 0x55,
                b: 0xcc,
            },
            NamedColor::Magenta => Rgb {
                r: 0xcc,
                g: 0x55,
                b: 0xcc,
            },
            NamedColor::Cyan => Rgb {
                r: 0x55,
                g: 0xcc,
                b: 0xcc,
            },
            NamedColor::White => Rgb {
                r: 0xdd,
                g: 0xdd,
                b: 0xdd,
            },
            NamedColor::BrightBlack => Rgb {
                r: 0x77,
                g: 0x77,
                b: 0x77,
            },
            NamedColor::BrightRed => Rgb {
                r: 0xff,
                g: 0x77,
                b: 0x77,
            },
            NamedColor::BrightGreen => Rgb {
                r: 0x77,
                g: 0xff,
                b: 0x77,
            },
            NamedColor::BrightYellow => Rgb {
                r: 0xff,
                g: 0xff,
                b: 0x77,
            },
            NamedColor::BrightBlue => Rgb {
                r: 0x77,
                g: 0x77,
                b: 0xff,
            },
            NamedColor::BrightMagenta => Rgb {
                r: 0xff,
                g: 0x77,
                b: 0xff,
            },
            NamedColor::BrightCyan => Rgb {
                r: 0x77,
                g: 0xff,
                b: 0xff,
            },
            NamedColor::BrightWhite => Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
            NamedColor::Foreground => Rgb {
                r: 0xdd,
                g: 0xdd,
                b: 0xdd,
            },
            NamedColor::Background => Rgb {
                r: 0x00,
                g: 0x00,
                b: 0x00,
            },
            NamedColor::Cursor => Rgb {
                r: 0xdd,
                g: 0xdd,
                b: 0xdd,
            },
            NamedColor::DimBlack => Rgb {
                r: 0x55,
                g: 0x55,
                b: 0x55,
            },
            NamedColor::DimRed => Rgb {
                r: 0x88,
                g: 0x44,
                b: 0x44,
            },
            NamedColor::DimGreen => Rgb {
                r: 0x44,
                g: 0x88,
                b: 0x44,
            },
            NamedColor::DimYellow => Rgb {
                r: 0x88,
                g: 0x88,
                b: 0x44,
            },
            NamedColor::DimBlue => Rgb {
                r: 0x44,
                g: 0x44,
                b: 0x88,
            },
            NamedColor::DimMagenta => Rgb {
                r: 0x88,
                g: 0x44,
                b: 0x88,
            },
            NamedColor::DimCyan => Rgb {
                r: 0x44,
                g: 0x88,
                b: 0x88,
            },
            NamedColor::DimWhite => Rgb {
                r: 0x88,
                g: 0x88,
                b: 0x88,
            },
            NamedColor::BrightForeground => Rgb {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
            NamedColor::DimForeground => Rgb {
                r: 0x88,
                g: 0x88,
                b: 0x88,
            },
        }
    }

    fn indexed(idx: u8, colors: &alacritty_terminal::term::color::Colors) -> Rgb {
        // 优先从 term.colors() (OSC 4) 读取
        let idx_usize = idx as usize;
        if idx_usize < alacritty_terminal::term::color::COUNT && colors[idx_usize].is_some() {
            return colors[idx_usize].unwrap();
        }
        // 0..15 与命名色一致
        if idx < 16 {
            let named = match idx {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                _ => NamedColor::BrightWhite,
            };
            return Self::named(named, colors);
        }
        // 16..231 立方体
        if idx < 232 {
            let i = idx - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let cv = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            return Rgb {
                r: cv(r),
                g: cv(g),
                b: cv(b),
            };
        }
        // 232..255 灰阶
        let i = idx - 232;
        let v = i * 10 + 8;
        Rgb { r: v, g: v, b: v }
    }
}

/// §3.3 把 alacritty Term 转成 z3rm FullGridSnapshot。
/// Export the active bottom screen. `display_offset` is carried separately so
/// clients can reconstruct the selected viewport from the same history.
pub fn snapshot_from_term<T: EventListener>(term: &Term<T>) -> FullGridSnapshot {
    let cols = term.columns() as u32;
    let rows = term.screen_lines() as u32;
    let content = term.renderable_content();
    let colors = content.colors;

    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for row in 0..rows as usize {
        for col in 0..cols as usize {
            cells.push(cell_from_alacritty(
                &term.grid()[AlacPoint::new(Line(row as i32), Column(col))],
                colors,
            ));
        }
    }

    let alt = content.mode.contains(TermMode::ALT_SCREEN);
    let cursor = content.cursor;
    let cursor_style = term.cursor_style();

    FullGridSnapshot {
        cols,
        rows,
        cells,
        cursor: CursorState {
            col: cursor.point.column.0 as u32,
            row: cursor.point.line.0.max(0) as u32,
            style: shape_from_alacritty(cursor.shape),
            visible: content.mode.contains(TermMode::SHOW_CURSOR)
                && cursor.shape != AlacCursorShape::Hidden,
            blinking: cursor_style.blinking,
        },
        alternate_screen: alt,
        display_offset: content.display_offset,
        history_size: term.grid().history_size(),
        history_version: 0,
        modes: modes_from_alacritty(content.mode),
    }
}

pub fn fetch_scrollback_from_term<T: EventListener>(
    term: &Term<T>,
    from_line: u32,
    direction: u32,
    count: u32,
) -> (Vec<RowChange>, u32) {
    let history_size = term.grid().history_size();
    let total = u32::try_from(history_size).unwrap_or(u32::MAX);
    if history_size == 0 || count == 0 {
        return (Vec::new(), total);
    }

    let from = from_line as usize;
    if from >= history_size {
        return (Vec::new(), total);
    }
    let count = count as usize;
    let indices = if direction == 0 {
        let start = from.saturating_sub(count.saturating_sub(1));
        start..from.saturating_add(1)
    } else {
        from..from.saturating_add(count).min(history_size)
    };
    let rows = indices
        .map(|index| row_from_history(term, index, history_size))
        .collect();
    (rows, total)
}

pub fn search_scrollback_from_term<T: EventListener>(
    term: &Term<T>,
    regex: &str,
    from_line: u32,
    direction: u32,
    max_results: u32,
) -> Vec<(u32, RowChange)> {
    let Ok(regex) = regex::Regex::new(regex) else {
        return Vec::new();
    };
    let history_size = term.grid().history_size();
    if history_size == 0 || max_results == 0 {
        return Vec::new();
    }

    let from = from_line as usize;
    let indices: Box<dyn Iterator<Item = usize>> = if direction == 0 {
        let start = from.min(history_size.saturating_sub(1));
        Box::new((0..=start).rev())
    } else if from >= history_size {
        Box::new(std::iter::empty())
    } else {
        Box::new(from..history_size)
    };

    indices
        .filter_map(|index| {
            let row = row_from_history(term, index, history_size);
            let text = row
                .cells
                .iter()
                .map(|cell| cell.character.as_str())
                .collect::<String>();
            regex.is_match(&text).then_some((index as u32, row))
        })
        .take(max_results as usize)
        .collect()
}

fn row_from_history<T: EventListener>(
    term: &Term<T>,
    index: usize,
    history_size: usize,
) -> RowChange {
    let line = Line(index as i32 - history_size as i32);
    let colors = term.renderable_content().colors;
    let cells = (0..term.columns())
        .map(|col| cell_from_alacritty(&term.grid()[AlacPoint::new(line, Column(col))], colors))
        .collect();
    RowChange {
        row: index as u32,
        cells,
    }
}

/// §3.3 把 alacritty Term 的 dirty 行转成 GridDiff (row-level, aligned with dirty_lines)。
///
/// `dirty` 是 `(行号, 该行 cells)` 的列表; 调用方负责先 `term.damage()` +
/// `term.reset_damage()` 收集。行号是 active screen 坐标 (0 = 屏幕顶行), 不含
/// `display_offset` —— 和 `snapshot_from_term` 用同一套坐标, 客户端才能把 diff
/// 直接贴到快照上; 滚动位置由快照里的 `display_offset` 单独带走。
pub fn diff_from_dirty<T: EventListener>(term: &Term<T>, dirty_rows: &[usize]) -> GridDiff {
    let content = term.renderable_content();
    let colors = content.colors;
    let cols = term.columns();

    let mut rows = Vec::with_capacity(dirty_rows.len());
    for &row in dirty_rows {
        if row >= term.screen_lines() {
            continue;
        }
        let line = Line(row as i32);
        let mut cells = Vec::with_capacity(cols);
        for col in 0..cols {
            let point = AlacPoint::new(line, Column(col));
            let cell = &term.grid()[point];
            cells.push(cell_from_alacritty(cell, colors));
        }
        rows.push(RowChange {
            row: row as u32,
            cells,
        });
    }
    GridDiff { rows }
}

fn cell_from_alacritty(alac: &AlacCell, colors: &alacritty_terminal::term::color::Colors) -> Cell {
    let underline = if alac.flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if alac.flags.contains(Flags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if alac.flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if alac.flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if alac.flags.contains(Flags::UNDERLINE) {
        UnderlineStyle::Single
    } else {
        UnderlineStyle::None
    };
    Cell {
        character: alac.c.to_string(),
        zerowidth: alac.zerowidth().into_iter().flatten().collect::<String>(),
        style: CellStyle {
            bold: alac.flags.contains(Flags::BOLD),
            italic: alac.flags.contains(Flags::ITALIC),
            underline,
            underline_color: alac
                .underline_color()
                .map(|color| Palette::resolve(color, colors)),
            strikethrough: alac.flags.contains(Flags::STRIKEOUT),
            dim: alac.flags.contains(Flags::DIM),
            reverse: alac.flags.contains(Flags::INVERSE),
            wide_char: alac.flags.contains(Flags::WIDE_CHAR),
            wide_char_spacer: alac.flags.contains(Flags::WIDE_CHAR_SPACER),
            leading_wide_char_spacer: alac.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER),
            wrapline: alac.flags.contains(Flags::WRAPLINE),
            hidden: alac.flags.contains(Flags::HIDDEN),
        },
        foreground: Palette::resolve(alac.fg, colors),
        background: Palette::resolve(alac.bg, colors),
        hyperlink: alac.hyperlink().map(|hyperlink| Hyperlink {
            id: hyperlink.id().to_string(),
            uri: hyperlink.uri().to_string(),
        }),
    }
}

fn shape_from_alacritty(shape: AlacCursorShape) -> CursorShape {
    match shape {
        AlacCursorShape::Block => CursorShape::Block,
        AlacCursorShape::Underline => CursorShape::Underline,
        AlacCursorShape::Beam => CursorShape::Bar,
        AlacCursorShape::HollowBlock => CursorShape::HollowBlock,
        AlacCursorShape::Hidden => CursorShape::Hidden,
    }
}

pub(crate) fn modes_from_alacritty(mode: TermMode) -> u32 {
    let mut modes = 0;
    for (source, target) in [
        (
            TermMode::APP_CURSOR,
            mux_protocol::terminal_mode::APP_CURSOR,
        ),
        (
            TermMode::APP_KEYPAD,
            mux_protocol::terminal_mode::APP_KEYPAD,
        ),
        (
            TermMode::SHOW_CURSOR,
            mux_protocol::terminal_mode::SHOW_CURSOR,
        ),
        (TermMode::LINE_WRAP, mux_protocol::terminal_mode::LINE_WRAP),
        (TermMode::ORIGIN, mux_protocol::terminal_mode::ORIGIN),
        (TermMode::INSERT, mux_protocol::terminal_mode::INSERT),
        (
            TermMode::LINE_FEED_NEW_LINE,
            mux_protocol::terminal_mode::LINE_FEED_NEW_LINE,
        ),
        (
            TermMode::FOCUS_IN_OUT,
            mux_protocol::terminal_mode::FOCUS_IN_OUT,
        ),
        (
            TermMode::ALTERNATE_SCROLL,
            mux_protocol::terminal_mode::ALTERNATE_SCROLL,
        ),
        (
            TermMode::BRACKETED_PASTE,
            mux_protocol::terminal_mode::BRACKETED_PASTE,
        ),
        (TermMode::SGR_MOUSE, mux_protocol::terminal_mode::SGR_MOUSE),
        (
            TermMode::UTF8_MOUSE,
            mux_protocol::terminal_mode::UTF8_MOUSE,
        ),
        (
            TermMode::ALT_SCREEN,
            mux_protocol::terminal_mode::ALT_SCREEN,
        ),
        (
            TermMode::MOUSE_REPORT_CLICK,
            mux_protocol::terminal_mode::MOUSE_REPORT_CLICK,
        ),
        (
            TermMode::MOUSE_DRAG,
            mux_protocol::terminal_mode::MOUSE_DRAG,
        ),
        (
            TermMode::MOUSE_MOTION,
            mux_protocol::terminal_mode::MOUSE_MOTION,
        ),
        (TermMode::VI, mux_protocol::terminal_mode::VI),
    ] {
        if mode.contains(source) {
            modes |= target;
        }
    }
    modes
}

/// §3.3 创建带初始尺寸的真实 alacritty Term (用于 Pane::spawn)。
pub fn new_term(cols: u32, rows: u32) -> Term<VoidListener> {
    let size = TermSize::new(cols as usize, rows as usize);
    let config = TermConfig::default();
    Term::new(config, &size, VoidListener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Scroll;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn term_with_output(cols: u32, rows: u32, bytes: &[u8]) -> Term<VoidListener> {
        let mut term = new_term(cols, rows);
        let mut processor = Processor::<StdSyncHandler>::new();
        processor.advance(&mut term, bytes);
        term
    }

    fn row_text(row: &RowChange) -> String {
        row.cells
            .iter()
            .map(|cell| cell.character.as_str())
            .collect()
    }

    fn snapshot_row_text(snapshot: &FullGridSnapshot, row: u32) -> String {
        let cols = snapshot.cols as usize;
        let start = row as usize * cols;
        snapshot.cells[start..start + cols]
            .iter()
            .map(|cell| cell.character.as_str())
            .collect()
    }

    fn first_cell(term: &Term<VoidListener>, line: i32, column: usize) -> Cell {
        let colors = term.renderable_content().colors;
        cell_from_alacritty(
            &term.grid()[AlacPoint::new(Line(line), Column(column))],
            colors,
        )
    }

    fn dirty_row_numbers(diff: &GridDiff) -> Vec<u32> {
        diff.rows.iter().map(|row| row.row).collect()
    }

    fn match_indices(matches: &[(u32, RowChange)]) -> Vec<u32> {
        matches.iter().map(|(index, _)| *index).collect()
    }

    // ------------------------------------------------------------------
    // diff_from_dirty
    // ------------------------------------------------------------------

    #[test]
    fn diff_from_dirty_emits_only_the_requested_rows_at_full_width() {
        let term = term_with_output(4, 3, b"AA\r\nBB\r\nCC");

        let diff = diff_from_dirty(&term, &[1]);

        assert_eq!(dirty_row_numbers(&diff), vec![1]);
        assert_eq!(
            diff.rows[0].cells.len(),
            4,
            "row diff must carry every column, got {:?}",
            row_text(&diff.rows[0])
        );
        assert_eq!(row_text(&diff.rows[0]), "BB  ");
    }

    #[test]
    fn diff_from_dirty_without_dirty_rows_is_empty_not_a_full_grid() {
        let term = term_with_output(4, 3, b"AA\r\nBB\r\nCC");

        let diff = diff_from_dirty(&term, &[]);

        assert_eq!(
            dirty_row_numbers(&diff),
            Vec::<u32>::new(),
            "an empty dirty set must not degrade into a full-grid diff"
        );
    }

    #[test]
    fn diff_from_dirty_drops_out_of_range_rows_and_keeps_caller_order() {
        let term = term_with_output(4, 3, b"AA\r\nBB\r\nCC");

        let diff = diff_from_dirty(&term, &[2, 3, 99, 0]);

        assert_eq!(dirty_row_numbers(&diff), vec![2, 0]);
        assert_eq!(row_text(&diff.rows[0]), "CC  ");
        assert_eq!(row_text(&diff.rows[1]), "AA  ");
    }

    #[test]
    fn diff_from_dirty_rows_share_the_snapshot_coordinate_space_while_scrolled_back() {
        let mut term = term_with_output(4, 2, b"L0\r\nL1\r\nL2\r\nL3");
        term.scroll_display(Scroll::Delta(2));
        let snapshot = snapshot_from_term(&term);
        assert_eq!(
            snapshot.display_offset, 2,
            "test needs a scrolled-back viewport to be meaningful"
        );

        let diff = diff_from_dirty(&term, &[0, 1]);

        // A client applies row diffs on top of the last full snapshot, so both
        // must number rows identically even when the viewport is scrolled.
        for row_change in &diff.rows {
            assert_eq!(
                row_text(row_change),
                snapshot_row_text(&snapshot, row_change.row),
                "row {} diverges between diff and snapshot",
                row_change.row
            );
        }
        assert_eq!(row_text(&diff.rows[0]), "L2  ");
        assert_eq!(row_text(&diff.rows[1]), "L3  ");
    }

    // ------------------------------------------------------------------
    // row_from_history / fetch_scrollback_from_term
    // ------------------------------------------------------------------

    #[test]
    fn history_index_zero_is_the_oldest_scrollback_line() {
        let term = term_with_output(4, 2, b"A\r\nB\r\nC\r\nD\r\nE");
        let history_size = term.grid().history_size();
        assert_eq!(history_size, 3, "expected A/B/C to have scrolled off");

        let oldest = row_from_history(&term, 0, history_size);
        let middle = row_from_history(&term, 1, history_size);
        let newest = row_from_history(&term, history_size - 1, history_size);

        assert_eq!(row_text(&oldest), "A   ");
        assert_eq!(row_text(&middle), "B   ");
        assert_eq!(row_text(&newest), "C   ");
        assert_eq!(oldest.row, 0);
        assert_eq!(newest.row, 2);
        assert_eq!(oldest.cells.len(), 4);
    }

    #[test]
    fn history_index_maps_to_alacritty_line_index_minus_history_size() {
        let term = term_with_output(4, 2, b"A\r\nB\r\nC\r\nD\r\nE");
        let history_size = term.grid().history_size();

        // Off-by-one here shifts the whole scrollback, so pin both ends
        // directly against the alacritty grid rather than against text.
        assert_eq!(
            row_from_history(&term, 0, history_size).cells[0].character,
            first_cell(&term, -(history_size as i32), 0).character
        );
        assert_eq!(
            row_from_history(&term, history_size - 1, history_size).cells[0].character,
            first_cell(&term, -1, 0).character
        );
    }

    #[test]
    fn history_rows_carry_cell_attributes_not_just_text() {
        let term = term_with_output(4, 2, b"\x1b[1mA\x1b[0m\r\nB\r\nC\r\nD\r\nE");
        let history_size = term.grid().history_size();

        let oldest = row_from_history(&term, 0, history_size);

        assert!(
            oldest.cells[0].style.bold,
            "scrollback line lost its bold attribute: {:?}",
            oldest.cells[0].style
        );
    }

    #[test]
    fn fetch_scrollback_walks_older_and_newer_in_ascending_index_order() {
        let term = term_with_output(4, 2, b"A\r\nB\r\nC\r\nD\r\nE");

        let (newer, total) = fetch_scrollback_from_term(&term, 0, 1, 10);
        assert_eq!(total, 3);
        assert_eq!(
            newer.iter().map(row_text).collect::<Vec<_>>(),
            vec!["A   ", "B   ", "C   "]
        );

        let (older, _) = fetch_scrollback_from_term(&term, 2, 0, 2);
        assert_eq!(
            older.iter().map(row_text).collect::<Vec<_>>(),
            vec!["B   ", "C   "]
        );
    }

    #[test]
    fn fetch_scrollback_returns_total_but_no_rows_for_degenerate_requests() {
        let term = term_with_output(4, 2, b"A\r\nB\r\nC\r\nD\r\nE");

        let (rows, total) = fetch_scrollback_from_term(&term, 0, 1, 0);
        assert!(rows.is_empty(), "count == 0 must yield no rows");
        assert_eq!(total, 3);

        let (rows, total) = fetch_scrollback_from_term(&term, 99, 1, 10);
        assert!(
            rows.is_empty(),
            "out-of-range from_line must yield no rows, got {:?}",
            rows.iter().map(row_text).collect::<Vec<_>>()
        );
        assert_eq!(total, 3);

        let empty = term_with_output(4, 2, b"A");
        let (rows, total) = fetch_scrollback_from_term(&empty, 0, 1, 10);
        assert!(rows.is_empty());
        assert_eq!(total, 0);
    }

    // ------------------------------------------------------------------
    // cell_from_alacritty
    // ------------------------------------------------------------------

    #[test]
    fn sgr_attributes_map_onto_cell_style() {
        let term = term_with_output(4, 1, b"\x1b[1;2;3;4;7;8;9mX");

        let cell = first_cell(&term, 0, 0);

        assert_eq!(cell.character, "X");
        assert!(cell.style.bold, "bold missing from {:?}", cell.style);
        assert!(cell.style.dim, "dim missing from {:?}", cell.style);
        assert!(cell.style.italic, "italic missing from {:?}", cell.style);
        assert!(cell.style.reverse, "reverse missing from {:?}", cell.style);
        assert!(cell.style.hidden, "hidden missing from {:?}", cell.style);
        assert!(
            cell.style.strikethrough,
            "strikethrough missing from {:?}",
            cell.style
        );
        assert_eq!(cell.style.underline, UnderlineStyle::Single);
    }

    #[test]
    fn plain_cell_carries_no_attributes() {
        let term = term_with_output(4, 1, b"X");

        let cell = first_cell(&term, 0, 0);

        assert!(!cell.style.bold, "unexpected style {:?}", cell.style);
        assert!(!cell.style.italic, "unexpected style {:?}", cell.style);
        assert!(!cell.style.reverse, "unexpected style {:?}", cell.style);
        assert!(!cell.style.wrapline, "unexpected style {:?}", cell.style);
        assert_eq!(cell.style.underline, UnderlineStyle::None);
        assert_eq!(cell.zerowidth, "");
        assert_eq!(cell.hyperlink, None);
    }

    #[test]
    fn underline_variants_map_to_distinct_underline_styles() {
        for (sequence, expected) in [
            (b"\x1b[4mX".as_slice(), UnderlineStyle::Single),
            (b"\x1b[4:2mX".as_slice(), UnderlineStyle::Double),
            (b"\x1b[4:3mX".as_slice(), UnderlineStyle::Curly),
            (b"\x1b[4:4mX".as_slice(), UnderlineStyle::Dotted),
            (b"\x1b[4:5mX".as_slice(), UnderlineStyle::Dashed),
            (b"\x1b[4m\x1b[24mX".as_slice(), UnderlineStyle::None),
        ] {
            let term = term_with_output(4, 1, sequence);
            let cell = first_cell(&term, 0, 0);
            assert_eq!(
                cell.style.underline,
                expected,
                "sequence {:?} produced the wrong underline",
                String::from_utf8_lossy(sequence)
            );
        }
    }

    #[test]
    fn underline_color_resolves_through_the_default_palette() {
        let term = term_with_output(4, 1, b"\x1b[4m\x1b[58;5;33mX");

        let cell = first_cell(&term, 0, 0);

        assert_eq!(cell.style.underline, UnderlineStyle::Single);
        assert_eq!(cell.style.underline_color, Some(0x0087ff));
    }

    #[test]
    fn named_indexed_and_rgb_colors_resolve_to_the_default_palette() {
        let default_cell = first_cell(&term_with_output(4, 1, b"X"), 0, 0);
        assert_eq!(default_cell.foreground, 0xdddddd);
        assert_eq!(default_cell.background, 0x000000);

        let named = first_cell(&term_with_output(4, 1, b"\x1b[31;44mX"), 0, 0);
        assert_eq!(named.foreground, 0xcc5555);
        assert_eq!(named.background, 0x5555cc);

        let bright = first_cell(&term_with_output(4, 1, b"\x1b[91mX"), 0, 0);
        assert_eq!(bright.foreground, 0xff7777);

        // 196 sits in the 6x6x6 cube: (5,0,0) -> 0xff0000.
        let indexed = first_cell(&term_with_output(4, 1, b"\x1b[38;5;196mX"), 0, 0);
        assert_eq!(indexed.foreground, 0xff0000);

        // 244 sits in the grayscale ramp: 12 * 10 + 8 = 128.
        let grayscale = first_cell(&term_with_output(4, 1, b"\x1b[38;5;244mX"), 0, 0);
        assert_eq!(grayscale.foreground, 0x808080);

        let truecolor = first_cell(&term_with_output(4, 1, b"\x1b[38;2;18;52;86mX"), 0, 0);
        assert_eq!(truecolor.foreground, 0x123456);
    }

    #[test]
    fn osc_4_palette_override_beats_the_default_palette() {
        let term = term_with_output(4, 1, b"\x1b]4;1;rgb:12/34/56\x1b\\\x1b[31mX");

        let cell = first_cell(&term, 0, 0);

        assert_eq!(
            cell.foreground, 0x123456,
            "OSC 4 override must win over the built-in xterm fallback"
        );
    }

    #[test]
    fn wide_char_occupies_two_cells_with_a_trailing_spacer() {
        let term = term_with_output(4, 1, "漢".as_bytes());

        let wide = first_cell(&term, 0, 0);
        let spacer = first_cell(&term, 0, 1);

        assert_eq!(wide.character, "漢");
        assert!(wide.style.wide_char, "wide flag missing: {:?}", wide.style);
        assert!(!wide.style.wide_char_spacer);
        assert_eq!(spacer.character, " ");
        assert!(
            spacer.style.wide_char_spacer,
            "spacer flag missing: {:?}",
            spacer.style
        );
        assert!(!spacer.style.wide_char);
    }

    #[test]
    fn wide_char_at_the_row_edge_emits_a_leading_spacer_and_wraps() {
        let term = term_with_output(4, 2, "abc漢".as_bytes());

        let leading_spacer = first_cell(&term, 0, 3);
        let wide = first_cell(&term, 1, 0);

        assert!(
            leading_spacer.style.leading_wide_char_spacer,
            "leading spacer flag missing: {:?}",
            leading_spacer.style
        );
        assert_eq!(wide.character, "漢");
        assert!(wide.style.wide_char, "wide flag missing: {:?}", wide.style);
    }

    #[test]
    fn combining_marks_land_in_zerowidth_not_in_character() {
        let term = term_with_output(4, 1, "e\u{0301}\u{0302}".as_bytes());

        let cell = first_cell(&term, 0, 0);

        assert_eq!(cell.character, "e");
        assert_eq!(cell.zerowidth, "\u{0301}\u{0302}");
    }

    #[test]
    fn wrapline_marks_only_the_final_cell_of_a_continued_row() {
        let term = term_with_output(4, 3, b"ABCDE");

        let diff = diff_from_dirty(&term, &[0, 1]);
        let wrapped: Vec<bool> = diff.rows[0]
            .cells
            .iter()
            .map(|cell| cell.style.wrapline)
            .collect();

        // `capture-pane -J` joins on this flag, so it must sit on the last
        // column of the wrapped row and nowhere else.
        assert_eq!(wrapped, vec![false, false, false, true]);
        assert_eq!(row_text(&diff.rows[0]), "ABCD");
        assert_eq!(row_text(&diff.rows[1]), "E   ");
        assert!(
            diff.rows[1].cells.iter().all(|cell| !cell.style.wrapline),
            "continuation row must not be marked as wrapped"
        );
    }

    #[test]
    fn wrapline_survives_the_trip_into_scrollback() {
        let term = term_with_output(4, 2, b"ABCDE\r\nF\r\nG\r\nH");
        let history_size = term.grid().history_size();
        assert!(history_size >= 2, "history_size was {history_size}");

        let wrapped_row = row_from_history(&term, 0, history_size);

        assert_eq!(row_text(&wrapped_row), "ABCD");
        assert!(
            wrapped_row.cells[3].style.wrapline,
            "scrollback lost WRAPLINE: {:?}",
            wrapped_row.cells[3].style
        );
    }

    #[test]
    fn osc_8_hyperlink_is_exported_with_id_and_uri() {
        let term = term_with_output(
            8,
            1,
            b"\x1b]8;id=link1;https://example.com\x1b\\Z\x1b]8;;\x1b\\Y",
        );

        let linked = first_cell(&term, 0, 0);
        let plain = first_cell(&term, 0, 1);

        assert_eq!(
            linked.hyperlink,
            Some(Hyperlink {
                id: "link1".to_string(),
                uri: "https://example.com".to_string(),
            })
        );
        assert_eq!(plain.hyperlink, None);
    }

    // ------------------------------------------------------------------
    // search_scrollback_from_term
    // ------------------------------------------------------------------

    fn term_with_search_history() -> Term<VoidListener> {
        // History ends up as [one, two, three, two]; "five" stays on screen.
        term_with_output(8, 2, b"one\r\ntwo\r\nthree\r\ntwo\r\nfour\r\nfive")
    }

    #[test]
    fn search_direction_zero_walks_toward_older_lines() {
        let term = term_with_search_history();
        assert_eq!(term.grid().history_size(), 4);

        let matches = search_scrollback_from_term(&term, "two", 3, 0, 10);

        assert_eq!(match_indices(&matches), vec![3, 1]);
        assert_eq!(matches[0].1.row, 3);
        assert_eq!(
            matches[0].1.cells.len(),
            8,
            "search hits must carry a full-width row"
        );
        assert_eq!(row_text(&matches[0].1), "two     ");
    }

    #[test]
    fn search_direction_one_walks_toward_newer_lines() {
        let term = term_with_search_history();

        let matches = search_scrollback_from_term(&term, "two", 0, 1, 10);

        assert_eq!(match_indices(&matches), vec![1, 3]);
    }

    #[test]
    fn search_clamps_an_out_of_range_start_when_walking_older() {
        let term = term_with_search_history();

        let matches = search_scrollback_from_term(&term, "two", 999, 0, 10);

        assert_eq!(match_indices(&matches), vec![3, 1]);
    }

    #[test]
    fn search_past_the_newest_history_line_finds_nothing() {
        let term = term_with_search_history();

        let matches = search_scrollback_from_term(&term, "two", 999, 1, 10);

        assert_eq!(match_indices(&matches), Vec::<u32>::new());
    }

    #[test]
    fn search_truncates_at_max_results() {
        let term = term_with_search_history();

        assert_eq!(
            match_indices(&search_scrollback_from_term(&term, "two", 3, 0, 1)),
            vec![3]
        );
        assert_eq!(
            match_indices(&search_scrollback_from_term(&term, "two", 0, 1, 1)),
            vec![1]
        );
    }

    #[test]
    fn search_returns_empty_for_zero_max_results_or_empty_history() {
        let term = term_with_search_history();
        assert_eq!(
            match_indices(&search_scrollback_from_term(&term, "two", 3, 0, 0)),
            Vec::<u32>::new()
        );

        let no_history = term_with_output(8, 2, b"two");
        assert_eq!(no_history.grid().history_size(), 0);
        assert_eq!(
            match_indices(&search_scrollback_from_term(&no_history, "two", 0, 0, 10)),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn search_returns_empty_for_an_invalid_regex() {
        let term = term_with_search_history();

        let matches = search_scrollback_from_term(&term, "two(", 0, 1, 10);

        assert_eq!(match_indices(&matches), Vec::<u32>::new());
    }

    #[test]
    fn search_matches_regex_syntax_not_just_literals() {
        let term = term_with_search_history();

        let matches = search_scrollback_from_term(&term, "^t(wo|hree)", 0, 1, 10);

        assert_eq!(match_indices(&matches), vec![1, 2, 3]);
    }

    #[test]
    fn search_does_not_report_lines_that_are_still_on_screen() {
        let term = term_with_search_history();

        let matches = search_scrollback_from_term(&term, "five", 0, 1, 10);

        // The API is scrollback-only; the client searches the live viewport
        // itself from the snapshot it already holds.
        assert_eq!(match_indices(&matches), Vec::<u32>::new());
    }

    // ------------------------------------------------------------------
    // GridDiffRing
    // ------------------------------------------------------------------

    fn diff_with_row(row: u32, character: &str) -> GridDiff {
        GridDiff {
            rows: vec![RowChange {
                row,
                cells: vec![Cell {
                    character: character.to_string(),
                    ..Default::default()
                }],
            }],
        }
    }

    #[test]
    fn ring_merges_rows_and_keeps_the_newest_content_per_row() {
        let mut ring = GridDiffRing::new(8);
        ring.push(1, diff_with_row(0, "a"));
        ring.push(2, diff_with_row(1, "b"));
        ring.push(3, diff_with_row(0, "c"));

        match ring.fetch_update(1, 3, || build_empty_snapshot(1, 2)) {
            GridUpdate::Diff {
                from_generation,
                to_generation,
                diff,
            } => {
                assert_eq!((from_generation, to_generation), (1, 3));
                assert_eq!(dirty_row_numbers(&diff), vec![1, 0]);
                assert_eq!(row_text(&diff.rows[1]), "c");
            }
            update => panic!("expected merged diff, got {update:?}"),
        }
    }

    #[test]
    fn ring_falls_back_to_a_full_snapshot_when_needed_generations_were_evicted() {
        let mut ring = GridDiffRing::new(2);
        for generation in 1..=4 {
            ring.push(generation, diff_with_row(0, "a"));
        }
        assert_eq!(ring.len(), 2, "ring must have dropped generations 1 and 2");

        // A client at generation 1 still needs generation 2, which is gone.
        match ring.fetch_update(1, 4, || build_empty_snapshot(1, 2)) {
            GridUpdate::FullSnapshot { to_generation, .. } => assert_eq!(to_generation, 4),
            update => panic!("expected full snapshot after eviction, got {update:?}"),
        }

        // A client at generation 2 needs only the retained 3 and 4.
        match ring.fetch_update(2, 4, || build_empty_snapshot(1, 2)) {
            GridUpdate::Diff { diff, .. } => assert_eq!(dirty_row_numbers(&diff), vec![0]),
            update => panic!("expected diff at the retention boundary, got {update:?}"),
        }
    }

    #[test]
    fn ring_falls_back_to_a_full_snapshot_for_non_row_representable_generations() {
        let mut ring = GridDiffRing::new(8);
        ring.push(1, diff_with_row(0, "a"));
        ring.push_requiring_full_snapshot(2, GridDiff::default());

        match ring.fetch_update(1, 2, || build_empty_snapshot(1, 2)) {
            GridUpdate::FullSnapshot { to_generation, .. } => assert_eq!(to_generation, 2),
            update => panic!("expected full snapshot, got {update:?}"),
        }
    }

    #[test]
    fn ring_reports_no_change_only_when_the_client_is_current() {
        let mut ring = GridDiffRing::new(8);
        ring.push(1, diff_with_row(0, "a"));

        match ring.fetch_update(1, 1, || build_empty_snapshot(1, 2)) {
            GridUpdate::NoChange(generation) => assert_eq!(generation, 1),
            update => panic!("expected NoChange, got {update:?}"),
        }
        match ring.fetch_update(0, 1, || build_empty_snapshot(1, 2)) {
            GridUpdate::FullSnapshot { to_generation, .. } => assert_eq!(to_generation, 1),
            update => panic!("expected full snapshot for a fresh client, got {update:?}"),
        }
        // A client ahead of the server (server restart, generation reset) must
        // be resynchronized rather than handed a diff.
        match ring.fetch_update(9, 1, || build_empty_snapshot(1, 2)) {
            GridUpdate::FullSnapshot { to_generation, .. } => assert_eq!(to_generation, 1),
            update => panic!("expected full snapshot for a stale-ahead client, got {update:?}"),
        }
    }

    #[test]
    fn snapshot_from_term_exports_dimensions_cursor_and_history() {
        let term = term_with_output(4, 2, b"A\r\nB\r\nC");

        let snapshot = snapshot_from_term(&term);

        assert_eq!((snapshot.cols, snapshot.rows), (4, 2));
        assert_eq!(snapshot.cells.len(), 8);
        assert_eq!((snapshot.cursor.row, snapshot.cursor.col), (1, 1));
        assert!(snapshot.cursor.visible);
        assert!(!snapshot.alternate_screen);
        assert_eq!(snapshot.history_size, 1);
        assert_eq!(snapshot_row_text(&snapshot, 0), "B   ");
        assert_ne!(snapshot.modes & mux_protocol::terminal_mode::SHOW_CURSOR, 0);
    }

    #[test]
    fn build_empty_snapshot_sizes_the_cell_buffer_to_cols_times_rows() {
        let snapshot = build_empty_snapshot(3, 4);

        assert_eq!(snapshot.cells.len(), 12);
        assert_eq!((snapshot.cols, snapshot.rows), (3, 4));
        assert_eq!(snapshot.history_size, 0);
        assert_ne!(snapshot.modes & mux_protocol::terminal_mode::SHOW_CURSOR, 0);
    }
}
