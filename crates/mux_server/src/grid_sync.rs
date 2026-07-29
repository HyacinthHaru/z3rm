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

// === §16.9 Scrollback Buffer ===

/// 回滚缓冲区 (§16.9) — 存储 alacritty 历史行。
/// 每行保存为 RowChange, 按时间倒序排列 (最新行在末尾)。
#[derive(Clone, Debug)]
pub struct ScrollbackBuffer {
    /// 历史行列表 (从旧到新)
    pub rows: Vec<RowChange>,
    /// 容量上限 (默认 10_000 行)
    capacity: usize,
}

/// 回滚版本 (§16.9) — counter + timestamp 对, 用于缓存失效检测。
/// Counter 在环形缓冲区 wrap 时递增, timestamp 为 Unix 秒。
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollbackVersion {
    /// 环形缓冲区 wrap 计数器
    pub counter: u64,
    /// Unix 时间戳 (秒)
    pub timestamp: u64,
}

impl ScrollbackVersion {
    /// 创建新版本 (§16.9)
    pub fn new() -> Self {
        Self {
            counter: 1,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }

    /// 递增 counter, 更新 timestamp (§16.9 ring wrap)
    pub fn bump(&mut self) {
        self.counter += 1;
        self.timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }

    /// 将版本编码为单个 u64 (counter << 32 | timestamp)
    pub fn encode(&self) -> u64 {
        (self.counter << 32) | (self.timestamp & 0xFFFFFFFF)
    }

    /// 从编码值解码版本
    pub fn decode(encoded: u64) -> Self {
        Self {
            counter: (encoded >> 32) as u64,
            timestamp: (encoded & 0xFFFFFFFF) as u64,
        }
    }
}

impl ScrollbackBuffer {
    /// 创建回滚缓冲区 (§16.9 默认 10_000 行)
    pub fn new(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// §16.11 Hot-reload scrollback capacity from server settings.
    /// Shrinks by dropping the oldest rows first (FIFO).
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        if self.rows.len() > self.capacity {
            let drop_count = self.rows.len() - self.capacity;
            self.rows.drain(0..drop_count);
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 追加一行到缓冲区 (§16.9)
    pub fn push_row(&mut self, row: RowChange) {
        self.rows.push(row);
        // 超出容量时移除最早的历史行
        while self.rows.len() > self.capacity {
            self.rows.remove(0);
        }
    }

    /// 获取总行数 (§16.9)
    pub fn total_lines(&self) -> u32 {
        self.rows.len() as u32
    }

    /// 获取指定范围的行 (§16.9 fetch_scrollback)
    /// from_line: 起始行号 (0 = 最早的历史行)
    /// count: 要获取的行数
    /// direction: 0 = 向上 (from_line 往旧方向), 1 = 向下 (from_line 往新方向)
    ///
    /// 全路径 panic-free: 对任意 u32 输入都不会 panic, 不会整数溢出。
    /// - 缓冲区为空 / `from_line >= total` / `count == 0` → 空 Vec。
    /// - 向上: 返回 `[start, from_line]` 共 count 行 (不足时从 0 开始)。
    /// - 向下: 返回 `[from_line, end)` 共 min(count, 余量) 行。
    pub fn fetch_lines(&self, from_line: u32, count: u32, direction: u32) -> Vec<RowChange> {
        let total = self.rows.len();
        if total == 0 || count == 0 {
            return Vec::new();
        }
        // from_line 越界: 向上/向下都没有可返回的行。
        if from_line as usize >= total {
            return Vec::new();
        }

        let from = from_line as usize;
        let count = count as usize;

        match direction {
            0 => {
                // §16.9 向上: 返回 from_line 及之前共 count 行 (行号减小)。
                // start = from - (count - 1), 下限 0。count >= 1 (上面已排除 0),
                // saturating_sub 保证不溢出、不 panic。
                let start = from.saturating_sub(count - 1);
                self.rows[start..=from].iter().cloned().collect()
            }
            _ => {
                // §16.9 向下: 返回 [from, end)。end = min(from + count, total)。
                // checked_add 防止 from + count 溢出 (虽然当前上限下不会溢出,
                // 但对任意 u32 输入保持鲁棒)。
                let end = from
                    .checked_add(count)
                    .map(|x| std::cmp::min(x, total))
                    .unwrap_or(total);
                self.rows[from..end].iter().cloned().collect()
            }
        }
    }

    /// §16.9 正则搜索回滚缓冲区
    /// 返回匹配行号列表 + 对应的 RowChange
    pub fn search(
        &self,
        regex: &str,
        from_line: u32,
        direction: u32,
        max_results: u32,
    ) -> Vec<(u32, RowChange)> {
        if self.rows.is_empty() {
            return Vec::new();
        }

        // 编译正则表达式 (§16.9)
        let re = match regex::Regex::new(regex) {
            Ok(re) => re,
            Err(_) => return Vec::new(),
        };

        // 构建搜索顺序
        let total = self.rows.len();
        let from = from_line as usize;
        let max = max_results as usize;

        let indices: Vec<usize> = match direction {
            0 => {
                // §16.9 向上搜索: 从 from_line 往 0
                if from >= total {
                    (0..total).rev().collect()
                } else {
                    (0..=from).rev().collect()
                }
            }
            _ => {
                // §16.9 向下搜索: 从 from_line 往末尾
                if from >= total {
                    Vec::new()
                } else {
                    (from..total).collect()
                }
            }
        };

        let mut results = Vec::new();
        for idx in indices {
            if results.len() >= max {
                break;
            }
            // 将行内容拼接为字符串, 用正则匹配 (§16.9)
            let text = self.rows[idx]
                .cells
                .iter()
                .map(|c| c.character.as_str())
                .collect::<String>();
            if re.is_match(&text) {
                results.push((idx as u32, self.rows[idx].clone()));
            }
        }
        results
    }

    /// 检查缓冲区是否已满, 需要 bump version (§16.9 wrap detection)
    pub fn is_full(&self) -> bool {
        self.rows.len() >= self.capacity
    }

    /// 清空缓冲区 (§16.9)
    pub fn clear(&mut self) {
        self.rows.clear();
    }
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
pub fn snapshot_from_term<T: EventListener>(term: &Term<T>) -> FullGridSnapshot {
    let cols = term.columns() as u32;
    let rows = term.screen_lines() as u32;
    let content = term.renderable_content();
    let colors = content.colors;

    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for cell in content.display_iter {
        cells.push(cell_from_alacritty(&cell.cell, colors));
    }
    // 如果 iter 比 cols*rows 短 (理论上不会发生, 防御性补齐)
    while cells.len() < (cols * rows) as usize {
        cells.push(Cell::default());
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
        modes: modes_from_alacritty(content.mode),
    }
}

/// §3.3 把 alacritty Term 的 dirty 行转成 GridDiff (row-level, aligned with dirty_lines)。
///
/// `dirty` 是 `(行号, 该行 cells)` 的列表; 调用方负责先 `term.damage()` +
/// `term.reset_damage()` 收集。viewport 坐标 (0 = 顶部可见行)。
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
