mod mappings;

mod alacritty;
pub mod kitty_graphics;
mod pty_info;
pub mod terminal_settings;

#[cfg(not(windows))]
use anyhow::Context as _;
use anyhow::{Result, bail};
use futures_lite::future::yield_now;
use log::trace;

use futures::{
    FutureExt,
    channel::mpsc::{UnboundedReceiver, unbounded},
};

use itertools::Itertools as _;
use mappings::mouse::{
    alt_scroll, grid_point, grid_point_and_side, mouse_button_report, mouse_moved_report,
    scroll_report,
};

use async_channel::{Receiver, Sender};
use collections::{HashMap, VecDeque};
use futures::StreamExt;
use pty_info::{ProcessIdGetter, PtyProcessInfo};
use serde::{Deserialize, Serialize};
use settings::Settings;
use terminal_settings::{AlternateScroll, CursorShape as SettingsCursorShape, TerminalSettings};
use theme::{ActiveTheme, Theme};
use urlencoding;
use util::shell::{Shell, ShellKind};
use util::{ResultExt as _, paths::PathStyle, truncate_and_trailoff};

/// 终端任务隐藏策略 (stub: replaced deleted task::HideStrategy)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum HideStrategy {
    #[default]
    Never,
    Always,
    OnSuccess,
}

/// 终端中要启动的任务 (stub: replaced deleted task::SpawnInTerminal)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpawnInTerminal {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub label: String,
    pub full_label: String,
    pub command_label: String,
    pub hide: HideStrategy,
    pub show_summary: bool,
    pub show_command: bool,
    pub id: u64,
    pub show_rerun: bool,
}

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::{
    borrow::Cow,
    cmp::{self, min},
    fmt::{self, Display, Formatter},
    ops::{BitOr, BitOrAssign, Deref, Range as StdRange},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use vte::ansi::{Attr, Handler, Processor, StdSyncHandler};
pub use vte::ansi::{Color, NamedColor, Rgb};

use gpui::{
    App, AppContext as _, BackgroundExecutor, Bounds, ClipboardItem, Context, EventEmitter, Hsla,
    Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point as GpuiPoint, Rgba, ScrollWheelEvent, Size, Task, TouchPhase, Window, actions, black, px,
};

#[cfg(not(windows))]
use crate::alacritty::current_child_signal_mask;
use crate::alacritty::{
    AlacrittyCell, AlacrittyGridIterator, AlacrittyHyperlink, AlacrittySearch, AlacrittyTerm,
    AlacrittyTermConfig, AlacrittyTermLock, HyperlinkMatch, PtySender, RegexSearches,
    append_text_to_term, apply_config, apply_structured_snapshot, clear_saved_screen, content_text,
    cursor_anchor, display_offset, display_only_term_config, find_from_terminal_point,
    full_content_range, last_non_empty_lines, make_content, new_term, open_pty, pty_options,
    pty_term_config, resize, screen_lines, scroll_display, scroll_to_point, search_matches,
    selection_text, set_default_cursor_style, set_selection as set_term_selection, shrink_to_used,
    spawn_event_loop, toggle_vi_mode as toggle_term_vi_mode, total_lines,
    update_selection as update_term_selection, update_selection_to_vi_cursor,
    update_vi_cursor_for_scroll, vi_goto_point, vi_motion,
};
use crate::mappings::colors::to_vte_rgb;
use crate::mappings::keys::to_esc_str;

/// Process-wide flag set by headless hosts (e.g. the eval CLI) that have no
/// controlling TTY. In such sandboxes PTY allocation and acquiring a
/// controlling terminal fail with `ENOTTY`, so when this is set terminals run
/// their command as a plain subprocess with piped output instead of through a
/// PTY. The normal editor leaves it unset to preserve the interactive PTY
/// experience.
#[derive(Clone, Copy, Default)]
pub struct HeadlessTerminal(pub bool);

impl gpui::Global for HeadlessTerminal {}

impl HeadlessTerminal {
    pub fn is_enabled(cx: &App) -> bool {
        cx.try_global::<Self>().is_some_and(|headless| headless.0)
    }
}

#[derive(Clone, Copy, Debug)]
enum Scroll {
    Delta(i32),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug)]
enum ViMotion {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
    FirstOccupied,
    High,
    Middle,
    Low,
    WordLeft,
    WordRight,
    WordRightEnd,
    Bracket,
    ParagraphUp,
    ParagraphDown,
    LineSelect,
    SearchStart,
    SearchNext,
    SearchPrev,
}

#[derive(Clone, Debug)]
pub struct Search {
    search: AlacrittySearch,
}

/// §12 Plan 31 — the confirmed copy-mode search. The match list itself lives in
/// [`Terminal::matches`] so search hits highlight through the same renderer path
/// as search-bar hits.
#[derive(Clone, Debug)]
struct SearchState {
    query: String,
    searcher: Search,
}

#[derive(Clone, Debug)]
struct Selection {
    ty: SelectionType,
    start: SelectionAnchor,
    end: SelectionAnchor,
    head: Point,
}

#[derive(Clone, Copy, Debug)]
struct SelectionAnchor {
    point: Point,
    side: SelectionSide,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectionSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionType {
    Simple,
    Semantic,
    Lines,
}

impl Selection {
    fn new(selection_type: SelectionType, point: Point, side: SelectionSide) -> Self {
        let anchor = SelectionAnchor { point, side };
        Self {
            ty: selection_type,
            start: anchor,
            end: anchor,
            head: point,
        }
    }

    fn simple_range(range: Range) -> Self {
        let mut selection = Self::new(SelectionType::Simple, range.start(), SelectionSide::Left);
        selection.update(range.end(), SelectionSide::Right);
        selection
    }

    fn update(&mut self, point: Point, side: SelectionSide) {
        self.end = SelectionAnchor { point, side };
        self.head = point;
    }
}

pub fn is_default_background_color(color: Color) -> bool {
    matches!(color, Color::Named(NamedColor::Background))
}

pub fn is_app_chosen_exact_color(color: Color) -> bool {
    matches!(color, Color::Spec(_) | Color::Indexed(16..=255))
}

pub type AnsiSpans = Vec<(StdRange<usize>, Option<Color>)>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedAnsiText {
    pub text: String,
    pub foreground_spans: AnsiSpans,
    pub background_spans: AnsiSpans,
}

pub fn parse_ansi_text(input: &[u8]) -> ParsedAnsiText {
    let mut handler = StyledAnsiTextHandler::default();
    let mut processor = Processor::<StdSyncHandler>::default();
    processor.advance(&mut handler, input);
    handler.finish()
}

pub fn strip_ansi_text(input: &[u8]) -> String {
    let mut handler = PlainAnsiTextHandler::default();
    let mut processor = Processor::<StdSyncHandler>::default();
    processor.advance(&mut handler, input);
    handler.text
}

#[derive(Default)]
struct StyledAnsiTextHandler {
    text: String,
    foreground_spans: AnsiSpans,
    background_spans: AnsiSpans,
    current_foreground_range_start: usize,
    current_background_range_start: usize,
    current_foreground_color: Option<Color>,
    current_background_color: Option<Color>,
}

impl StyledAnsiTextHandler {
    fn finish(mut self) -> ParsedAnsiText {
        if self.current_foreground_range_start < self.text.len() {
            self.foreground_spans.push((
                self.current_foreground_range_start..self.text.len(),
                self.current_foreground_color,
            ));
        }

        if self.current_background_range_start < self.text.len() {
            self.background_spans.push((
                self.current_background_range_start..self.text.len(),
                self.current_background_color,
            ));
        }

        ParsedAnsiText {
            text: self.text,
            foreground_spans: self.foreground_spans,
            background_spans: self.background_spans,
        }
    }

    fn break_foreground_span(&mut self, color: Option<Color>) {
        self.foreground_spans.push((
            self.current_foreground_range_start..self.text.len(),
            self.current_foreground_color,
        ));
        self.current_foreground_color = color;
        self.current_foreground_range_start = self.text.len();
    }

    fn break_background_span(&mut self, color: Option<Color>) {
        self.background_spans.push((
            self.current_background_range_start..self.text.len(),
            self.current_background_color,
        ));
        self.current_background_color = color;
        self.current_background_range_start = self.text.len();
    }
}

impl Handler for StyledAnsiTextHandler {
    fn input(&mut self, c: char) {
        self.text.push(c);
    }

    fn linefeed(&mut self) {
        self.text.push('\n');
    }

    fn put_tab(&mut self, count: u16) {
        self.text.extend(std::iter::repeat_n('\t', count as usize));
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        match attr {
            Attr::Foreground(color) => {
                self.break_foreground_span(Some(color));
            }
            Attr::Background(color) => {
                self.break_background_span(Some(color));
            }
            Attr::Reset => {
                self.break_foreground_span(None);
                self.break_background_span(None);
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct PlainAnsiTextHandler {
    text: String,
    line_start: usize,
}

impl Handler for PlainAnsiTextHandler {
    fn input(&mut self, c: char) {
        self.text.push(c);
    }

    fn linefeed(&mut self) {
        self.text.push('\n');
        self.line_start = self.text.len();
    }

    fn carriage_return(&mut self) {
        self.text.truncate(self.line_start);
    }

    fn put_tab(&mut self, count: u16) {
        self.text.extend(std::iter::repeat_n('\t', count as usize));
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Hyperlink {
    data: HyperlinkData,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum HyperlinkData {
    Alacritty(AlacrittyHyperlink),
    Owned { id: Option<Arc<str>>, uri: Arc<str> },
}

#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct Cell {
    cell: AlacrittyCell,
}

/// A fully materialized terminal cell supplied by an authoritative external
/// emulator. This deliberately lives in `terminal`, rather than depending on
/// a transport crate, so display-only terminals can import structured state
/// without reparsing ANSI text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredTerminalCell {
    pub character: char,
    pub zerowidth: Vec<char>,
    pub foreground: Rgb,
    pub background: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: StructuredUnderlineStyle,
    pub underline_color: Option<Rgb>,
    pub strikethrough: bool,
    pub dim: bool,
    pub reverse: bool,
    pub wide_char: bool,
    pub wide_char_spacer: bool,
    pub leading_wide_char_spacer: bool,
    pub wrapline: bool,
    pub hidden: bool,
    pub hyperlink: Option<Hyperlink>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StructuredUnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Default for StructuredTerminalCell {
    fn default() -> Self {
        Self {
            character: ' ',
            zerowidth: Vec::new(),
            foreground: Rgb {
                r: 0xdd,
                g: 0xdd,
                b: 0xdd,
            },
            background: Rgb { r: 0, g: 0, b: 0 },
            bold: false,
            italic: false,
            underline: StructuredUnderlineStyle::None,
            underline_color: None,
            strikethrough: false,
            dim: false,
            reverse: false,
            wide_char: false,
            wide_char_spacer: false,
            leading_wide_char_spacer: false,
            wrapline: false,
            hidden: false,
            hyperlink: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StructuredTerminalCursor {
    pub point: Point,
    pub shape: CursorShape,
    pub visible: bool,
    pub blinking: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredTerminalSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<StructuredTerminalCell>,
    /// Flat row-major history cells, oldest row first.
    pub history: Vec<StructuredTerminalCell>,
    /// Number of history rows above the active screen selected for display.
    pub display_offset: usize,
    pub cursor: Option<StructuredTerminalCursor>,
    pub alternate_screen: bool,
    pub modes: Modes,
}

pub struct RenderableCells<'a> {
    cells: AlacrittyGridIterator<'a>,
}

#[derive(Debug, Clone)]
pub struct IndexedCell {
    pub point: Point,
    pub cell: Cell,
}

impl Deref for IndexedCell {
    type Target = Cell;

    #[inline]
    fn deref(&self) -> &Cell {
        &self.cell
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modes(u32);

impl Modes {
    pub const NONE: Self = Self(0);
    pub const APP_CURSOR: Self = Self(1 << 0);
    pub const APP_KEYPAD: Self = Self(1 << 1);
    pub const SHOW_CURSOR: Self = Self(1 << 2);
    pub const LINE_WRAP: Self = Self(1 << 3);
    pub const ORIGIN: Self = Self(1 << 4);
    pub const INSERT: Self = Self(1 << 5);
    pub const LINE_FEED_NEW_LINE: Self = Self(1 << 6);
    pub const FOCUS_IN_OUT: Self = Self(1 << 7);
    pub const ALTERNATE_SCROLL: Self = Self(1 << 8);
    pub const BRACKETED_PASTE: Self = Self(1 << 9);
    pub const SGR_MOUSE: Self = Self(1 << 10);
    pub const UTF8_MOUSE: Self = Self(1 << 11);
    pub const ALT_SCREEN: Self = Self(1 << 12);
    pub const MOUSE_REPORT_CLICK: Self = Self(1 << 13);
    pub const MOUSE_DRAG: Self = Self(1 << 14);
    pub const MOUSE_MOTION: Self = Self(1 << 15);
    pub const VI: Self = Self(1 << 16);
    pub const MOUSE_MODE: Self =
        Self(Self::MOUSE_REPORT_CLICK.0 | Self::MOUSE_DRAG.0 | Self::MOUSE_MOTION.0);

    pub const fn empty() -> Self {
        Self::NONE
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & ((1 << 17) - 1))
    }
}

impl BitOr for Modes {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.insert(rhs);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub shape: CursorShape,
    pub point: Point,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
    HollowBlock,
    Hidden,
}

impl From<SettingsCursorShape> for CursorShape {
    fn from(shape: SettingsCursorShape) -> Self {
        match shape {
            SettingsCursorShape::Block => Self::Block,
            SettingsCursorShape::Underline => Self::Underline,
            SettingsCursorShape::Bar => Self::Bar,
            SettingsCursorShape::Hollow => Self::HollowBlock,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Point {
    pub line: i32,
    pub column: usize,
}

impl Point {
    pub fn new(line: i32, column: usize) -> Self {
        Self { line, column }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Range {
    start: Point,
    end: Point,
}

impl Range {
    pub fn new(start: Point, end: Point) -> Self {
        Self { start, end }
    }

    pub fn start(&self) -> Point {
        self.start
    }

    pub fn end(&self) -> Point {
        self.end
    }

    pub fn contains(&self, point: Point) -> bool {
        self.start <= point && point <= self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub start: Point,
    pub end: Point,
    pub is_block: bool,
}

impl SelectionRange {
    pub fn point_range(self) -> Range {
        Range::new(self.start, self.end)
    }
}

// TODO: Un-pub
#[derive(Clone)]
pub struct Content {
    pub cells: Vec<IndexedCell>,
    pub mode: Modes,
    pub display_offset: usize,
    pub selection_text: Option<String>,
    pub selection: Option<SelectionRange>,
    pub cursor: Cursor,
    pub cursor_char: char,
    pub terminal_bounds: TerminalBounds,
    pub last_hovered_word: Option<HoveredWord>,
    pub scrolled_to_top: bool,
    pub scrolled_to_bottom: bool,
    pub bottom_row_occupied: bool,
    /// Kitty graphics / OSC 1337 图像叠加层, 已经投影到当前视口。
    pub images: Vec<VisibleImage>,
}

/// 一次图像放置。
///
/// 锚点用滚动缓冲区的绝对行号而不是视口行号, 这样内容滚动时图像跟着一起走。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImagePlacement {
    pub id: kitty_graphics::ImageId,
    /// 从当前滚动缓冲区最老一行算起的行号。
    pub anchor_line: i64,
    pub column: usize,
    pub columns: usize,
    pub rows: usize,
    pub z_index: i32,
}

/// [`ImagePlacement`] 投影到当前视口后的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleImage {
    pub id: kitty_graphics::ImageId,
    /// 相对视口顶端的行号。图像从视口上方开始时为负。
    pub row: i32,
    pub column: usize,
    pub columns: usize,
    pub rows: usize,
    pub z_index: i32,
}

/// 单个 pane 同时保留的放置数量上限。
const MAX_IMAGE_PLACEMENTS: usize = 256;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HoveredWord {
    pub word: String,
    pub word_match: Range,
    pub id: usize,
}

impl Default for Content {
    fn default() -> Self {
        Content {
            cells: Default::default(),
            mode: Default::default(),
            display_offset: Default::default(),
            selection_text: Default::default(),
            selection: Default::default(),
            cursor: Cursor {
                shape: CursorShape::Block,
                point: Point::new(0, 0),
            },
            cursor_char: Default::default(),
            terminal_bounds: Default::default(),
            last_hovered_word: None,
            scrolled_to_top: false,
            scrolled_to_bottom: false,
            bottom_row_occupied: false,
            images: Vec::new(),
        }
    }
}

#[derive(PartialEq, Eq)]
enum SelectionPhase {
    Selecting,
    Ended,
}

#[cfg(test)]
mod domain_tests {
    use super::*;

    #[test]
    fn strip_ansi_text_removes_ansi_and_handles_carriage_returns() {
        let cases = [
            ("no escape codes here\n", "no escape codes here\n"),
            ("\x1b[31mhello\x1b[0m", "hello"),
            ("\x1b[1;32mfoo\x1b[0m bar", "foo bar"),
            ("progress 10%\rprogress 100%\n", "progress 100%\n"),
        ];

        for (input, expected) in cases {
            assert_eq!(strip_ansi_text(input.as_bytes()), expected);
        }
    }

    #[test]
    fn parse_ansi_text_records_foreground_and_background_spans() {
        let parsed = parse_ansi_text(b"\x1b[31mred\x1b[44mblue-bg\x1b[0mplain");

        assert_eq!(parsed.text, "redblue-bgplain");
        assert_eq!(
            parsed.foreground_spans,
            vec![
                (0..0, None),
                (0..10, Some(Color::Named(NamedColor::Red))),
                (10..15, None),
            ]
        );
        assert_eq!(
            parsed.background_spans,
            vec![
                (0..3, None),
                (3..10, Some(Color::Named(NamedColor::Blue))),
                (10..15, None),
            ]
        );
    }

    #[test]
    fn terminal_cell_clone_shares_extra_storage() {
        let mut cell = Cell::default();
        cell.push_zerowidth('a');

        let clone = cell.clone();

        match (&cell.cell.extra, &clone.cell.extra) {
            (Some(extra), Some(clone_extra)) => assert!(Arc::ptr_eq(extra, clone_extra)),
            _ => panic!("expected extra storage on both cells"),
        }
    }
}

actions!(
    terminal,
    [
        /// Clears the terminal screen.
        Clear,
        /// Copies selected text to the clipboard.
        Copy,
        /// Pastes from the clipboard.
        Paste,
        /// Pastes the text from the clipboard.
        PasteText,
        /// Shows the character palette for special characters.
        ShowCharacterPalette,
        /// Searches for text in the terminal.
        SearchTest,
        /// Scrolls up by one line.
        ScrollLineUp,
        /// Scrolls down by one line.
        ScrollLineDown,
        /// Scrolls up by one page.
        ScrollPageUp,
        /// Scrolls down by one page.
        ScrollPageDown,
        /// Scrolls up by half a page.
        ScrollHalfPageUp,
        /// Scrolls down by half a page.
        ScrollHalfPageDown,
        /// Scrolls to the top of the terminal buffer.
        ScrollToTop,
        /// Scrolls to the bottom of the terminal buffer.
        ScrollToBottom,
        /// Toggles vi mode in the terminal.
        ToggleViMode,
        /// Selects all text in the terminal.
        SelectAll,
    ]
);

const DEBUG_TERMINAL_WIDTH: Pixels = px(500.);
const DEBUG_TERMINAL_HEIGHT: Pixels = px(30.);
const DEBUG_CELL_WIDTH: Pixels = px(5.);
const DEBUG_LINE_HEIGHT: Pixels = px(5.);

/// Inserts Zed-specific environment variables for terminal sessions.
/// Used by both local terminals and remote terminals (via SSH).
pub fn insert_zed_terminal_env(
    env: &mut HashMap<String, String>,
    version: &impl std::fmt::Display,
) {
    env.insert("Z3RM_TERM".to_string(), "true".to_string());
    env.insert("TERM_PROGRAM".to_string(), "zed".to_string());
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());
    env.insert("TERM_PROGRAM_VERSION".to_string(), version.to_string());
}

///Upward flowing events, for changing the title and such
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    TitleChanged,
    BreadcrumbsChanged,
    CloseTerminal,
    Bell,
    Wakeup,
    BlinkChanged(bool),
    SelectionsChanged,
    NewNavigationTarget(Option<MaybeNavigationTarget>),
    Open(MaybeNavigationTarget),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLikeTarget {
    /// File system path, absolute or relative, existing or not.
    /// Might have line and column number(s) attached as `file.rs:1:23`
    pub maybe_path: String,
    /// Current working directory of the terminal
    pub terminal_dir: Option<PathBuf>,
}

/// A string inside terminal, potentially useful as a URI that can be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaybeNavigationTarget {
    /// HTTP, git, etc. string determined by the `URL_REGEX` regex.
    Url(String),
    /// File system path, absolute or relative, existing or not.
    /// Might have line and column number(s) attached as `file.rs:1:23`
    PathLike(PathLikeTarget),
}

#[derive(Clone)]
enum InternalEvent {
    Resize(TerminalBounds),
    Clear,
    // FocusNextMatch,
    Scroll(Scroll),
    // §15.12 absolute display-offset restore (reconnect recovery)
    ScrollToDisplayOffset(usize),
    ScrollToPoint(Point),
    SetSelection(Option<Selection>),
    UpdateSelection(GpuiPoint<Pixels>),
    FindHyperlink(GpuiPoint<Pixels>, bool),
    ProcessHyperlink(HyperlinkMatch, bool),
    // Whether keep selection when copy
    Copy(Option<bool>),
    // Vi mode events
    ToggleViMode,
    ViMotion(ViMotion),
    MoveViCursorToPoint(Point),
}

type ClipboardFormatter = Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;
type ColorFormatter = Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;
type TextAreaSizeFormatter = Arc<dyn Fn(TerminalBounds) -> String + Sync + Send + 'static>;

#[derive(Clone)]
pub(crate) enum TerminalBackendEvent {
    MouseCursorDirty,
    Title(String),
    ResetTitle,
    ClipboardStore(String),
    ClipboardLoad(ClipboardFormatter),
    ColorRequest(usize, ColorFormatter),
    PtyWrite(String),
    TextAreaSizeRequest(TextAreaSizeFormatter),
    CursorBlinkingChange,
    Wakeup,
    Bell,
    Exit,
    ChildExit(ExitStatus),
}

impl fmt::Debug for TerminalBackendEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MouseCursorDirty => f.write_str("MouseCursorDirty"),
            Self::Title(title) => write!(f, "Title({title})"),
            Self::ResetTitle => f.write_str("ResetTitle"),
            Self::ClipboardStore(data) => write!(f, "ClipboardStore({data})"),
            Self::ClipboardLoad(_) => f.write_str("ClipboardLoad"),
            Self::ColorRequest(index, _) => write!(f, "ColorRequest({index})"),
            Self::PtyWrite(output) => write!(f, "PtyWrite({output})"),
            Self::TextAreaSizeRequest(_) => f.write_str("TextAreaSizeRequest"),
            Self::CursorBlinkingChange => f.write_str("CursorBlinkingChange"),
            Self::Wakeup => f.write_str("Wakeup"),
            Self::Bell => f.write_str("Bell"),
            Self::Exit => f.write_str("Exit"),
            Self::ChildExit(status) => write!(f, "ChildExit({status})"),
        }
    }
}

enum PtyEvent {
    Event(TerminalBackendEvent),
    /// 由 [`kitty_graphics::GraphicsScanner`] 在 PTY 读取线程上解析出来的
    /// 图形协议动作。
    Graphics(Vec<kitty_graphics::GraphicsEvent>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalBounds {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub bounds: Bounds<Pixels>,
}

impl TerminalBounds {
    pub fn new(line_height: Pixels, cell_width: Pixels, bounds: Bounds<Pixels>) -> Self {
        TerminalBounds {
            cell_width,
            line_height,
            bounds,
        }
    }

    pub fn num_lines(&self) -> usize {
        // Tolerance to prevent f32 precision from losing a row:
        // `N * line_height / line_height` can be N-epsilon, which floor()
        // would round down, pushing the first line into invisible scrollback.
        let raw = self.bounds.size.height / self.line_height;
        raw.next_up().floor() as usize
    }

    pub fn num_columns(&self) -> usize {
        let raw = self.bounds.size.width / self.cell_width;
        raw.next_up().floor() as usize
    }

    pub fn height(&self) -> Pixels {
        self.bounds.size.height
    }

    pub fn width(&self) -> Pixels {
        self.bounds.size.width
    }

    pub fn cell_width(&self) -> Pixels {
        self.cell_width
    }

    pub fn line_height(&self) -> Pixels {
        self.line_height
    }
}

impl Default for TerminalBounds {
    fn default() -> Self {
        TerminalBounds::new(
            DEBUG_LINE_HEIGHT,
            DEBUG_CELL_WIDTH,
            Bounds {
                origin: GpuiPoint::default(),
                size: Size {
                    width: DEBUG_TERMINAL_WIDTH,
                    height: DEBUG_TERMINAL_HEIGHT,
                },
            },
        )
    }
}

fn normalize_terminal_bounds(mut bounds: TerminalBounds) -> TerminalBounds {
    bounds.bounds.size.height = cmp::max(bounds.line_height, bounds.height());
    bounds.bounds.size.width = cmp::max(bounds.cell_width, bounds.width());
    bounds
}

#[derive(Error, Debug)]
pub struct TerminalError {
    pub directory: Option<PathBuf>,
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub title_override: Option<String>,
    pub source: std::io::Error,
}

impl TerminalError {
    fn fmt_directory(&self) -> String {
        self.directory
            .clone()
            .map(|path| {
                match path
                    .into_os_string()
                    .into_string()
                    .map_err(|os_str| format!("<non-utf8 path> {}", os_str.to_string_lossy()))
                {
                    Ok(s) => s,
                    Err(s) => s,
                }
            })
            .unwrap_or_else(|| "<none specified>".to_string())
    }

    fn fmt_shell(&self) -> String {
        if let Some(title_override) = &self.title_override {
            format!(
                "{} {} ({})",
                self.program.as_deref().unwrap_or("<system defined shell>"),
                self.args.as_ref().into_iter().flatten().format(" "),
                title_override
            )
        } else {
            format!(
                "{} {}",
                self.program.as_deref().unwrap_or("<system defined shell>"),
                self.args.as_ref().into_iter().flatten().format(" ")
            )
        }
    }
}

impl Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir_string: String = self.fmt_directory();
        let shell = self.fmt_shell();

        write!(
            f,
            "Working directory: {} Shell command: `{}`, IOError: {}",
            dir_string, shell, self.source
        )
    }
}

// https://github.com/alacritty/alacritty/blob/cb3a79dbf6472740daca8440d5166c1d4af5029e/extra/man/alacritty.5.scd?plain=1#L207-L213
const DEFAULT_SCROLL_HISTORY_LINES: usize = 10_000;
pub const MAX_SCROLL_HISTORY_LINES: usize = 100_000;
const MAX_STRUCTURED_GRID_COLUMNS: usize = 4_096;
const MAX_STRUCTURED_GRID_ROWS: usize = 4_096;
const MAX_STRUCTURED_GRID_CELLS: usize = 1_048_576;
static NEXT_INIT_COMMAND_STARTUP_MARKER_ID: AtomicU64 = AtomicU64::new(1);

const INIT_COMMAND_STARTUP_MARKER_PREFIX: &str = "__zed_init_command_ready_";
const INIT_COMMAND_STARTUP_MARKER_SUFFIX: &str = "__";
const INIT_COMMAND_STARTUP_MARKER_SEARCH_LINES: usize = 64;

fn init_command_startup_marker(marker_id: u64) -> String {
    format!("{INIT_COMMAND_STARTUP_MARKER_PREFIX}{marker_id}{INIT_COMMAND_STARTUP_MARKER_SUFFIX}")
}

fn init_command_startup_marker_command(shell_kind: ShellKind, marker_id: u64) -> String {
    // Split the marker across the command so its echo can't satisfy the
    // handshake; only the command's output contains the contiguous marker.
    match shell_kind {
        ShellKind::PowerShell | ShellKind::Pwsh => format!(
            "Write-Output ('{INIT_COMMAND_STARTUP_MARKER_PREFIX}' + '{marker_id}' + '{INIT_COMMAND_STARTUP_MARKER_SUFFIX}')"
        ),
        ShellKind::Cmd => {
            format!(
                "<nul set /p zed_init_ready={INIT_COMMAND_STARTUP_MARKER_PREFIX}&echo {marker_id}{INIT_COMMAND_STARTUP_MARKER_SUFFIX}"
            )
        }
        ShellKind::Nushell => {
            format!(
                "print $\"{INIT_COMMAND_STARTUP_MARKER_PREFIX}({marker_id}){INIT_COMMAND_STARTUP_MARKER_SUFFIX}\""
            )
        }
        ShellKind::Posix
        | ShellKind::Csh
        | ShellKind::Tcsh
        | ShellKind::Rc
        | ShellKind::Fish
        | ShellKind::Xonsh
        | ShellKind::Elvish => format!(
            "printf '%s%s%s\\n' {INIT_COMMAND_STARTUP_MARKER_PREFIX} {marker_id} {INIT_COMMAND_STARTUP_MARKER_SUFFIX}"
        ),
    }
}

pub struct TerminalBuilder {
    terminal: Terminal,
    events_rx: UnboundedReceiver<PtyEvent>,
}

impl TerminalBuilder {
    pub fn new_display_only(
        cursor_shape: SettingsCursorShape,
        alternate_scroll: AlternateScroll,
        max_scroll_history_lines: Option<usize>,
        window_id: u64,
        background_executor: &BackgroundExecutor,
        path_style: PathStyle,
    ) -> TerminalBuilder {
        Self::new_display_only_with_bounds(
            cursor_shape,
            alternate_scroll,
            max_scroll_history_lines,
            window_id,
            background_executor,
            path_style,
            TerminalBounds::default(),
        )
    }

    pub fn new_display_only_with_bounds(
        cursor_shape: SettingsCursorShape,
        alternate_scroll: AlternateScroll,
        max_scroll_history_lines: Option<usize>,
        window_id: u64,
        background_executor: &BackgroundExecutor,
        path_style: PathStyle,
        terminal_bounds: TerminalBounds,
    ) -> TerminalBuilder {
        let terminal_bounds = normalize_terminal_bounds(terminal_bounds);

        let scrolling_history = max_scroll_history_lines
            .unwrap_or(DEFAULT_SCROLL_HISTORY_LINES)
            .min(MAX_SCROLL_HISTORY_LINES);
        let config = display_only_term_config(scrolling_history, cursor_shape);

        let (events_tx, events_rx) = unbounded();
        let term = new_term(&config, terminal_bounds, events_tx, alternate_scroll);

        let terminal = Terminal {
            task: None,
            terminal_type: TerminalType::DisplayOnly,
            input_sink: None,
            subprocess: None,
            completion_tx: None,
            term,
            term_config: config,
            output_processor: Processor::<StdSyncHandler>::new(),
            output_previous_byte_was_cr: false,
            title_override: None,
            events: VecDeque::with_capacity(10),
            last_content: Content {
                terminal_bounds,
                ..Default::default()
            },
            last_mouse: None,
            mouse_down_position: None,
            matches: Vec::new(),

            selection_head: None,
            breadcrumb_text: String::new(),
            scroll_px: px(0.),
            next_link_id: 0,
            selection_phase: SelectionPhase::Ended,
            hyperlink_regex_searches: RegexSearches::default(),
            vi_mode_enabled: false,
            search_state: None,
            is_remote_terminal: false,
            last_mouse_move_time: Instant::now(),
            last_hyperlink_search_position: None,
            mouse_down_hyperlink: None,
            #[cfg(windows)]
            shell_program: None,
            activation_script: Vec::new(),
            template: CopyTemplate {
                shell: Shell::System,
                env: HashMap::default(),
                cursor_shape,
                alternate_scroll,
                max_scroll_history_lines,
                path_hyperlink_regexes: Vec::default(),
                path_hyperlink_timeout_ms: 0,
                window_id,
            },
            child_exited: None,
            keyboard_input_sent: false,
            init_command_startup_marker: None,
            init_command_startup_tx: None,
            event_loop_task: Task::ready(Ok(())),
            background_executor: background_executor.clone(),
            path_style,
            image_cache: kitty_graphics::PaneImageCache::new(),
            image_placements: Vec::new(),
            graphics_scanner: kitty_graphics::GraphicsScanner::new(),
            #[cfg(any(test, feature = "test-support"))]
            input_log: Vec::new(),
            #[cfg(any(test, feature = "test-support"))]
            pty_write_log: Default::default(),
        };

        TerminalBuilder {
            terminal,
            events_rx,
        }
    }

    pub fn new(
        working_directory: Option<PathBuf>,
        task: Option<TaskState>,
        shell: Shell,
        mut env: HashMap<String, String>,
        cursor_shape: SettingsCursorShape,
        alternate_scroll: AlternateScroll,
        max_scroll_history_lines: Option<usize>,
        path_hyperlink_regexes: Vec<String>,
        path_hyperlink_timeout_ms: u64,
        is_remote_terminal: bool,
        window_id: u64,
        completion_tx: Option<Sender<Option<ExitStatus>>>,
        cx: &App,
        activation_script: Vec<String>,
        path_style: PathStyle,
    ) -> Task<Result<TerminalBuilder>> {
        let version = release_channel::AppVersion::global(cx);
        let background_executor = cx.background_executor().clone();
        // Headless hosts (e.g. the eval CLI) have no controlling TTY, so PTY
        // allocation / acquiring a controlling terminal fails with `ENOTTY`.
        // When set, run the command as a plain subprocess instead.
        let no_pty = HeadlessTerminal::is_enabled(cx);
        #[cfg(not(windows))]
        let child_signal_mask = match current_child_signal_mask()
            .context("failed to capture terminal child signal mask")
        {
            Ok(signal_mask) => Some(signal_mask),
            Err(error) => return Task::ready(Err(error)),
        };
        let fut = async move {
            // Remove SHLVL so the spawned shell initializes it to 1, matching
            // the behavior of standalone terminal emulators like iTerm2/Kitty/Alacritty.
            env.remove("SHLVL");

            // If the parent environment doesn't have a locale set
            // (As is the case when launched from a .app on MacOS),
            // and the Project doesn't have a locale set, then
            // set a fallback for our child environment to use.
            if std::env::var("LANG").is_err() {
                env.entry("LANG".to_string())
                    .or_insert_with(|| "en_US.UTF-8".to_string());
            }

            insert_zed_terminal_env(&mut env, &version);

            #[derive(Default)]
            struct ShellParams {
                program: String,
                args: Option<Vec<String>>,
                title_override: Option<String>,
            }

            impl ShellParams {
                fn new(
                    program: String,
                    args: Option<Vec<String>>,
                    title_override: Option<String>,
                ) -> Self {
                    log::debug!("Using {program} as shell");
                    Self {
                        program,
                        args,
                        title_override,
                    }
                }
            }

            let shell_params = match shell.clone() {
                Shell::System => {
                    if cfg!(windows) {
                        Some(ShellParams::new(
                            util::shell::get_windows_system_shell(),
                            None,
                            None,
                        ))
                    } else {
                        None
                    }
                }
                Shell::Program(program) => Some(ShellParams::new(program, None, None)),
                Shell::WithArguments {
                    program,
                    args,
                    title_override,
                } => Some(ShellParams::new(program, Some(args), title_override)),
            };
            let terminal_title_override =
                shell_params.as_ref().and_then(|e| e.title_override.clone());

            #[cfg(windows)]
            let shell_program = shell_params.as_ref().map(|params| {
                use util::ResultExt;

                Self::resolve_path(&params.program)
                    .log_err()
                    .unwrap_or(params.program.clone())
            });

            // Note: when remoting, this shell_kind will scrutinize `ssh` or
            // `wsl.exe` as a shell and fall back to posix or powershell based on
            // the compilation target. This is fine right now due to the restricted
            // way we use the return value, but would become incorrect if we
            // supported remoting into windows.
            let shell_kind = shell.shell_kind(cfg!(windows));

            let scrolling_history = if task.is_some() {
                // Tasks like `cargo build --all` may produce a lot of output, ergo allow maximum scrolling.
                // After the task finishes, we do not allow appending to that terminal, so small tasks output should not
                // cause excessive memory usage over time.
                MAX_SCROLL_HISTORY_LINES
            } else {
                max_scroll_history_lines
                    .unwrap_or(DEFAULT_SCROLL_HISTORY_LINES)
                    .min(MAX_SCROLL_HISTORY_LINES)
            };
            let config = pty_term_config(scrolling_history, cursor_shape);

            //Spawn a task so the Alacritty EventLoop (or the subprocess reader) can communicate with us
            //TODO: Remove with a bounded sender which can be dispatched on &self
            let (events_tx, events_rx) = unbounded();
            //Set up the terminal...
            let term = new_term(
                &config,
                TerminalBounds::default(),
                events_tx.clone(),
                alternate_scroll,
            );

            // When `no_pty` is set (headless hosts), run the task as a plain
            // subprocess and pump its piped output into the same emulator the
            // PTY path would feed.
            let (terminal_type, subprocess) = if no_pty {
                let (program, args) = match &shell_params {
                    Some(params) => (
                        params.program.clone(),
                        params.args.clone().unwrap_or_default(),
                    ),
                    None => (util::shell::get_system_shell(), Vec::new()),
                };
                let subprocess = match spawn_task_subprocess(
                    program,
                    args,
                    env.clone(),
                    working_directory.clone(),
                    term.clone(),
                    events_tx,
                    &background_executor,
                ) {
                    Ok(subprocess) => subprocess,
                    Err(error) => {
                        bail!(TerminalError {
                            directory: working_directory,
                            program: shell_params.as_ref().map(|params| params.program.clone()),
                            args: shell_params.as_ref().and_then(|params| params.args.clone()),
                            title_override: terminal_title_override,
                            source: std::io::Error::other(format!("{error:#}")),
                        });
                    }
                };
                (TerminalType::DisplayOnly, Some(subprocess))
            } else {
                let alacritty_shell = shell_params.as_ref().map(|params| {
                    (
                        params.program.clone(),
                        params.args.clone().unwrap_or_default(),
                    )
                });
                let pty_options = pty_options(
                    alacritty_shell,
                    working_directory.clone(),
                    env.clone(),
                    // We pass in the foreground thread's signal mask to the child process via pty_options,
                    // so terminal construction can run on a background thread without breaking Ctrl-C and other signals
                    // otherwise the terminal would inherit the background executor's signal mask which blocks
                    // some terminal signals
                    #[cfg(not(windows))]
                    child_signal_mask,
                    #[cfg(windows)]
                    shell_kind.tty_escape_args(),
                );

                //Setup the pty...
                let pty = match open_pty(&pty_options, TerminalBounds::default(), window_id) {
                    Ok(pty) => pty,
                    Err(error) => {
                        bail!(TerminalError {
                            directory: working_directory,
                            program: shell_params.as_ref().map(|params| params.program.clone()),
                            args: shell_params.as_ref().and_then(|params| params.args.clone()),
                            title_override: terminal_title_override,
                            source: error,
                        });
                    }
                };

                let pty_info = PtyProcessInfo::new(ProcessIdGetter::from(&pty));

                //And connect them together
                let pty_tx =
                    spawn_event_loop(term.clone(), events_tx, pty, pty_options.drain_on_exit)?;

                (
                    TerminalType::Pty {
                        pty_tx,
                        info: Arc::new(pty_info),
                    },
                    None,
                )
            };

            let no_task = task.is_none();
            let terminal = Terminal {
                task,
                terminal_type,
                input_sink: None,
                subprocess,
                completion_tx,
                term,
                term_config: config,
                output_processor: Processor::<StdSyncHandler>::new(),
                output_previous_byte_was_cr: false,
                title_override: terminal_title_override,
                events: VecDeque::with_capacity(10), //Should never get this high.
                last_content: Default::default(),
                last_mouse: None,
                mouse_down_position: None,
                matches: Vec::new(),

                selection_head: None,
                breadcrumb_text: String::new(),
                scroll_px: px(0.),
                next_link_id: 0,
                selection_phase: SelectionPhase::Ended,
                hyperlink_regex_searches: RegexSearches::new(
                    &path_hyperlink_regexes,
                    path_hyperlink_timeout_ms,
                ),
                vi_mode_enabled: false,
                search_state: None,
                is_remote_terminal,
                last_mouse_move_time: Instant::now(),
                last_hyperlink_search_position: None,
                mouse_down_hyperlink: None,
                #[cfg(windows)]
                shell_program,
                activation_script: activation_script.clone(),
                template: CopyTemplate {
                    shell,
                    env,
                    cursor_shape,
                    alternate_scroll,
                    max_scroll_history_lines,
                    path_hyperlink_regexes,
                    path_hyperlink_timeout_ms,
                    window_id,
                },
                child_exited: None,
                keyboard_input_sent: false,
                init_command_startup_marker: None,
                init_command_startup_tx: None,
                event_loop_task: Task::ready(Ok(())),
                background_executor,
                path_style,
                image_cache: kitty_graphics::PaneImageCache::new(),
                image_placements: Vec::new(),
                graphics_scanner: kitty_graphics::GraphicsScanner::new(),
                #[cfg(any(test, feature = "test-support"))]
                input_log: Vec::new(),
                #[cfg(any(test, feature = "test-support"))]
                pty_write_log: Default::default(),
            };

            if !activation_script.is_empty() && no_task {
                for activation_script in activation_script {
                    terminal.write_to_pty(activation_script.into_bytes());
                    // Simulate enter key press
                    // NOTE(PowerShell): using `\r\n` will put PowerShell in a continuation mode (infamous >> character)
                    // and generally mess up the rendering.
                    terminal.write_to_pty(b"\x0d");
                }
                // In order to clear the screen at this point, we have two options:
                // 1. We can send a shell-specific command such as "clear" or "cls"
                // 2. We can "echo" a marker message that we will then catch when handling a Wakeup event
                //    and clear the screen using `terminal.clear()` method
                // We cannot issue a `terminal.clear()` command at this point as alacritty is evented
                // and while we have sent the activation script to the pty, it will be executed asynchronously.
                // Therefore, we somehow need to wait for the activation script to finish executing before we
                // can proceed with clearing the screen.
                terminal.write_to_pty(shell_kind.clear_screen_command().as_bytes());
                // Simulate enter key press
                terminal.write_to_pty(b"\x0d");
            }

            Ok(TerminalBuilder {
                terminal,
                events_rx,
            })
        };
        cx.background_spawn(fut)
    }

    pub fn subscribe(mut self, cx: &Context<Terminal>) -> Terminal {
        //Event loop
        self.terminal.event_loop_task = cx.spawn(async move |terminal, cx| {
            while let Some(event) = self.events_rx.next().await {
                terminal.update(cx, |terminal, cx| {
                    //Process the first event immediately for lowered latency
                    terminal.process_pty_event(event, cx);
                })?;

                'outer: loop {
                    let mut events = Vec::new();

                    #[cfg(any(test, feature = "test-support"))]
                    let mut timer = cx.background_executor().simulate_random_delay().fuse();
                    #[cfg(not(any(test, feature = "test-support")))]
                    let mut timer = cx
                        .background_executor()
                        .timer(std::time::Duration::from_millis(4))
                        .fuse();

                    let mut wakeup = false;
                    loop {
                        futures::select_biased! {
                            _ = timer => break,
                            event = self.events_rx.next() => {
                                if let Some(event) = event {
                                    if matches!(event, PtyEvent::Event(TerminalBackendEvent::Wakeup))
                                    {
                                        wakeup = true;
                                    } else {
                                        events.push(event);
                                    }

                                    if events.len() > 100 {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            },
                        }
                    }

                    if events.is_empty() && !wakeup {
                        yield_now().await;
                        break 'outer;
                    }

                    terminal.update(cx, |this, cx| {
                        if wakeup {
                            this.process_event(TerminalBackendEvent::Wakeup, cx);
                        }

                        for event in events {
                            this.process_pty_event(event, cx);
                        }
                    })?;
                    yield_now().await;
                }
            }
            anyhow::Ok(())
        });
        self.terminal
    }

    #[cfg(windows)]
    fn resolve_path(path: &str) -> Result<String> {
        use windows::Win32::Storage::FileSystem::SearchPathW;
        use windows::core::HSTRING;

        let path = if path.starts_with(r"\\?\") || !path.contains(&['/', '\\']) {
            path.to_string()
        } else {
            r"\\?\".to_string() + path
        };

        let required_length = unsafe { SearchPathW(None, &HSTRING::from(&path), None, None, None) };
        let mut buf = vec![0u16; required_length as usize];
        let size = unsafe { SearchPathW(None, &HSTRING::from(&path), None, Some(&mut buf), None) };

        Ok(String::from_utf16(&buf[..size as usize])?)
    }
}

enum TerminalType {
    Pty {
        pty_tx: PtySender,
        info: Arc<PtyProcessInfo>,
    },
    DisplayOnly,
}

pub struct Terminal {
    terminal_type: TerminalType,
    /// Optional sink for DisplayOnly terminals that still need to emit
    /// input bytes (mouse reports, focus events) to a remote mux PTY.
    /// When set, `write_to_pty` forwards bytes here instead of no-opping.
    input_sink: Option<std::sync::Arc<dyn Fn(Vec<u8>) + Send + Sync>>,
    /// Set for non-PTY terminals (see [`HeadlessTerminal`]); owns the spawned
    /// subprocess and the task pumping its output into the grid.
    subprocess: Option<SubprocessHandle>,
    completion_tx: Option<Sender<Option<ExitStatus>>>,
    term: Arc<AlacrittyTermLock>,
    term_config: AlacrittyTermConfig,
    output_processor: Processor<StdSyncHandler>,
    /// Streaming LF normalization state for non-PTY injected output. PTY bytes
    /// use `write_pty_output` and bypass normalization entirely.
    output_previous_byte_was_cr: bool,
    events: VecDeque<InternalEvent>,
    /// This is only used for mouse mode cell change detection
    last_mouse: Option<(Point, SelectionSide)>,
    /// Window-relative position of the most recent left mouse-down. Used to
    /// apply a drag threshold before starting a selection (see #58970).
    mouse_down_position: Option<GpuiPoint<Pixels>>,
    pub matches: Vec<Range>,
    pub last_content: Content,
    pub selection_head: Option<Point>,

    pub breadcrumb_text: String,
    title_override: Option<String>,
    scroll_px: Pixels,
    next_link_id: usize,
    selection_phase: SelectionPhase,
    hyperlink_regex_searches: RegexSearches,
    task: Option<TaskState>,
    vi_mode_enabled: bool,
    /// §12 Plan 31 — copy-mode search query, `None` until one is confirmed.
    search_state: Option<SearchState>,
    is_remote_terminal: bool,
    last_mouse_move_time: Instant,
    last_hyperlink_search_position: Option<GpuiPoint<Pixels>>,
    mouse_down_hyperlink: Option<HyperlinkMatch>,
    #[cfg(windows)]
    shell_program: Option<String>,
    template: CopyTemplate,
    activation_script: Vec<String>,
    child_exited: Option<ExitStatus>,
    keyboard_input_sent: bool,
    init_command_startup_marker: Option<String>,
    init_command_startup_tx: Option<Sender<()>>,
    event_loop_task: Task<Result<(), anyhow::Error>>,
    background_executor: BackgroundExecutor,
    path_style: PathStyle,
    /// 每 pane 图像缓存 (kitty graphics / OSC 1337)
    image_cache: kitty_graphics::PaneImageCache,
    /// 当前有效的图像放置, 按插入顺序保存。
    image_placements: Vec<ImagePlacement>,
    /// 扫描 [`Terminal::write_output`] 注入的字节流, PTY 路径上的扫描器在
    /// 读取线程里 (见 [`crate::alacritty::spawn_event_loop`])。
    graphics_scanner: kitty_graphics::GraphicsScanner,
    #[cfg(any(test, feature = "test-support"))]
    input_log: Vec<Vec<u8>>,
    #[cfg(any(test, feature = "test-support"))]
    pty_write_log: std::cell::RefCell<Vec<Vec<u8>>>,
}

struct CopyTemplate {
    shell: Shell,
    env: HashMap<String, String>,
    cursor_shape: SettingsCursorShape,
    alternate_scroll: AlternateScroll,
    max_scroll_history_lines: Option<usize>,
    path_hyperlink_regexes: Vec<String>,
    path_hyperlink_timeout_ms: u64,
    window_id: u64,
}

#[derive(Debug)]
pub struct TaskState {
    pub status: TaskStatus,
    pub completion_rx: Receiver<Option<ExitStatus>>,
    pub spawned_task: SpawnInTerminal,
}

/// A status of the current terminal tab's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// The task had been started, but got cancelled or somehow otherwise it did not
    /// report its exit code before the terminal event loop was shut down.
    Unknown,
    /// The task is started and running currently.
    Running,
    /// After the start, the task stopped running and reported its error code back.
    Completed { success: bool },
}

impl TaskStatus {
    fn register_terminal_exit(&mut self) {
        if self == &Self::Running {
            *self = Self::Unknown;
        }
    }

    fn register_task_exit(&mut self, error_code: i32) {
        *self = TaskStatus::Completed {
            success: error_code == 0,
        };
    }
}

const FIND_HYPERLINK_THROTTLE_PX: Pixels = px(5.0);

/// Minimum pointer movement before a left click begins a selection. This keeps
/// a click that jitters by a pixel or two (such as the window-focusing click)
/// from starting a selection and, with `copy_on_select` enabled, clobbering the
/// clipboard. Mirrors the drag threshold used by gpui's `div` element.
const SELECTION_DRAG_THRESHOLD: f64 = 2.0;

impl Terminal {
    fn process_pty_event(&mut self, event: PtyEvent, cx: &mut Context<Self>) {
        match event {
            PtyEvent::Event(event) => self.process_event(event, cx),
            PtyEvent::Graphics(events) => self.apply_graphics_events(events, cx),
        }
    }

    fn process_event(&mut self, event: TerminalBackendEvent, cx: &mut Context<Self>) {
        match event {
            TerminalBackendEvent::Title(title) => {
                // ignore default shell program title change as windows always sends those events
                // and it would end up showing the shell executable path in breadcrumbs
                #[cfg(windows)]
                if self
                    .shell_program
                    .as_ref()
                    .map(|e| *e == title)
                    .unwrap_or(false)
                {
                    return;
                }

                self.breadcrumb_text = title;
                cx.emit(Event::BreadcrumbsChanged);
            }
            TerminalBackendEvent::ResetTitle => {
                self.breadcrumb_text = String::new();
                cx.emit(Event::BreadcrumbsChanged);
            }
            TerminalBackendEvent::ClipboardStore(data) => {
                cx.write_to_clipboard(ClipboardItem::new_string(data))
            }
            TerminalBackendEvent::ClipboardLoad(format) => {
                self.write_to_pty(
                    match &cx.read_from_clipboard().and_then(|item| item.text()) {
                        // The terminal only supports pasting strings, not images.
                        Some(text) => format(text),
                        _ => format(""),
                    }
                    .into_bytes(),
                )
            }
            TerminalBackendEvent::PtyWrite(out) => self.write_to_pty(out.into_bytes()),
            TerminalBackendEvent::TextAreaSizeRequest(format) => {
                self.write_to_pty(format(self.last_content.terminal_bounds).into_bytes())
            }
            TerminalBackendEvent::CursorBlinkingChange => {
                let terminal = self.term.lock();
                let blinking = terminal.cursor_style().blinking;
                cx.emit(Event::BlinkChanged(blinking));
            }
            TerminalBackendEvent::Bell => {
                cx.emit(Event::Bell);
            }
            TerminalBackendEvent::Exit => self.register_task_finished(None, cx),
            TerminalBackendEvent::MouseCursorDirty => {
                //NOOP, Handled in render
            }
            TerminalBackendEvent::Wakeup => {
                self.detect_init_command_startup_marker();
                cx.emit(Event::Wakeup);

                if let TerminalType::Pty { info, .. } = &self.terminal_type {
                    info.emit_title_changed_if_changed(cx);
                }
            }
            TerminalBackendEvent::ColorRequest(index, format) => {
                // It's important that the color request is processed here to retain relative order
                // with other PTY writes. Otherwise applications might witness out-of-order
                // responses to requests. For example: An application sending `OSC 11 ; ? ST`
                // (color request) followed by `CSI c` (request device attributes) would receive
                // the response to `CSI c` first.
                // Instead of locking, we could store the colors in `self.last_content`. But then
                // we might respond with out of date value if a "set color" sequence is immediately
                // followed by a color request sequence.

                let color = self.term.lock().colors()[index]
                    .unwrap_or_else(|| to_vte_rgb(get_color_at_index(index, cx.theme().as_ref())));
                self.write_to_pty(format(color).into_bytes());
            }
            TerminalBackendEvent::ChildExit(exit_status) => {
                self.register_task_finished(Some(exit_status), cx);
            }
        }
    }

    pub fn selection_started(&self) -> bool {
        self.selection_phase == SelectionPhase::Selecting
    }

    fn process_terminal_event(
        &mut self,
        event: &InternalEvent,
        term: &mut AlacrittyTerm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            &InternalEvent::Resize(new_bounds) => {
                let new_bounds = normalize_terminal_bounds(new_bounds);
                trace!("Resizing: new_bounds={new_bounds:?}");

                self.last_content.terminal_bounds = new_bounds;

                if let TerminalType::Pty { pty_tx, .. } = &self.terminal_type {
                    pty_tx.resize(new_bounds);
                }

                resize(term, new_bounds);
                // If there are matches we need to emit a wake up event to
                // invalidate the matches and recalculate their locations
                // in the new terminal layout
                if !self.matches.is_empty() {
                    cx.emit(Event::Wakeup);
                }
            }
            InternalEvent::Clear => {
                trace!("Clearing");
                clear_saved_screen(term);
                cx.emit(Event::Wakeup);
            }
            InternalEvent::Scroll(scroll) => {
                trace!("Scrolling: scroll={scroll:?}");
                scroll_display(term, *scroll);
                self.refresh_hovered_word(window);

                if self.vi_mode_enabled {
                    update_vi_cursor_for_scroll(term, *scroll);
                    if let Some(selection_head) = update_selection_to_vi_cursor(term) {
                        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                        if let Some(selection_text) = selection_text(term) {
                            cx.write_to_primary(ClipboardItem::new_string(selection_text));
                        }

                        self.selection_head = Some(selection_head);
                        cx.emit(Event::SelectionsChanged)
                    }
                }
            }
            InternalEvent::ScrollToDisplayOffset(offset) => {
                // §15.12 Resolve an absolute offset against the live grid,
                // clamped to history. Computed as a relative Delta at flush
                // time (term lock held) so it stays correct after prior output.
                let current = display_offset(term);
                let history = total_lines(term).saturating_sub(screen_lines(term));
                let target = (*offset).min(history);
                let delta = target as i32 - current as i32;
                if delta != 0 {
                    let scroll = Scroll::Delta(delta);
                    trace!("Scrolling to display offset: target={target}, delta={delta}");
                    scroll_display(term, scroll);
                    self.refresh_hovered_word(window);

                    if self.vi_mode_enabled {
                        update_vi_cursor_for_scroll(term, scroll);
                        if let Some(selection_head) = update_selection_to_vi_cursor(term) {
                            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                            if let Some(selection_text) = selection_text(term) {
                                cx.write_to_primary(ClipboardItem::new_string(selection_text));
                            }

                            self.selection_head = Some(selection_head);
                            cx.emit(Event::SelectionsChanged)
                        }
                    }
                }
            }
            InternalEvent::SetSelection(selection) => {
                trace!("Setting selection: selection={selection:?}");
                set_term_selection(term, selection.as_ref());

                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                if let Some(selection_text) = selection_text(term) {
                    cx.write_to_primary(ClipboardItem::new_string(selection_text));
                }

                if let Some(selection) = selection {
                    self.selection_head = Some(selection.head);
                }
                cx.emit(Event::SelectionsChanged)
            }
            InternalEvent::UpdateSelection(position) => {
                trace!("Updating selection: position={position:?}");
                let (point, side) = grid_point_and_side(
                    *position,
                    self.last_content.terminal_bounds,
                    display_offset(term),
                );

                if update_term_selection(term, point, side) {
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    if let Some(selection_text) = selection_text(term) {
                        cx.write_to_primary(ClipboardItem::new_string(selection_text));
                    }

                    self.selection_head = Some(point);
                    cx.emit(Event::SelectionsChanged)
                }
            }

            InternalEvent::Copy(keep_selection) => {
                trace!("Copying selection: keep_selection={keep_selection:?}");
                if let Some(txt) = selection_text(term) {
                    cx.write_to_clipboard(ClipboardItem::new_string(txt));
                    if !keep_selection.unwrap_or_else(|| {
                        let settings = TerminalSettings::get_global(cx);
                        settings.keep_selection_on_copy
                    }) {
                        self.events.push_back(InternalEvent::SetSelection(None));
                    }
                }
            }
            InternalEvent::ScrollToPoint(point) => {
                trace!("Scrolling to point: point={point:?}");
                scroll_to_point(term, *point);
                self.refresh_hovered_word(window);
            }
            InternalEvent::MoveViCursorToPoint(point) => {
                trace!("Move vi cursor to point: point={point:?}");
                vi_goto_point(term, *point);
                self.refresh_hovered_word(window);
            }
            InternalEvent::ToggleViMode => {
                trace!("Toggling vi mode");
                self.vi_mode_enabled = !self.vi_mode_enabled;
                toggle_term_vi_mode(term);
            }
            InternalEvent::ViMotion(motion) => {
                trace!("Performing vi motion: motion={motion:?}");
                vi_motion(term, *motion);
            }
            InternalEvent::FindHyperlink(position, open) => {
                trace!("Finding hyperlink at position: position={position:?}, open={open:?}");

                let point = grid_point(
                    *position,
                    self.last_content.terminal_bounds,
                    display_offset(term),
                );

                match find_from_terminal_point(
                    term,
                    point,
                    &mut self.hyperlink_regex_searches,
                    self.path_style,
                ) {
                    Some(hyperlink) => {
                        self.process_hyperlink(hyperlink, *open, cx);
                    }
                    None => {
                        self.last_content.last_hovered_word = None;
                        cx.emit(Event::NewNavigationTarget(None));
                    }
                }
            }
            InternalEvent::ProcessHyperlink(hyperlink, open) => {
                self.process_hyperlink(hyperlink.clone(), *open, cx);
            }
        }
    }

    fn process_hyperlink(&mut self, hyperlink: HyperlinkMatch, open: bool, cx: &mut Context<Self>) {
        let HyperlinkMatch {
            text: maybe_url_or_path,
            is_url,
            range,
        } = hyperlink;
        let prev_hovered_word = self.last_content.last_hovered_word.take();

        let target = if is_url {
            if let Some(path) = maybe_url_or_path.strip_prefix("file://") {
                let decoded_path = urlencoding::decode(path)
                    .map(|decoded| decoded.into_owned())
                    .unwrap_or(path.to_owned());

                MaybeNavigationTarget::PathLike(PathLikeTarget {
                    maybe_path: decoded_path,
                    terminal_dir: self.working_directory(),
                })
            } else {
                MaybeNavigationTarget::Url(maybe_url_or_path.clone())
            }
        } else {
            MaybeNavigationTarget::PathLike(PathLikeTarget {
                maybe_path: maybe_url_or_path.clone(),
                terminal_dir: self.working_directory(),
            })
        };

        if open {
            cx.emit(Event::Open(target));
        } else {
            self.update_selected_word(prev_hovered_word, range, maybe_url_or_path, target, cx);
        }
    }

    fn find_hyperlink_at_point(&mut self, point: Point) -> Option<HyperlinkMatch> {
        let term_lock = self.term.lock();
        find_from_terminal_point(
            &term_lock,
            point,
            &mut self.hyperlink_regex_searches,
            self.path_style,
        )
    }

    /// Atomically replace a display-only terminal's active viewport from an
    /// authoritative structured snapshot.
    pub fn apply_structured_snapshot(
        &mut self,
        snapshot: &StructuredTerminalSnapshot,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if !matches!(self.terminal_type, TerminalType::DisplayOnly) {
            bail!("structured snapshots are only valid for display-only terminals");
        }
        if snapshot.cols == 0 || snapshot.rows == 0 {
            bail!("structured snapshot dimensions must be nonzero");
        }
        if snapshot.cols > MAX_STRUCTURED_GRID_COLUMNS || snapshot.rows > MAX_STRUCTURED_GRID_ROWS {
            bail!("structured snapshot dimensions exceed terminal limits");
        }
        let expected_cells = snapshot
            .cols
            .checked_mul(snapshot.rows)
            .filter(|&cells| cells <= MAX_STRUCTURED_GRID_CELLS)
            .ok_or_else(|| {
                anyhow::anyhow!("structured snapshot cell count exceeds terminal limit")
            })?;
        if snapshot.cells.len() != expected_cells {
            bail!(
                "structured snapshot has {} cells, expected {} for {}x{}",
                snapshot.cells.len(),
                expected_cells,
                snapshot.cols,
                snapshot.rows
            );
        }
        if snapshot.history.len() % snapshot.cols != 0 {
            bail!(
                "structured snapshot history has {} cells, not complete {}-column rows",
                snapshot.history.len(),
                snapshot.cols
            );
        }
        let history_rows = snapshot.history.len() / snapshot.cols;
        if history_rows > MAX_SCROLL_HISTORY_LINES {
            bail!("structured snapshot history exceeds terminal limits");
        }
        if snapshot.display_offset > history_rows {
            bail!(
                "structured snapshot display offset {} exceeds {} history rows",
                snapshot.display_offset,
                history_rows
            );
        }

        let mut bounds = self.last_content.terminal_bounds;
        bounds.bounds.size.width = bounds.cell_width * snapshot.cols as f32;
        bounds.bounds.size.height = bounds.line_height * snapshot.rows as f32;
        if bounds.num_columns() != snapshot.cols || bounds.num_lines() != snapshot.rows {
            bail!("structured snapshot dimensions cannot be represented by terminal bounds");
        }
        self.last_content.terminal_bounds = bounds;
        let term = self.term.clone();
        let mut term = term.lock_unfair();
        let history_capacity = self
            .template
            .max_scroll_history_lines
            .unwrap_or(DEFAULT_SCROLL_HISTORY_LINES)
            .min(MAX_SCROLL_HISTORY_LINES);
        apply_structured_snapshot(&mut term, snapshot, bounds, history_capacity);
        term.selection = None;
        self.last_content = make_content(&term, &self.last_content, &self.image_placements);
        drop(term);

        // A snapshot is a parser checkpoint as well as a grid checkpoint. Any
        // partial escape sequence buffered by the incremental byte path belongs
        // to the pre-snapshot stream and must not mutate the replacement grid.
        self.output_processor = Processor::<StdSyncHandler>::new();
        self.output_previous_byte_was_cr = false;

        self.selection_head = None;
        self.selection_phase = SelectionPhase::Ended;
        self.last_mouse = None;
        self.mouse_down_position = None;
        self.mouse_down_hyperlink = None;
        self.last_hyperlink_search_position = None;
        self.last_content.last_hovered_word = None;
        self.matches.clear();
        cx.emit(Event::SelectionsChanged);
        cx.emit(Event::NewNavigationTarget(None));
        cx.emit(Event::Wakeup);
        Ok(())
    }

    fn update_selected_word(
        &mut self,
        prev_word: Option<HoveredWord>,
        word_match: Range,
        word: String,
        navigation_target: MaybeNavigationTarget,
        cx: &mut Context<Self>,
    ) {
        if let Some(prev_word) = prev_word
            && prev_word.word == word
            && prev_word.word_match == word_match
        {
            self.last_content.last_hovered_word = Some(HoveredWord {
                word,
                word_match,
                id: prev_word.id,
            });
            return;
        }

        self.last_content.last_hovered_word = Some(HoveredWord {
            word,
            word_match,
            id: self.next_link_id(),
        });
        cx.emit(Event::NewNavigationTarget(Some(navigation_target)));
        cx.notify()
    }

    fn next_link_id(&mut self) -> usize {
        let res = self.next_link_id;
        self.next_link_id = self.next_link_id.wrapping_add(1);
        res
    }

    pub fn last_content(&self) -> &Content {
        &self.last_content
    }

    pub fn set_cursor_shape(&mut self, cursor_shape: SettingsCursorShape) {
        set_default_cursor_style(&mut self.term_config, cursor_shape);
        apply_config(&self.term, &self.term_config);
    }

    /// Inject non-PTY output, normalizing lone LF to CRLF across call boundaries.
    pub fn write_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let converted = convert_lf_to_crlf(bytes, &mut self.output_previous_byte_was_cr);
        self.write_emulator_bytes(&converted, cx);
    }

    /// Inject an authoritative PTY byte stream verbatim.
    ///
    /// The mux server and this DisplayOnly renderer must parse exactly the same
    /// bytes. Applying LF→CRLF normalization here would change cursor columns
    /// (`LF` preserves the column while `CRLF` resets it), causing glyph/cursor
    /// divergence from the server-owned grid.
    pub fn write_pty_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.output_previous_byte_was_cr = bytes.last().is_some_and(|byte| *byte == b'\r');
        self.write_emulator_bytes(bytes, cx);
    }

    fn write_emulator_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        // vte 会整段丢弃 APC 和未知 OSC, 所以图形协议必须在字节进入模拟器
        // 之前单独扫一遍。
        let graphics_events = self.graphics_scanner.feed(bytes);

        let mut term = self.term.lock();
        self.output_processor.advance(&mut *term, bytes);
        drop(term);
        if !graphics_events.is_empty() {
            self.apply_graphics_events(graphics_events, cx);
        }
        self.detect_init_command_startup_marker();
        cx.emit(Event::Wakeup);
    }

    pub fn total_lines(&self) -> usize {
        total_lines(&self.term.lock_unfair())
    }

    pub fn viewport_lines(&self) -> usize {
        screen_lines(&self.term.lock_unfair())
    }

    //To test:
    //- Activate match on terminal (scrolling and selection)
    //- Editor search snapping behavior

    pub fn activate_match(&mut self, index: usize) {
        if let Some(search_match) = self.matches.get(index).cloned() {
            self.set_selection(Some(Selection::simple_range(search_match)));
            if self.vi_mode_enabled {
                self.events
                    .push_back(InternalEvent::MoveViCursorToPoint(search_match.end()));
            } else {
                self.events
                    .push_back(InternalEvent::ScrollToPoint(search_match.start()));
            }
        }
    }

    pub fn select_matches(&mut self, matches: &[Range]) {
        let matches_to_select = self
            .matches
            .iter()
            .filter(|self_match| matches.contains(self_match))
            .cloned()
            .collect::<Vec<_>>();
        for match_to_select in matches_to_select {
            self.set_selection(Some(Selection::simple_range(match_to_select)));
        }
    }

    pub fn select_all(&mut self) {
        let term = self.term.lock();
        let range = full_content_range(&term);
        drop(term);
        self.set_selection(Some(Selection::simple_range(range)));
    }

    fn set_selection(&mut self, selection: Option<Selection>) {
        self.events
            .push_back(InternalEvent::SetSelection(selection));
    }

    pub fn copy(&mut self, keep_selection: Option<bool>) {
        self.events.push_back(InternalEvent::Copy(keep_selection));
    }

    pub fn clear(&mut self) {
        self.events.push_back(InternalEvent::Clear)
    }

    pub fn shrink_to_used(&mut self) {
        shrink_to_used(&mut self.term.lock());
    }

    pub fn scroll_line_up(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(Scroll::Delta(1)));
    }

    pub fn scroll_up_by(&mut self, lines: usize) {
        self.events
            .push_back(InternalEvent::Scroll(Scroll::Delta(lines as i32)));
    }

    pub fn scroll_line_down(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(Scroll::Delta(-1)));
    }

    pub fn scroll_down_by(&mut self, lines: usize) {
        self.events
            .push_back(InternalEvent::Scroll(Scroll::Delta(-(lines as i32))));
    }

    pub fn scroll_page_up(&mut self) {
        self.events.push_back(InternalEvent::Scroll(Scroll::PageUp));
    }

    pub fn scroll_page_down(&mut self) {
        self.events
            .push_back(InternalEvent::Scroll(Scroll::PageDown));
    }

    pub fn scroll_to_top(&mut self) {
        self.events.push_back(InternalEvent::Scroll(Scroll::Top));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.events.push_back(InternalEvent::Scroll(Scroll::Bottom));
    }

    /// §15.12 Scroll to an absolute display offset, clamped to scrollback history.
    ///
    /// Used by reconnect recovery to restore the server-authoritative scroll
    /// position carried in a `FullGridSnapshot`. The delta is resolved against
    /// the live `display_offset` when the event is flushed in `sync`, so it stays
    /// correct even if prior output shifted the grid.
    pub fn scroll_to_display_offset(&mut self, offset: usize) {
        self.events
            .push_back(InternalEvent::ScrollToDisplayOffset(offset));
    }

    pub fn scrolled_to_top(&self) -> bool {
        self.last_content.scrolled_to_top
    }

    pub fn scrolled_to_bottom(&self) -> bool {
        self.last_content.scrolled_to_bottom
    }

    ///Resize the terminal and the PTY.
    pub fn set_size(&mut self, new_bounds: TerminalBounds) {
        let new_bounds = normalize_terminal_bounds(new_bounds);

        let old_bounds = self.last_content.terminal_bounds;
        self.last_content.terminal_bounds = new_bounds;

        // Avoid spamming PTY resizes on pixel-level size changes (e.g. while dragging edges),
        // since those can generate excessive SIGWINCH/reflows and cause visible flicker.
        let requires_resize = old_bounds.num_lines() != new_bounds.num_lines()
            || old_bounds.num_columns() != new_bounds.num_columns()
            || old_bounds.cell_width != new_bounds.cell_width
            || old_bounds.line_height != new_bounds.line_height;

        if !requires_resize {
            return;
        }

        match self.events.back_mut() {
            Some(InternalEvent::Resize(pending_bounds)) => *pending_bounds = new_bounds,
            _ => self.events.push_back(InternalEvent::Resize(new_bounds)),
        }
    }

    /// Write the Input payload to the PTY, if applicable.
    /// (This is a no-op for display-only terminals.)
    fn write_to_pty(&self, input: impl Into<Cow<'static, [u8]>>) {
        let input = input.into();
        #[cfg(any(test, feature = "test-support"))]
        self.pty_write_log.borrow_mut().push(input.to_vec());
        if let TerminalType::Pty { pty_tx, .. } = &self.terminal_type {
            if log::log_enabled!(log::Level::Debug) {
                if let Ok(str) = str::from_utf8(&input) {
                    log::debug!("Writing to PTY: {:?}", str);
                } else {
                    log::debug!("Writing to PTY: {:?}", input);
                }
            }
            pty_tx.notify(input);
            return;
        }
        // §16.6 DisplayOnly mux panes: mouse reports / focus events must
        // still reach the server-owned PTY via the registered input sink.
        if let Some(sink) = &self.input_sink {
            sink(input.into_owned());
        }
    }

    /// Register a sink that receives bytes `write_to_pty` would otherwise
    /// drop on DisplayOnly terminals (mux mouse reporting path).
    pub fn set_input_sink(&mut self, sink: Option<std::sync::Arc<dyn Fn(Vec<u8>) + Send + Sync>>) {
        self.input_sink = sink;
    }

    pub fn input(&mut self, input: impl Into<Cow<'static, [u8]>>) {
        self.keyboard_input_sent = true;
        self.complete_init_command_startup_handshake();
        self.write_input(input);
    }

    /// Sends a shell-level marker command and returns a task that completes when
    /// the marker appears in terminal output. Already complete for non-PTY
    /// terminals or those whose child has exited.
    ///
    /// Call at most once per terminal: a second handshake drops the previous
    /// `Sender`, which would write the init command twice.
    pub fn start_init_command_startup_handshake(&mut self) -> Task<()> {
        if !self.is_pty() || self.child_exited.is_some() {
            return Task::ready(());
        }

        debug_assert!(
            self.init_command_startup_tx.is_none(),
            "start_init_command_startup_handshake called while a handshake is already in flight"
        );

        let (startup_tx, startup_rx) = async_channel::bounded(1);
        let startup_task = self.background_executor.spawn(async move {
            match startup_rx.recv().await {
                Ok(()) | Err(_) => {}
            }
        });

        let marker_id = NEXT_INIT_COMMAND_STARTUP_MARKER_ID.fetch_add(1, Ordering::Relaxed);
        self.init_command_startup_marker = Some(init_command_startup_marker(marker_id));
        self.init_command_startup_tx = Some(startup_tx);

        let shell_kind = self.template.shell.shell_kind(self.path_style.is_windows());
        let mut input = init_command_startup_marker_command(shell_kind, marker_id).into_bytes();
        input.push(b'\x0d');
        self.write_to_pty(input);

        startup_task
    }

    fn detect_init_command_startup_marker(&mut self) {
        let Some(marker) = self.init_command_startup_marker.as_deref() else {
            return;
        };

        let has_marker = {
            let term = self.term.lock_unfair();
            last_non_empty_lines(&term, INIT_COMMAND_STARTUP_MARKER_SEARCH_LINES)
                .iter()
                .any(|line| line.contains(marker))
        };

        if has_marker {
            self.complete_init_command_startup_handshake();
        }
    }

    fn complete_init_command_startup_handshake(&mut self) {
        self.init_command_startup_marker = None;
        if let Some(startup_tx) = self.init_command_startup_tx.take() {
            match startup_tx.try_send(()) {
                Ok(()) | Err(async_channel::TrySendError::Full(())) => {}
                Err(async_channel::TrySendError::Closed(())) => {}
            }
        }
    }

    /// Write a programmatically-generated command to the PTY as if it had been
    /// typed, without marking the terminal as having received user keyboard
    /// input.
    pub fn write_init_command(&mut self, input: impl Into<Cow<'static, [u8]>>) {
        self.write_input(input);
    }

    pub fn is_pty(&self) -> bool {
        matches!(self.terminal_type, TerminalType::Pty { .. })
    }

    pub fn write_init_command_after_startup(
        &mut self,
        input: impl Into<Cow<'static, [u8]>>,
        cx: &mut Context<Self>,
    ) -> bool {
        // Ends the handshake even if the marker was never seen (timeout
        // fallback), so detection stops scanning on every wakeup.
        self.complete_init_command_startup_handshake();

        if self.keyboard_input_sent || self.child_exited.is_some() {
            return false;
        }

        self.clear_for_init_command(cx);
        self.write_init_command(input);
        true
    }

    fn clear_for_init_command(&mut self, cx: &mut Context<Self>) {
        let mut term = self.term.lock_unfair();
        clear_saved_screen(&mut term);
        self.last_content = make_content(&term, &self.last_content, &self.image_placements);
        cx.emit(Event::Wakeup);
    }

    fn write_input(&mut self, input: impl Into<Cow<'static, [u8]>>) {
        self.events.push_back(InternalEvent::Scroll(Scroll::Bottom));
        self.events.push_back(InternalEvent::SetSelection(None));

        let input = input.into();
        #[cfg(any(test, feature = "test-support"))]
        self.input_log.push(input.to_vec());

        self.write_to_pty(input);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn take_input_log(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.input_log)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn take_pty_write_log(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(self.pty_write_log.get_mut())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn keyboard_input_sent(&self) -> bool {
        self.keyboard_input_sent
    }

    pub fn toggle_vi_mode(&mut self) {
        self.events.push_back(InternalEvent::ToggleViMode);
    }

    /// §12 Plan 31 — confirm a copy-mode search query and refresh the match
    /// list over the whole grid, scrollback included. Returns the number of
    /// matches, or an error when `query` is not a valid regex so callers can
    /// show the failure instead of silently browsing a stale match list.
    pub fn set_search_query(&mut self, query: &str) -> anyhow::Result<usize> {
        self.clear_search();
        anyhow::ensure!(!query.is_empty(), "search query is empty");
        let searcher = Search::new(query)
            .ok_or_else(|| anyhow::anyhow!("`{query}` is not a valid regular expression"))?;
        let matches = {
            let term = self.term.lock();
            search_matches(&term, searcher.clone())
        };
        self.matches = matches;
        self.search_state = Some(SearchState {
            query: query.to_string(),
            searcher,
        });
        Ok(self.matches.len())
    }

    /// §12 Plan 31 — the confirmed copy-mode search query, if any.
    pub fn search_query(&self) -> Option<&str> {
        self.search_state.as_ref().map(|state| state.query.as_str())
    }

    /// §12 Plan 31 — drop the copy-mode search and its highlighted matches.
    pub fn clear_search(&mut self) {
        self.search_state = None;
        self.matches.clear();
    }

    /// §12 Plan 31 — move to the next match after the cursor, wrapping around.
    pub fn search_next(&mut self) -> bool {
        self.advance_search(true)
    }

    /// §12 Plan 31 — move to the previous match before the cursor, wrapping
    /// around.
    pub fn search_previous(&mut self) -> bool {
        self.advance_search(false)
    }

    /// Matches and the cursor share absolute grid coordinates (negative lines
    /// are scrollback), so the next hit is found by ordering alone. The cursor
    /// only reaches the activated match once the queued events are flushed by
    /// `sync`, which is why stepping is derived from the cursor rather than
    /// from a remembered index.
    fn advance_search(&mut self, forward: bool) -> bool {
        // Re-run the search rather than reusing `matches`: output that arrived
        // since the query was confirmed shifts every hit in the scrollback, and
        // navigating a stale list would jump to the wrong cells.
        let Some(searcher) = self
            .search_state
            .as_ref()
            .map(|state| state.searcher.clone())
        else {
            return false;
        };
        self.matches = {
            let term = self.term.lock();
            search_matches(&term, searcher)
        };
        let match_count = self.matches.len();
        if match_count == 0 {
            return false;
        }
        let cursor = self.last_content.cursor.point;
        let index = if forward {
            self.matches
                .iter()
                .position(|search_match| search_match.start() > cursor)
                .unwrap_or(0)
        } else {
            self.matches
                .iter()
                .rposition(|search_match| search_match.end() < cursor)
                .unwrap_or(match_count - 1)
        };
        self.activate_match(index);
        true
    }

    pub fn vi_motion(&mut self, keystroke: &Keystroke) {
        if !self.vi_mode_enabled {
            return;
        }

        let key: Cow<'_, str> = if keystroke.modifiers.shift {
            Cow::Owned(keystroke.key.to_uppercase())
        } else {
            Cow::Borrowed(keystroke.key.as_str())
        };

        let motion: Option<ViMotion> = match key.as_ref() {
            "h" | "left" => Some(ViMotion::Left),
            "j" | "down" => Some(ViMotion::Down),
            "k" | "up" => Some(ViMotion::Up),
            "l" | "right" => Some(ViMotion::Right),
            "w" => Some(ViMotion::WordRight),
            "b" if !keystroke.modifiers.control => Some(ViMotion::WordLeft),
            "e" => Some(ViMotion::WordRightEnd),
            "%" => Some(ViMotion::Bracket),
            "$" => Some(ViMotion::Last),
            "0" => Some(ViMotion::First),
            "^" => Some(ViMotion::FirstOccupied),
            "H" => Some(ViMotion::High),
            "M" => Some(ViMotion::Middle),
            "L" => Some(ViMotion::Low),
            "{" => Some(ViMotion::ParagraphUp),
            "}" => Some(ViMotion::ParagraphDown),
            _ => None,
        };

        if let Some(motion) = motion {
            let cursor = self.last_content.cursor.point;
            let cursor_pos = GpuiPoint {
                x: cursor.column as f32 * self.last_content.terminal_bounds.cell_width,
                y: cursor.line as f32 * self.last_content.terminal_bounds.line_height,
            };
            self.events
                .push_back(InternalEvent::UpdateSelection(cursor_pos));
            self.events.push_back(InternalEvent::ViMotion(motion));
            return;
        }

        let scroll_motion = match key.as_ref() {
            "g" => Some(Scroll::Top),
            "G" => Some(Scroll::Bottom),
            "b" if keystroke.modifiers.control => Some(Scroll::PageUp),
            "f" if keystroke.modifiers.control => Some(Scroll::PageDown),
            "d" if keystroke.modifiers.control => {
                let amount = self.last_content.terminal_bounds.line_height().to_f64() as i32 / 2;
                Some(Scroll::Delta(-amount))
            }
            "u" if keystroke.modifiers.control => {
                let amount = self.last_content.terminal_bounds.line_height().to_f64() as i32 / 2;
                Some(Scroll::Delta(amount))
            }
            _ => None,
        };

        if let Some(scroll_motion) = scroll_motion {
            self.events.push_back(InternalEvent::Scroll(scroll_motion));
            return;
        }

        match key.as_ref() {
            "v" => {
                let point = self.last_content.cursor.point;
                let selection_type = SelectionType::Simple;
                let side = SelectionSide::Right;
                let selection = Selection::new(selection_type, point, side);
                self.events
                    .push_back(InternalEvent::SetSelection(Some(selection)));
            }

            "escape" => {
                self.events.push_back(InternalEvent::SetSelection(None));
            }

            "y" => {
                self.copy(Some(false));
            }

            "i" => {
                self.scroll_to_bottom();
                self.toggle_vi_mode();
            }

            // §12 Plan 31 — search navigation over the confirmed query.
            "n" => {
                self.search_next();
            }

            "N" => {
                self.search_previous();
            }

            "V" => {
                // §12 Plan 31 — Line selection mode (linewise visual select)
                let point = self.last_content.cursor.point;
                let selection_type = SelectionType::Lines;
                let side = SelectionSide::Left;
                let selection = Selection::new(selection_type, point, side);
                self.events
                    .push_back(InternalEvent::SetSelection(Some(selection)));
            }
            _ => {}
        }
    }

    pub fn try_keystroke(&mut self, keystroke: &Keystroke, option_as_meta: bool) -> bool {
        if self.vi_mode_enabled {
            self.vi_motion(keystroke);
            return true;
        }

        // Keep default terminal behavior
        let esc = to_esc_str(keystroke, self.last_content.mode, option_as_meta);
        if let Some(esc) = esc {
            match esc {
                Cow::Borrowed(string) => self.input(string.as_bytes()),
                Cow::Owned(string) => self.input(string.into_bytes()),
            };
            true
        } else {
            false
        }
    }

    pub fn try_modifiers_change(
        &mut self,
        modifiers: &Modifiers,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .last_content
            .terminal_bounds
            .bounds
            .contains(&window.mouse_position())
            && modifiers.secondary()
        {
            self.refresh_hovered_word(window);
        }
        cx.notify();
    }

    ///Paste text into the terminal
    pub fn paste(&mut self, text: &str) {
        let paste_text = if self.last_content.mode.contains(Modes::BRACKETED_PASTE) {
            format!("{}{}{}", "\x1b[200~", text.replace('\x1b', ""), "\x1b[201~")
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };

        self.input(paste_text.into_bytes());
    }

    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let term = self.term.clone();
        let mut terminal = term.lock_unfair();
        //Note that the ordering of events matters for event processing
        while let Some(e) = self.events.pop_front() {
            self.process_terminal_event(&e, &mut terminal, window, cx)
        }

        self.image_placements
            .retain(|placement| self.image_cache.get(placement.id).is_some());
        self.last_content = make_content(&terminal, &self.last_content, &self.image_placements);
    }

    pub fn with_renderable_cells<R>(&self, f: impl for<'a> FnOnce(RenderableCells<'a>) -> R) -> R {
        let term = self.term.lock_unfair();
        let content = term.renderable_content();
        f(RenderableCells::new(content.display_iter))
    }

    pub fn get_content(&self) -> String {
        let term = self.term.lock_unfair();
        content_text(&term)
    }

    pub fn last_n_non_empty_lines(&self, n: usize) -> Vec<String> {
        let terminal = self.term.lock_unfair();
        last_non_empty_lines(&terminal, n)
    }

    pub fn focus_in(&self) {
        if self.last_content.mode.contains(Modes::FOCUS_IN_OUT) {
            self.write_to_pty("\x1b[I".as_bytes());
        }
    }

    pub fn focus_out(&mut self) {
        if self.last_content.mode.contains(Modes::FOCUS_IN_OUT) {
            self.write_to_pty("\x1b[O".as_bytes());
        }
    }

    fn mouse_changed(&mut self, point: Point, side: SelectionSide) -> bool {
        match self.last_mouse {
            Some((old_point, old_side)) => {
                if old_point == point && old_side == side {
                    false
                } else {
                    self.last_mouse = Some((point, side));
                    true
                }
            }
            None => {
                self.last_mouse = Some((point, side));
                true
            }
        }
    }

    pub fn mouse_mode(&self, shift: bool) -> bool {
        self.last_content.mode.intersects(Modes::MOUSE_MODE) && !shift
    }

    pub fn mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        if self.mouse_mode(e.modifiers.shift) {
            // A ctrl/cmd press on a link suppressed its button-press report in
            // `mouse_down`. Since the app never saw the press, we must swallow
            // the whole gesture rather than forward later motion/release
            // reports, which would be a press-less (malformed) sequence.
            // `mouse_up` resolves it: release on the same link opens it,
            // otherwise the gesture is dropped.
            if self.mouse_down_hyperlink.is_none() {
                let (point, side) = grid_point_and_side(
                    position,
                    self.last_content.terminal_bounds,
                    self.last_content.display_offset,
                );

                if self.mouse_changed(point, side) {
                    let bytes = mouse_moved_report(
                        point,
                        e.pressed_button,
                        e.modifiers,
                        self.last_content.mode,
                    );

                    if let Some(bytes) = bytes {
                        self.write_to_pty(bytes);
                    }
                }
            }
        } else {
            self.schedule_find_hyperlink(e.modifiers, e.position);
        }
        cx.notify();
    }

    fn schedule_find_hyperlink(&mut self, modifiers: Modifiers, position: GpuiPoint<Pixels>) {
        if self.selection_phase == SelectionPhase::Selecting
            || !modifiers.secondary()
            || !self.last_content.terminal_bounds.bounds.contains(&position)
        {
            self.last_content.last_hovered_word = None;
            return;
        }

        // Throttle hyperlink searches to avoid excessive processing
        let now = Instant::now();
        if self
            .last_hyperlink_search_position
            .map_or(true, |last_pos| {
                // Only search if mouse moved significantly or enough time passed
                let distance_moved = ((position.x - last_pos.x).abs()
                    + (position.y - last_pos.y).abs())
                    > FIND_HYPERLINK_THROTTLE_PX;
                let time_elapsed = now.duration_since(self.last_mouse_move_time).as_millis() > 100;
                distance_moved || time_elapsed
            })
        {
            self.last_mouse_move_time = now;
            self.last_hyperlink_search_position = Some(position);
            self.events.push_back(InternalEvent::FindHyperlink(
                position - self.last_content.terminal_bounds.bounds.origin,
                false,
            ));
        }
    }

    pub fn select_word_at_event_position(&mut self, e: &MouseDownEvent) {
        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        let (point, side) = grid_point_and_side(
            position,
            self.last_content.terminal_bounds,
            self.last_content.display_offset,
        );
        let selection = Selection::new(SelectionType::Semantic, point, side);
        self.events
            .push_back(InternalEvent::SetSelection(Some(selection)));
    }

    pub fn mouse_drag(
        &mut self,
        e: &MouseMoveEvent,
        region: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        if !self.mouse_mode(e.modifiers.shift) {
            if let Some(hyperlink) = &self.mouse_down_hyperlink {
                let point = grid_point(
                    position,
                    self.last_content.terminal_bounds,
                    self.last_content.display_offset,
                );

                if !hyperlink.range.contains(point) {
                    self.mouse_down_hyperlink = None;
                } else {
                    return;
                }
            }

            // Ignore tiny pointer movements so that a click that jitters by a
            // pixel or two (e.g. the window-focusing click) does not begin a
            // selection. Mirrors the drag threshold used by gpui's `div`.
            if self.selection_phase != SelectionPhase::Selecting
                && let Some(mouse_down_position) = self.mouse_down_position
                && (e.position - mouse_down_position).magnitude() <= SELECTION_DRAG_THRESHOLD
            {
                return;
            }

            self.selection_phase = SelectionPhase::Selecting;
            // Alacritty has the same ordering, of first updating the selection
            // then scrolling 15ms later
            self.events
                .push_back(InternalEvent::UpdateSelection(position));

            // Doesn't make sense to scroll the alt screen
            if !self.last_content.mode.contains(Modes::ALT_SCREEN) {
                let scroll_lines = match self.drag_line_delta(e, region) {
                    Some(value) => value,
                    None => return,
                };

                self.events
                    .push_back(InternalEvent::Scroll(Scroll::Delta(scroll_lines)));
            }

            cx.notify();
        }
    }

    fn drag_line_delta(&self, e: &MouseMoveEvent, region: Bounds<Pixels>) -> Option<i32> {
        let top = region.origin.y;
        let bottom = region.bottom_left().y;

        let scroll_lines = if e.position.y < top {
            let scroll_delta = (top - e.position.y).pow(1.1);
            (scroll_delta / self.last_content.terminal_bounds.line_height).ceil() as i32
        } else if e.position.y > bottom {
            let scroll_delta = -((e.position.y - bottom).pow(1.1));
            (scroll_delta / self.last_content.terminal_bounds.line_height).floor() as i32
        } else {
            return None;
        };

        Some(scroll_lines.clamp(-3, 3))
    }

    pub fn mouse_down(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        let point = grid_point(
            position,
            self.last_content.terminal_bounds,
            self.last_content.display_offset,
        );

        if e.button == MouseButton::Left
            && e.modifiers.secondary()
            && (TerminalSettings::get_global(cx).open_links_in_mouse_mode
                || !self.mouse_mode(e.modifiers.shift))
        {
            self.mouse_down_hyperlink = self.find_hyperlink_at_point(point);

            if self.mouse_down_hyperlink.is_some() {
                return;
            }
        }

        if self.mouse_mode(e.modifiers.shift) {
            let bytes =
                mouse_button_report(point, e.button, e.modifiers, true, self.last_content.mode);

            if let Some(bytes) = bytes {
                self.write_to_pty(bytes);
            }
        } else {
            match e.button {
                MouseButton::Left => {
                    self.mouse_down_position = Some(e.position);
                    let (point, side) = grid_point_and_side(
                        position,
                        self.last_content.terminal_bounds,
                        self.last_content.display_offset,
                    );

                    let selection_type = match e.click_count {
                        0 => return, //This is a release
                        1 => Some(SelectionType::Simple),
                        2 => Some(SelectionType::Semantic),
                        3 => Some(SelectionType::Lines),
                        _ => None,
                    };

                    if selection_type == Some(SelectionType::Simple) && e.modifiers.shift {
                        self.events
                            .push_back(InternalEvent::UpdateSelection(position));
                        return;
                    }

                    let selection = selection_type
                        .map(|selection_type| Selection::new(selection_type, point, side));

                    if let Some(selection) = selection {
                        self.events
                            .push_back(InternalEvent::SetSelection(Some(selection)));
                    }
                }
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                MouseButton::Middle => {
                    if let Some(item) = cx.read_from_primary() {
                        let text = item.text().unwrap_or_default();
                        self.paste(&text);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn mouse_up(&mut self, e: &MouseUpEvent, cx: &Context<Self>) {
        let setting = TerminalSettings::get_global(cx);

        let position = e.position - self.last_content.terminal_bounds.bounds.origin;
        if let Some(mouse_down_hyperlink) = self.mouse_down_hyperlink.take() {
            let point = grid_point(
                position,
                self.last_content.terminal_bounds,
                self.last_content.display_offset,
            );

            if self
                .find_hyperlink_at_point(point)
                .is_some_and(|mouse_up_hyperlink| mouse_up_hyperlink == mouse_down_hyperlink)
            {
                self.events
                    .push_back(InternalEvent::ProcessHyperlink(mouse_down_hyperlink, true));
                self.selection_phase = SelectionPhase::Ended;
                self.last_mouse = None;
                self.mouse_down_position = None;
                return;
            }

            if self.mouse_mode(e.modifiers.shift) {
                self.selection_phase = SelectionPhase::Ended;
                self.last_mouse = None;
                self.mouse_down_position = None;
                return;
            }
        }

        if self.mouse_mode(e.modifiers.shift) {
            let point = grid_point(
                position,
                self.last_content.terminal_bounds,
                self.last_content.display_offset,
            );

            let bytes =
                mouse_button_report(point, e.button, e.modifiers, false, self.last_content.mode);

            if let Some(bytes) = bytes {
                self.write_to_pty(bytes);
            }
        } else {
            if e.button == MouseButton::Left && setting.copy_on_select {
                self.copy(Some(true));
            }

            //Hyperlinks
            if self.selection_phase == SelectionPhase::Ended {
                let mouse_cell_index =
                    content_index_for_mouse(position, &self.last_content.terminal_bounds);
                if let Some(link) = self
                    .last_content
                    .cells
                    .get(mouse_cell_index)
                    .and_then(|cell| cell.hyperlink())
                {
                    cx.open_url(link.uri());
                } else if e.modifiers.secondary() {
                    self.events
                        .push_back(InternalEvent::FindHyperlink(position, true));
                }
            }
        }

        self.selection_phase = SelectionPhase::Ended;
        self.last_mouse = None;
        self.mouse_down_position = None;
    }

    ///Scroll the terminal
    pub fn scroll_wheel(&mut self, e: &ScrollWheelEvent, scroll_multiplier: f32) {
        let mouse_mode = self.mouse_mode(e.shift);
        let scroll_multiplier = if mouse_mode { 1. } else { scroll_multiplier };

        if let Some(scroll_lines) = self.determine_scroll_lines(e, scroll_multiplier)
            && scroll_lines != 0
        {
            if mouse_mode {
                let point = grid_point(
                    e.position - self.last_content.terminal_bounds.bounds.origin,
                    self.last_content.terminal_bounds,
                    self.last_content.display_offset,
                );

                if let Some(scrolls) = scroll_report(point, scroll_lines, e, self.last_content.mode)
                {
                    for scroll in scrolls {
                        self.write_to_pty(scroll);
                    }
                };
            } else if self
                .last_content
                .mode
                .contains(Modes::ALT_SCREEN | Modes::ALTERNATE_SCROLL)
                && !e.shift
            {
                self.write_to_pty(alt_scroll(scroll_lines));
            } else {
                self.events
                    .push_back(InternalEvent::Scroll(Scroll::Delta(scroll_lines)));
            }
        }
    }

    fn refresh_hovered_word(&mut self, window: &Window) {
        self.schedule_find_hyperlink(window.modifiers(), window.mouse_position());
    }

    fn determine_scroll_lines(
        &mut self,
        e: &ScrollWheelEvent,
        scroll_multiplier: f32,
    ) -> Option<i32> {
        let line_height = self.last_content.terminal_bounds.line_height;
        match e.touch_phase {
            /* Reset scroll state on started */
            TouchPhase::Started => {
                self.scroll_px = px(0.);
                None
            }
            /* Calculate the appropriate scroll lines */
            TouchPhase::Moved => {
                let old_offset = (self.scroll_px / line_height) as i32;

                self.scroll_px += e.delta.pixel_delta(line_height).y * scroll_multiplier;

                let new_offset = (self.scroll_px / line_height) as i32;

                // Whenever we hit the edges, reset our stored scroll to 0
                // so we can respond to changes in direction quickly
                self.scroll_px %= self.last_content.terminal_bounds.height();

                Some(new_offset - old_offset)
            }
            // Cancellation does not commit a scroll, same as a plain end.
            TouchPhase::Ended | TouchPhase::Cancelled => None,
        }
    }

    pub fn find_matches(&self, searcher: Search, cx: &Context<Self>) -> Task<Vec<Range>> {
        let term = self.term.clone();
        cx.background_spawn(async move {
            let term = term.lock();
            search_matches(&term, searcher)
        })
    }

    pub fn working_directory(&self) -> Option<PathBuf> {
        if self.is_remote_terminal {
            // We can't yet reliably detect the working directory of a shell on the
            // SSH host. Until we can do that, it doesn't make sense to display
            // the working directory on the client and persist that.
            None
        } else {
            self.client_side_working_directory()
        }
    }

    /// Normalizes the command name of the foreground process, if one is known.
    pub fn foreground_process_command_name(&self) -> Option<String> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => info
                .current
                .read()
                .as_ref()
                .and_then(|process| foreground_process_command_from_argv(&process.argv)),
            TerminalType::DisplayOnly => None,
        }
    }

    /// Returns the working directory of the process that's connected to the PTY.
    /// That means it returns the working directory of the local shell or program
    /// that's running inside the terminal.
    ///
    /// This does *not* return the working directory of the shell that runs on the
    /// remote host, in case Zed is connected to a remote host.
    fn client_side_working_directory(&self) -> Option<PathBuf> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => info
                .current
                .read()
                .as_ref()
                .map(|process| process.cwd.clone()),
            TerminalType::DisplayOnly => None,
        }
    }

    /// Set the title supplied by a server-authoritative display-only pane.
    pub fn set_display_title(&mut self, title: String, cx: &mut Context<Self>) {
        if self.title_override.as_deref() != Some(title.as_str()) {
            self.title_override = Some(title);
            cx.notify();
        }
    }

    pub fn title(&self, truncate: bool) -> String {
        const MAX_CHARS: usize = 25;
        match &self.task {
            Some(task_state) => {
                if truncate {
                    truncate_and_trailoff(&task_state.spawned_task.label, MAX_CHARS)
                } else {
                    task_state.spawned_task.full_label.clone()
                }
            }
            None => self
                .title_override
                .as_ref()
                .map(|title_override| title_override.to_string())
                .unwrap_or_else(|| match &self.terminal_type {
                    TerminalType::Pty { info, .. } => info
                        .current
                        .read()
                        .as_ref()
                        .map(|fpi| {
                            let process_file = fpi
                                .cwd
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();

                            let argv = fpi.argv.as_slice();
                            let process_name = format!(
                                "{}{}",
                                fpi.name,
                                if !argv.is_empty() {
                                    format!(" {}", (argv[1..]).join(" "))
                                } else {
                                    "".to_string()
                                }
                            );
                            let (process_file, process_name) = if truncate {
                                (
                                    truncate_and_trailoff(&process_file, MAX_CHARS),
                                    truncate_and_trailoff(&process_name, MAX_CHARS),
                                )
                            } else {
                                (process_file, process_name)
                            };
                            format!("{process_file} — {process_name}")
                        })
                        .unwrap_or_else(|| "Terminal".to_string()),
                    TerminalType::DisplayOnly => "Terminal".to_string(),
                }),
        }
    }

    pub fn kill_active_task(&mut self) {
        if let Some(task) = self.task()
            && task.status == TaskStatus::Running
        {
            match &self.terminal_type {
                TerminalType::Pty { info, .. } => {
                    // First kill the foreground process group (the command running in the shell)
                    info.kill_current_process();
                    // Then kill the shell itself so that the terminal exits properly
                    // and wait_for_completed_task can complete
                    info.kill_child_process();
                }
                TerminalType::DisplayOnly => {
                    // Non-PTY task terminals own their subprocess directly.
                    if let Some(subprocess) = &self.subprocess {
                        subprocess.kill();
                    }
                }
            }
        }
    }

    pub fn pid(&self) -> Option<sysinfo::Pid> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => info.pid(),
            TerminalType::DisplayOnly => None,
        }
    }

    pub fn pid_getter(&self) -> Option<&ProcessIdGetter> {
        match &self.terminal_type {
            TerminalType::Pty { info, .. } => Some(info.pid_getter()),
            TerminalType::DisplayOnly => None,
        }
    }

    pub fn task(&self) -> Option<&TaskState> {
        self.task.as_ref()
    }

    pub fn wait_for_completed_task(&self, cx: &App) -> Task<Option<ExitStatus>> {
        if let Some(task) = self.task() {
            if task.status == TaskStatus::Running {
                let completion_receiver = task.completion_rx.clone();
                return cx.spawn(async move |_| completion_receiver.recv().await.ok().flatten());
            } else if let Ok(status) = task.completion_rx.try_recv() {
                return Task::ready(status);
            }
        }
        Task::ready(None)
    }

    fn register_task_finished(
        &mut self,
        exit_status: Option<ExitStatus>,
        cx: &mut Context<Terminal>,
    ) {
        if let Some(tx) = &self.completion_tx {
            tx.try_send(exit_status).ok();
        }
        if let Some(e) = exit_status {
            self.child_exited = Some(e);
        }
        self.complete_init_command_startup_handshake();
        let task = match &mut self.task {
            Some(task) => task,
            None => {
                // For interactive shells (no task), we need to differentiate:
                // 1. User-initiated exits (typed "exit", Ctrl+D, etc.) - always close,
                //    even if the shell exits with a non-zero code (e.g. after `false`).
                // 2. Shell spawn failures (bad $SHELL) - don't close, so the user sees
                //    the error. Spawn failures never receive keyboard input.
                let should_close = if self.keyboard_input_sent {
                    true
                } else {
                    self.child_exited.is_none_or(|e| e.code() == Some(0))
                };
                if should_close {
                    cx.emit(Event::CloseTerminal);
                }
                return;
            }
        };
        if task.status != TaskStatus::Running {
            return;
        }
        match exit_status.and_then(|e| e.code()) {
            Some(error_code) => {
                task.status.register_task_exit(error_code);
            }
            None => {
                task.status.register_terminal_exit();
            }
        };

        let (finished_successfully, task_line, command_line) = task_summary(task, exit_status);
        let mut lines_to_show = Vec::new();
        if task.spawned_task.show_summary {
            lines_to_show.push(task_line.as_str());
        }
        if task.spawned_task.show_command {
            lines_to_show.push(command_line.as_str());
        }
        let hide = task.spawned_task.hide;

        if !lines_to_show.is_empty() {
            // SAFETY: the invocation happens on non `TaskStatus::Running` tasks, once,
            // after either `AlacTermEvent::Exit` or `AlacTermEvent::ChildExit` events that are spawned
            // when Zed task finishes and no more output is made.
            // After the task summary is output once, no more text is appended to the terminal.
            unsafe { append_text_to_term(&mut self.term.lock(), &lines_to_show) };
        }

        match hide {
            HideStrategy::Never => {}
            HideStrategy::Always => {
                cx.emit(Event::CloseTerminal);
            }
            HideStrategy::OnSuccess => {
                if finished_successfully {
                    cx.emit(Event::CloseTerminal);
                }
            }
        }
    }

    pub fn vi_mode_enabled(&self) -> bool {
        self.vi_mode_enabled
    }

    pub fn clone_builder(&self, cx: &App, cwd: Option<PathBuf>) -> Task<Result<TerminalBuilder>> {
        let working_directory = self.working_directory().or_else(|| cwd);
        TerminalBuilder::new(
            working_directory,
            None,
            self.template.shell.clone(),
            self.template.env.clone(),
            self.template.cursor_shape,
            self.template.alternate_scroll,
            self.template.max_scroll_history_lines,
            self.template.path_hyperlink_regexes.clone(),
            self.template.path_hyperlink_timeout_ms,
            self.is_remote_terminal,
            self.template.window_id,
            None,
            cx,
            self.activation_script.clone(),
            self.path_style,
        )
    }

    /// 获取图像缓存引用
    pub fn image_cache(&self) -> &kitty_graphics::PaneImageCache {
        &self.image_cache
    }

    /// 当前有效的图像放置。
    pub fn image_placements(&self) -> &[ImagePlacement] {
        &self.image_placements
    }

    /// 执行扫描器解析出来的图形协议动作。
    ///
    /// §11.2 kitty graphics / iTerm2 OSC 1337 动作落地
    fn apply_graphics_events(
        &mut self,
        events: Vec<kitty_graphics::GraphicsEvent>,
        cx: &mut Context<Self>,
    ) {
        use kitty_graphics::GraphicsEvent;

        let mut changed = false;
        for event in events {
            match event {
                GraphicsEvent::Transmit {
                    image_id,
                    image_number,
                    image,
                } => {
                    self.image_cache.insert(image, image_id, image_number);
                }
                GraphicsEvent::Place(request) => changed |= self.place_image(request),
                GraphicsEvent::Delete(request) => changed |= self.delete_images(request),
                GraphicsEvent::Respond(bytes) => self.write_to_pty(bytes),
            }
        }

        // 从缓存里淘汰掉的图像还留在 GPUI 的纹理图集里, 必须显式释放。
        for image in self.image_cache.take_dropped_images() {
            cx.drop_image(image, None);
        }

        if changed {
            cx.emit(Event::Wakeup);
            cx.notify();
        }
    }

    fn place_image(&mut self, request: kitty_graphics::PlacementRequest) -> bool {
        let id = match request.image_id {
            Some(client_id) => self.image_cache.resolve_client_id(client_id),
            None => self.image_cache.last_transmitted(),
        };
        let Some(id) = id else {
            return false;
        };
        let Some(cached) = self.image_cache.get(id) else {
            return false;
        };
        let (pixel_width, pixel_height) = cached.image.pixel_size;

        let bounds = self.last_content.terminal_bounds;
        let cell_width = f32::from(bounds.cell_width()).max(1.0);
        let line_height = f32::from(bounds.line_height()).max(1.0);
        let (columns, rows) = resolve_image_cell_size(
            request.columns,
            request.rows,
            (pixel_width as f32, pixel_height as f32),
            (cell_width, line_height),
            (bounds.num_columns(), bounds.num_lines()),
        );
        if columns == 0 || rows == 0 {
            return false;
        }

        // 放置点取"事件被执行时光标在哪儿"。扫描器在 PTY 读取线程上先于
        // alacritty 看到字节, 但事件要跨线程送到这里才执行, 那时同一批字节
        // 里图形序列之后的文本可能已经被模拟器消化掉了, 所以这是个近似值。
        // 要做到逐格精确需要 kitty 的 Unicode placeholder 方案, 把图像锚在
        // 真实网格单元上。
        let (anchor_line, column) = {
            let term = self.term.lock_unfair();
            cursor_anchor(&term)
        };

        self.image_cache.touch(id);
        self.image_placements.push(ImagePlacement {
            id,
            anchor_line,
            column,
            columns,
            rows,
            z_index: request.z_index,
        });
        if self.image_placements.len() > MAX_IMAGE_PLACEMENTS {
            let excess = self.image_placements.len() - MAX_IMAGE_PLACEMENTS;
            self.image_placements.drain(..excess);
        }
        true
    }

    fn delete_images(&mut self, request: kitty_graphics::DeleteRequest) -> bool {
        use kitty_graphics::DeleteScope;

        let removed: Vec<kitty_graphics::ImageId> = match request.scope {
            DeleteScope::All => self.image_placements.iter().map(|p| p.id).collect(),
            DeleteScope::ImageId(client_id) => self
                .image_cache
                .resolve_client_id(client_id)
                .into_iter()
                .collect(),
            DeleteScope::ImageNumber(image_number) => self
                .image_cache
                .resolve_image_number(image_number)
                .into_iter()
                .collect(),
        };

        if removed.is_empty() && !matches!(request.scope, DeleteScope::All) {
            return false;
        }

        let placements_before = self.image_placements.len();
        self.image_placements
            .retain(|placement| !removed.contains(&placement.id));

        if request.free_data {
            match request.scope {
                DeleteScope::All => self.image_cache.clear(),
                _ => {
                    for id in removed {
                        self.image_cache.remove(id);
                    }
                }
            }
        }

        placements_before != self.image_placements.len()
    }
}

/// 把协议里的尺寸描述换算成单元格数。
///
/// 只给出一个方向时按图像原始宽高比补另一个方向, 两个都没给就按图像像素
/// 尺寸铺满对应的格子数。
fn resolve_image_cell_size(
    requested_columns: kitty_graphics::ImageDimension,
    requested_rows: kitty_graphics::ImageDimension,
    pixel_size: (f32, f32),
    cell_size: (f32, f32),
    grid_size: (usize, usize),
) -> (usize, usize) {
    use kitty_graphics::ImageDimension;

    let (pixel_width, pixel_height) = pixel_size;
    let (cell_width, line_height) = cell_size;
    let (grid_columns, grid_rows) = grid_size;

    let resolve = |dimension: ImageDimension, unit: f32, available: usize| {
        let cells = match dimension {
            ImageDimension::Auto => return None,
            ImageDimension::Cells(cells) => cells as usize,
            ImageDimension::Pixels(value) => ((value as f32) / unit).ceil() as usize,
            ImageDimension::Percent(percent) => {
                ((available as f32) * (percent as f32) / 100.0).ceil() as usize
            }
        };
        Some(cells.clamp(1, available.max(1)))
    };

    let columns = resolve(requested_columns, cell_width, grid_columns);
    let rows = resolve(requested_rows, line_height, grid_rows);

    let natural_columns =
        ((pixel_width / cell_width).ceil() as usize).clamp(1, grid_columns.max(1));
    let natural_rows = ((pixel_height / line_height).ceil() as usize).clamp(1, grid_rows.max(1));

    match (columns, rows) {
        (Some(columns), Some(rows)) => (columns, rows),
        (Some(columns), None) => {
            let scale = (columns as f32 * cell_width) / pixel_width.max(1.0);
            let rows = ((pixel_height * scale) / line_height).ceil() as usize;
            (columns, rows.clamp(1, grid_rows.max(1)))
        }
        (None, Some(rows)) => {
            let scale = (rows as f32 * line_height) / pixel_height.max(1.0);
            let columns = ((pixel_width * scale) / cell_width).ceil() as usize;
            (columns.clamp(1, grid_columns.max(1)), rows)
        }
        (None, None) => (natural_columns, natural_rows),
    }
}

const TASK_DELIMITER: &str = "⏵ ";
fn task_summary(task: &TaskState, exit_status: Option<ExitStatus>) -> (bool, String, String) {
    let escaped_full_label = task
        .spawned_task
        .full_label
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    let task_label = |suffix: &str| format!("{TASK_DELIMITER}Task `{escaped_full_label}` {suffix}");
    let (success, task_line) = match exit_status {
        Some(status) => {
            let code = status.code();
            #[cfg(unix)]
            let signal = status.signal();
            #[cfg(not(unix))]
            let signal: Option<i32> = None;

            match (code, signal) {
                (Some(0), _) => (true, task_label("finished successfully")),
                (Some(code), _) => (
                    false,
                    task_label(&format!("finished with exit code: {code}")),
                ),
                (None, Some(signal)) => (
                    false,
                    task_label(&format!("terminated by signal: {signal}")),
                ),
                (None, None) => (false, task_label("finished")),
            }
        }
        None => (false, task_label("finished")),
    };
    let escaped_command_label = task
        .spawned_task
        .command_label
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    let command_line = format!("{TASK_DELIMITER}Command: {escaped_command_label}");
    (success, task_line, command_line)
}

/// Converts bare LFs into CRLFs so output captured from a pipe (rather than a
/// PTY) wraps correctly in Alacritty. A PTY's line discipline performs this
/// `ONLCR` translation for us; piped output (e.g. `ls` run outside a PTY) only
/// emits `\n`, which moves Alacritty's cursor down without returning it to
/// column zero and makes the rendered output look misaligned. Alacritty has no
/// setting for this, so we insert a `\r` before each `\n` that lacks one.
fn convert_lf_to_crlf(bytes: &[u8], previous_byte_was_cr: &mut bool) -> Vec<u8> {
    let mut converted = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if byte == b'\n' && !*previous_byte_was_cr {
            converted.push(b'\r');
        }
        converted.push(byte);
        *previous_byte_was_cr = byte == b'\r';
    }
    converted
}

/// Owns a non-PTY task subprocess and the background task pumping its output
/// into the terminal emulator. Used by headless hosts (e.g. the eval CLI) where
/// PTY allocation fails with `ENOTTY`. Dropping this kills the child.
struct SubprocessHandle {
    child: Arc<parking_lot::Mutex<Option<util::process::Child>>>,
    _reader: Task<()>,
}

impl SubprocessHandle {
    fn kill(&self) {
        if let Some(child) = self.child.lock().as_mut() {
            child.kill().log_err();
        }
    }
}

/// Spawns `program`/`args` as a plain subprocess with piped stdout/stderr and
/// drives its output into `term`, mirroring what the Alacritty event loop does
/// for a PTY but without one. Used when [`HeadlessTerminal`] is enabled.
fn spawn_task_subprocess(
    program: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    working_directory: Option<PathBuf>,
    term: Arc<AlacrittyTermLock>,
    events_tx: futures::channel::mpsc::UnboundedSender<PtyEvent>,
    executor: &BackgroundExecutor,
) -> Result<SubprocessHandle> {
    use futures::io::AsyncReadExt as _;
    use std::process::Stdio;

    let mut command = util::command::new_std_command(&program);
    command.args(&args);
    command.envs(&env);
    if let Some(directory) = &working_directory {
        command.current_dir(directory);
    }

    let mut child =
        util::process::Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(parking_lot::Mutex::new(Some(child)));

    let reader = executor.spawn({
        let child = child.clone();
        let executor = executor.clone();
        async move {
            // stdout and stderr are pumped concurrently, each through its own
            // parser; the shared term mutex serializes grid mutation.
            type BoxedReader = Box<dyn futures::io::AsyncRead + Unpin + Send>;
            let pump = |reader: Option<BoxedReader>| {
                let term = term.clone();
                let events_tx = events_tx.clone();
                async move {
                    let Some(mut reader) = reader else { return };
                    let mut processor = Processor::<StdSyncHandler>::new();
                    let mut scanner = kitty_graphics::GraphicsScanner::new();
                    let mut buffer = [0u8; 8192];
                    let mut previous_byte_was_cr = false;
                    loop {
                        match reader.read(&mut buffer).await {
                            Ok(0) => return,
                            Err(error) => {
                                log::warn!("failed to read subprocess output: {error}");
                                return;
                            }
                            Ok(count) => {
                                let converted =
                                    convert_lf_to_crlf(&buffer[..count], &mut previous_byte_was_cr);
                                let graphics_events = scanner.feed(&converted);
                                {
                                    let mut term = term.lock();
                                    processor.advance(&mut *term, &converted);
                                }
                                if !graphics_events.is_empty() {
                                    events_tx
                                        .unbounded_send(PtyEvent::Graphics(graphics_events))
                                        .ok();
                                }
                                events_tx
                                    .unbounded_send(PtyEvent::Event(TerminalBackendEvent::Wakeup))
                                    .ok();
                            }
                        }
                    }
                }
            };
            let stdout = stdout.map(|reader| Box::new(reader) as BoxedReader);
            let stderr = stderr.map(|reader| Box::new(reader) as BoxedReader);
            futures::future::join(pump(stdout), pump(stderr)).await;

            // Both pipes are closed, so the child has exited or is about to.
            // Poll for its status without holding the lock across an await.
            let status = loop {
                let status = match child.lock().as_mut() {
                    Some(child) => match child.try_status() {
                        Ok(status) => status,
                        Err(error) => {
                            log::warn!("failed to get subprocess exit status: {error}");
                            break None;
                        }
                    },
                    None => Some(ExitStatus::default()),
                };
                match status {
                    Some(status) => break Some(status),
                    None => executor.timer(Duration::from_millis(20)).await,
                }
            };
            child.lock().take();
            let event = match status {
                Some(status) => TerminalBackendEvent::ChildExit(status),
                None => TerminalBackendEvent::Exit,
            };
            events_tx.unbounded_send(PtyEvent::Event(event)).ok();
        }
    });

    Ok(SubprocessHandle {
        child,
        _reader: reader,
    })
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(subprocess) = self.subprocess.take() {
            subprocess.kill();
        }
        if let TerminalType::Pty { pty_tx, info } =
            std::mem::replace(&mut self.terminal_type, TerminalType::DisplayOnly)
        {
            pty_tx.shutdown();
            info.terminate_child_process();

            let timer = self.background_executor.timer(Duration::from_millis(100));
            self.background_executor
                .spawn(async move {
                    timer.await;
                    info.kill_child_process();
                })
                .detach();
        }
    }
}

impl EventEmitter<Event> for Terminal {}

fn normalize_path_command_name(command: &str) -> Option<String> {
    const MAX_COMMAND_NAME_LENGTH: usize = 64;

    let command = command.trim();
    if command.is_empty()
        || command.len() > MAX_COMMAND_NAME_LENGTH
        || command.starts_with('.')
        || command.starts_with('-')
        || command.contains('/')
        || command.contains('\\')
    {
        return None;
    }

    let mut command = command.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if command.ends_with(suffix) {
            command.truncate(command.len() - suffix.len());
            break;
        }
    }

    if command.is_empty()
        || !command.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }

    Some(command)
}

fn foreground_process_command_from_argv(argv: &[String]) -> Option<String> {
    let command = argv
        .first()
        .and_then(|command| normalize_path_command_name(command));

    if !matches!(
        command.as_deref(),
        Some("node" | "python" | "python3" | "bun" | "deno")
    ) {
        return command;
    }

    argv.iter()
        .skip(1)
        .filter_map(|argument| normalize_script_command_name(argument))
        .next()
        .or(command)
}

fn normalize_script_command_name(argument: &str) -> Option<String> {
    let path = Path::new(argument);
    let file_stem = path
        .file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .and_then(normalize_path_command_name)?;

    if file_stem != "index" {
        return Some(file_stem);
    }

    path.parent()
        .and_then(|parent| parent.parent())
        .and_then(|package_path| package_path.file_name())
        .and_then(|package_name| package_name.to_str())
        .and_then(|package_name| package_name.strip_suffix("-cli").or(Some(package_name)))
        .and_then(normalize_path_command_name)
}

fn content_index_for_mouse(pos: GpuiPoint<Pixels>, terminal_bounds: &TerminalBounds) -> usize {
    let col = (pos.x / terminal_bounds.cell_width()).round() as usize;
    let clamped_col = min(col, terminal_bounds.num_columns().saturating_sub(1));
    let row = (pos.y / terminal_bounds.line_height()).round() as usize;
    let clamped_row = min(row, terminal_bounds.num_lines().saturating_sub(1));
    clamped_row * terminal_bounds.num_columns() + clamped_col
}

/// Converts an 8 bit ANSI color to its GPUI equivalent.
/// Accepts `usize` for compatibility with the `alacritty::Colors` interface,
/// Other than that use case, should only be called with values in the `[0,255]` range
pub fn get_color_at_index(index: usize, theme: &Theme) -> Hsla {
    let colors = theme.colors();

    match index {
        // 0-15 are the same as the named colors above
        0 => colors.terminal_ansi_black,
        1 => colors.terminal_ansi_red,
        2 => colors.terminal_ansi_green,
        3 => colors.terminal_ansi_yellow,
        4 => colors.terminal_ansi_blue,
        5 => colors.terminal_ansi_magenta,
        6 => colors.terminal_ansi_cyan,
        7 => colors.terminal_ansi_white,
        8 => colors.terminal_ansi_bright_black,
        9 => colors.terminal_ansi_bright_red,
        10 => colors.terminal_ansi_bright_green,
        11 => colors.terminal_ansi_bright_yellow,
        12 => colors.terminal_ansi_bright_blue,
        13 => colors.terminal_ansi_bright_magenta,
        14 => colors.terminal_ansi_bright_cyan,
        15 => colors.terminal_ansi_bright_white,
        // 16-231 are a 6x6x6 RGB color cube, mapped to 0-255 using steps defined by XTerm.
        // See: https://github.com/xterm-x11/xterm-snapshots/blob/master/256colres.pl
        16..=231 => {
            let (r, g, b) = rgb_for_index(index as u8);
            rgba_color(
                if r == 0 { 0 } else { r * 40 + 55 },
                if g == 0 { 0 } else { g * 40 + 55 },
                if b == 0 { 0 } else { b * 40 + 55 },
            )
        }
        // 232-255 are a 24-step grayscale ramp from (8, 8, 8) to (238, 238, 238).
        232..=255 => {
            let i = index as u8 - 232; // Align index to 0..24
            let value = i * 10 + 8;
            rgba_color(value, value, value)
        }
        // For compatibility with the alacritty::Colors interface
        // See: https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/term/color.rs
        256 => colors.terminal_foreground,
        257 => colors.terminal_background,
        258 => theme.players().local().cursor,
        259 => colors.terminal_ansi_dim_black,
        260 => colors.terminal_ansi_dim_red,
        261 => colors.terminal_ansi_dim_green,
        262 => colors.terminal_ansi_dim_yellow,
        263 => colors.terminal_ansi_dim_blue,
        264 => colors.terminal_ansi_dim_magenta,
        265 => colors.terminal_ansi_dim_cyan,
        266 => colors.terminal_ansi_dim_white,
        267 => colors.terminal_bright_foreground,
        268 => colors.terminal_ansi_black, // 'Dim Background', non-standard color

        _ => black(),
    }
}

/// Generates the RGB channels in [0, 5] for a given index into the 6x6x6 ANSI color cube.
///
/// See: [8 bit ANSI color](https://en.wikipedia.org/wiki/ANSI_escape_code#8-bit).
///
/// Wikipedia gives a formula for calculating the index for a given color:
///
/// ```text
/// index = 16 + 36 × r + 6 × g + b (0 ≤ r, g, b ≤ 5)
/// ```
///
/// This function does the reverse, calculating the `r`, `g`, and `b` components from a given index.
fn rgb_for_index(i: u8) -> (u8, u8, u8) {
    debug_assert!((16..=231).contains(&i));
    let i = i - 16;
    let r = (i - (i % 36)) / 36;
    let g = ((i % 36) - (i % 6)) / 6;
    let b = (i % 36) % 6;
    (r, g, b)
}

pub fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    Rgba {
        r: (r as f32 / 255.),
        g: (g as f32 / 255.),
        b: (b as f32 / 255.),
        a: 1.,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        Cell, Content, IndexedCell, TerminalBounds, TerminalBuilder, content_index_for_mouse,
        rgb_for_index,
    };
    use async_channel::Receiver;
    use collections::HashMap;
    use gpui::MouseMoveEvent;
    use gpui::{
        ClipboardItem, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
        TestAppContext, bounds, point, size,
    };
    use parking_lot::Mutex;
    use rand::{Rng, distr, rngs::StdRng};
    use util::shell::{Shell, ShellKind};
    use util::shell_builder::ShellBuilder;

    #[gpui::test]
    async fn display_only_raw_pty_output_preserves_lf_cursor_column(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        terminal.update(cx, |terminal, cx| {
            terminal.write_pty_output(b"abc\nX", cx);
            let cursor = terminal.term.lock().grid().cursor.point;
            assert_eq!(cursor.line.0, 1);
            assert_eq!(cursor.column.0, 4);
        });
    }

    #[test]
    fn test_init_command_startup_marker_commands_do_not_contain_marker() {
        let marker_id = 42;
        let marker = init_command_startup_marker(marker_id);

        for shell_kind in [
            ShellKind::Posix,
            ShellKind::Csh,
            ShellKind::Tcsh,
            ShellKind::Rc,
            ShellKind::Fish,
            ShellKind::PowerShell,
            ShellKind::Pwsh,
            ShellKind::Nushell,
            ShellKind::Cmd,
            ShellKind::Xonsh,
            ShellKind::Elvish,
        ] {
            let command = init_command_startup_marker_command(shell_kind, marker_id);
            assert!(
                !command.contains(&marker),
                "startup marker command for {shell_kind:?} should not contain the full marker, got {command:?}"
            );
        }
    }

    #[gpui::test]
    async fn test_init_command_startup_marker_ignores_echoed_command(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let marker_id = 4242;
        let marker = init_command_startup_marker(marker_id);
        let command = init_command_startup_marker_command(ShellKind::Posix, marker_id);
        let (startup_tx, startup_rx) = async_channel::bounded(1);

        terminal.update(cx, |terminal, cx| {
            terminal.init_command_startup_marker = Some(marker.clone());
            terminal.init_command_startup_tx = Some(startup_tx);
            terminal.write_output(command.as_bytes(), cx);
        });
        assert!(matches!(
            startup_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(marker.as_bytes(), cx);
        });
        assert!(startup_rx.try_recv().is_ok());
    }

    #[gpui::test]
    async fn display_only_structured_snapshot_preserves_render_state(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let styled = StructuredTerminalCell {
            character: 'A',
            zerowidth: vec!['\u{301}'],
            foreground: Rgb { r: 1, g: 2, b: 3 },
            background: Rgb { r: 4, g: 5, b: 6 },
            bold: true,
            italic: true,
            underline: StructuredUnderlineStyle::Curly,
            underline_color: Some(Rgb { r: 7, g: 8, b: 9 }),
            strikethrough: true,
            dim: true,
            reverse: true,
            wide_char: true,
            wrapline: true,
            hyperlink: Some(Hyperlink::new(
                Some("link-id"),
                "https://example.com".to_string(),
            )),
            ..Default::default()
        };
        let alternate = StructuredTerminalSnapshot {
            cols: 2,
            rows: 1,
            cells: vec![styled.clone(), StructuredTerminalCell::default()],
            history: Vec::new(),
            display_offset: 0,
            cursor: Some(StructuredTerminalCursor {
                point: Point::new(0, 1),
                shape: CursorShape::Underline,
                visible: true,
                blinking: true,
            }),
            alternate_screen: true,
            modes: Modes::ALT_SCREEN | Modes::APP_CURSOR | Modes::BRACKETED_PASTE,
        };

        let applied = terminal.update(cx, |terminal, cx| {
            terminal.apply_structured_snapshot(&alternate, cx)
        });
        if let Err(error) = applied {
            panic!("apply alternate structured snapshot: {error}");
        }
        terminal.read_with(cx, |terminal, _cx| {
            let content = terminal.last_content();
            assert!(content.mode.contains(Modes::ALT_SCREEN));
            assert_eq!(content.cursor.point, Point::new(0, 1));
            assert_eq!(content.cursor.shape, CursorShape::Underline);
            assert_eq!(content.terminal_bounds.num_columns(), 2);
            assert_eq!(content.terminal_bounds.num_lines(), 1);
            let cell = content
                .cells
                .iter()
                .find(|cell| cell.point == Point::new(0, 0))
                .unwrap_or_else(|| panic!("structured cell missing from render content"));
            assert_eq!(cell.character(), 'A');
            assert_eq!(cell.foreground(), Color::Spec(styled.foreground));
            assert_eq!(cell.background(), Color::Spec(styled.background));
            assert!(cell.is_bold());
            assert!(cell.is_italic());
            assert!(cell.has_underline());
            assert!(cell.has_strikeout());
            assert!(cell.is_dim());
            assert!(cell.is_inverse());
            assert_eq!(cell.zerowidth(), Some(['\u{301}'].as_slice()));
            assert!(cell.has_undercurl());
            assert_eq!(
                cell.underline_color(),
                Some(Color::Spec(Rgb { r: 7, g: 8, b: 9 }))
            );
            assert!(!cell.is_hidden());
            let hyperlink = cell
                .hyperlink()
                .unwrap_or_else(|| panic!("structured hyperlink missing"));
            assert_eq!(hyperlink.id(), Some("link-id"));
            assert_eq!(hyperlink.uri(), "https://example.com");
            assert!(content.mode.contains(Modes::APP_CURSOR));
            assert!(content.mode.contains(Modes::BRACKETED_PASTE));
        });

        let primary_hidden = StructuredTerminalSnapshot {
            cols: 1,
            rows: 1,
            cells: vec![StructuredTerminalCell {
                character: 'P',
                ..Default::default()
            }],
            history: Vec::new(),
            display_offset: 0,
            cursor: Some(StructuredTerminalCursor {
                point: Point::new(0, 0),
                shape: CursorShape::Bar,
                visible: false,
                blinking: false,
            }),
            alternate_screen: false,
            modes: Modes::NONE,
        };
        let applied = terminal.update(cx, |terminal, cx| {
            terminal.apply_structured_snapshot(&primary_hidden, cx)
        });
        if let Err(error) = applied {
            panic!("apply primary structured snapshot: {error}");
        }
        terminal.read_with(cx, |terminal, _cx| {
            let content = terminal.last_content();
            assert!(!content.mode.contains(Modes::ALT_SCREEN));
            assert_eq!(content.cursor.shape, CursorShape::Hidden);
            assert_eq!(content.cursor.point, Point::new(0, 0));
            assert_eq!(content.cells[0].character(), 'P');
        });
    }

    #[gpui::test]
    async fn structured_snapshot_reconstructs_history_and_display_offset(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                Some(10),
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let cell = |character| StructuredTerminalCell {
            character,
            ..Default::default()
        };
        let snapshot = StructuredTerminalSnapshot {
            cols: 2,
            rows: 2,
            cells: vec![cell('C'), cell(' '), cell('D'), cell(' ')],
            history: vec![cell('A'), cell(' '), cell('B'), cell(' ')],
            display_offset: 2,
            cursor: Some(StructuredTerminalCursor {
                point: Point::new(1, 0),
                shape: CursorShape::Block,
                visible: true,
                blinking: false,
            }),
            alternate_screen: false,
            modes: Modes::SHOW_CURSOR,
        };

        let applied = terminal.update(cx, |terminal, cx| {
            terminal.apply_structured_snapshot(&snapshot, cx)
        });
        if let Err(error) = applied {
            panic!("apply structured history snapshot: {error}");
        }
        terminal.read_with(cx, |terminal, _cx| {
            assert_eq!(terminal.total_lines(), 4);
            let content = terminal.last_content();
            assert_eq!(content.display_offset, 2);
            assert_eq!(content.cells[0].character(), 'A');
            assert_eq!(content.cells[2].character(), 'B');
            assert_eq!(content.cursor.point, Point::new(1, 0));
        });
    }

    #[gpui::test]
    async fn structured_snapshot_clears_stale_history(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                Some(10),
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let cell = |character| StructuredTerminalCell {
            character,
            ..Default::default()
        };
        let with_history = StructuredTerminalSnapshot {
            cols: 1,
            rows: 2,
            cells: vec![cell('C'), cell('D')],
            history: vec![cell('A'), cell('B')],
            display_offset: 2,
            cursor: None,
            alternate_screen: false,
            modes: Modes::NONE,
        };
        terminal
            .update(cx, |terminal, cx| {
                terminal.apply_structured_snapshot(&with_history, cx)
            })
            .unwrap_or_else(|error| panic!("apply history snapshot: {error}"));
        assert_eq!(
            terminal.read_with(cx, |terminal, _cx| terminal.total_lines()),
            4
        );

        let without_history = StructuredTerminalSnapshot {
            cols: 1,
            rows: 2,
            cells: vec![cell('X'), cell('Y')],
            history: Vec::new(),
            display_offset: 0,
            cursor: None,
            alternate_screen: false,
            modes: Modes::NONE,
        };
        terminal
            .update(cx, |terminal, cx| {
                terminal.apply_structured_snapshot(&without_history, cx)
            })
            .unwrap_or_else(|error| panic!("apply empty-history snapshot: {error}"));

        terminal.read_with(cx, |terminal, _cx| {
            assert_eq!(terminal.total_lines(), 2);
            assert_eq!(terminal.last_content().display_offset, 0);
            assert_eq!(terminal.last_content().cells[0].character(), 'X');
            assert_eq!(terminal.last_content().cells[1].character(), 'Y');
        });
    }

    #[gpui::test]
    async fn structured_snapshot_reconstructs_history_larger_than_viewport(
        cx: &mut TestAppContext,
    ) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                Some(100),
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let cell = |character| StructuredTerminalCell {
            character,
            ..Default::default()
        };
        let history = ('A'..='T').map(cell).collect::<Vec<_>>();
        let snapshot = StructuredTerminalSnapshot {
            cols: 1,
            rows: 2,
            cells: vec![cell('X'), cell('Y')],
            history,
            display_offset: 20,
            cursor: Some(StructuredTerminalCursor {
                point: Point::new(1, 0),
                shape: CursorShape::Block,
                visible: true,
                blinking: false,
            }),
            alternate_screen: false,
            modes: Modes::SHOW_CURSOR,
        };
        terminal
            .update(cx, |terminal, cx| {
                terminal.apply_structured_snapshot(&snapshot, cx)
            })
            .unwrap_or_else(|error| panic!("apply large history snapshot: {error}"));

        terminal.read_with(cx, |terminal, _cx| {
            use alacritty_terminal::index::{Column, Line};

            assert_eq!(terminal.total_lines(), 22);
            assert_eq!(terminal.last_content().display_offset, 20);
            assert_eq!(terminal.last_content().cells[0].character(), 'A');
            assert_eq!(terminal.last_content().cells[1].character(), 'B');
            assert_eq!(terminal.last_content().cursor.point, Point::new(1, 0));
            let term = terminal.term.lock_unfair();
            assert_eq!(term.grid()[Line(-20)][Column(0)].c, 'A');
            assert_eq!(term.grid()[Line(-1)][Column(0)].c, 'T');
            assert_eq!(term.grid()[Line(0)][Column(0)].c, 'X');
            assert_eq!(term.grid()[Line(1)][Column(0)].c, 'Y');
        });
    }

    #[gpui::test]
    async fn structured_snapshot_preserves_configured_history_capacity(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                Some(10),
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let cell = |character| StructuredTerminalCell {
            character,
            ..Default::default()
        };
        let snapshot = StructuredTerminalSnapshot {
            cols: 1,
            rows: 2,
            cells: vec![cell('X'), cell('Y')],
            history: Vec::new(),
            display_offset: 0,
            cursor: None,
            alternate_screen: false,
            modes: Modes::NONE,
        };
        terminal
            .update(cx, |terminal, cx| {
                terminal.apply_structured_snapshot(&snapshot, cx)
            })
            .unwrap_or_else(|error| panic!("apply empty-history snapshot: {error}"));

        terminal.update(cx, |terminal, cx| {
            terminal.write_pty_output(b"A\nB\nC\n", cx);
        });

        terminal.read_with(cx, |terminal, _cx| {
            assert!(
                terminal.total_lines() > 2,
                "structured snapshot must retain the configured scrollback capacity"
            );
        });
    }

    #[gpui::test]
    async fn structured_snapshot_rejects_incomplete_grid_without_mutation(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let before = terminal.read_with(cx, |terminal, _cx| terminal.get_content());
        let malformed = StructuredTerminalSnapshot {
            cols: 2,
            rows: 1,
            cells: vec![StructuredTerminalCell::default()],
            cursor: None,
            alternate_screen: false,
            history: Vec::new(),
            display_offset: 0,
            modes: Modes::NONE,
        };
        let result = terminal.update(cx, |terminal, cx| {
            terminal.apply_structured_snapshot(&malformed, cx)
        });
        assert!(result.is_err());
        let after = terminal.read_with(cx, |terminal, _cx| terminal.get_content());
        assert_eq!(after, before);
    }

    #[gpui::test]
    async fn structured_snapshot_rejects_oversized_grid_without_mutation(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let before = terminal.read_with(cx, |terminal, _cx| terminal.get_content());
        let oversized = StructuredTerminalSnapshot {
            cols: 4_097,
            rows: 1,
            cells: vec![StructuredTerminalCell::default(); 4_097],
            history: Vec::new(),
            display_offset: 0,
            cursor: None,
            alternate_screen: false,
            modes: Modes::NONE,
        };
        let result = terminal.update(cx, |terminal, cx| {
            terminal.apply_structured_snapshot(&oversized, cx)
        });
        assert!(result.is_err());
        let after = terminal.read_with(cx, |terminal, _cx| terminal.get_content());
        assert_eq!(after, before);
    }

    #[test]
    fn test_normalize_path_command_name() {
        assert_eq!(normalize_path_command_name("claude"), Some("claude".into()));
        assert_eq!(normalize_path_command_name("Cargo"), Some("cargo".into()));
        assert_eq!(normalize_path_command_name("node.exe"), Some("node".into()));
        assert_eq!(
            normalize_path_command_name("my-agent_cli.1"),
            Some("my-agent_cli.1".into())
        );
        assert_eq!(normalize_path_command_name("./local-agent"), None);
        assert_eq!(normalize_path_command_name("../local-agent"), None);
        assert_eq!(normalize_path_command_name("/usr/local/bin/cargo"), None);
        assert_eq!(
            normalize_path_command_name("target\\debug\\agent.exe"),
            None
        );
        assert_eq!(normalize_path_command_name(".hidden-agent"), None);
        assert_eq!(normalize_path_command_name("agent with spaces"), None);
        assert_eq!(normalize_path_command_name("zsh"), Some("zsh".into()));
        assert_eq!(normalize_path_command_name("-zsh"), None);
        assert_eq!(normalize_path_command_name("pwsh.exe"), Some("pwsh".into()));
    }

    #[test]
    fn test_foreground_process_command_from_interpreter_wrapper() {
        assert_eq!(
            foreground_process_command_from_argv(&[
                "node".to_string(),
                "/opt/homebrew/lib/node_modules/@google/gemini-cli/dist/index.js".to_string(),
            ]),
            Some("gemini".to_string())
        );
        assert_eq!(
            foreground_process_command_from_argv(&[
                "python3".to_string(),
                "/Users/me/.local/bin/codex.py".to_string(),
            ]),
            Some("codex".to_string())
        );
        assert_eq!(
            foreground_process_command_from_argv(&[
                "node".to_string(),
                "/Users/me/private-project/scripts/customer-data-export.js".to_string(),
            ]),
            Some("customer-data-export".to_string())
        );
    }

    #[cfg(not(target_os = "windows"))]
    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    /// Helper to build a test terminal running a shell command.
    /// Returns the terminal entity and a receiver for the completion signal.
    async fn build_test_terminal(
        cx: &mut TestAppContext,
        command: &str,
        args: &[&str],
    ) -> (Entity<Terminal>, Receiver<Option<ExitStatus>>) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (program, args) =
            ShellBuilder::new(&Shell::System, false).build(Some(command.to_owned()), &args);
        build_test_terminal_with_arguments(cx, program, args).await
    }

    async fn build_test_terminal_with_arguments(
        cx: &mut TestAppContext,
        program: String,
        args: Vec<String>,
    ) -> (Entity<Terminal>, Receiver<Option<ExitStatus>>) {
        let (completion_tx, completion_rx) = async_channel::unbounded();
        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    util::shell::Shell::WithArguments {
                        program,
                        args,
                        title_override: None,
                    },
                    HashMap::default(),
                    SettingsCursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    vec![],
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));
        (terminal, completion_rx)
    }

    /// Builds a non-PTY (`no_pty`) task terminal, exercising the path used by
    /// headless hosts (e.g. the eval CLI) where PTY allocation fails with
    /// `ENOTTY`. The command runs as a plain subprocess whose piped output is
    /// pumped into the emulator.
    #[cfg(not(target_os = "windows"))]
    async fn build_test_subprocess_terminal(
        cx: &mut TestAppContext,
        program: String,
        args: Vec<String>,
    ) -> (Entity<Terminal>, Receiver<Option<ExitStatus>>) {
        let (completion_tx, completion_rx) = async_channel::unbounded();
        let task_state = TaskState {
            status: TaskStatus::Running,
            completion_rx: completion_rx.clone(),
            spawned_task: SpawnInTerminal {
                command: Some(program.clone()),
                args: args.clone(),
                ..Default::default()
            },
        };
        let builder = cx
            .update(|cx| {
                cx.set_global(HeadlessTerminal(true));
                TerminalBuilder::new(
                    None,
                    Some(task_state),
                    util::shell::Shell::WithArguments {
                        program,
                        args,
                        title_override: None,
                    },
                    HashMap::default(),
                    SettingsCursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    vec![],
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));
        (terminal, completion_rx)
    }

    #[test]
    fn test_convert_lf_to_crlf_preserves_split_crlf() {
        let mut previous_byte_was_cr = false;
        assert_eq!(
            convert_lf_to_crlf(b"one\n", &mut previous_byte_was_cr),
            b"one\r\n"
        );
        assert!(!previous_byte_was_cr);

        let mut previous_byte_was_cr = false;
        assert_eq!(
            convert_lf_to_crlf(b"two\r", &mut previous_byte_was_cr),
            b"two\r"
        );
        assert!(previous_byte_was_cr);
        assert_eq!(
            convert_lf_to_crlf(b"\nthree", &mut previous_byte_was_cr),
            b"\nthree"
        );
        assert!(!previous_byte_was_cr);
    }

    /// Regression test for the agent terminal failing with `Not a tty (os error
    /// 25)` in headless/eval sandboxes: a `no_pty` task terminal must run
    /// without a PTY, capture stdout, and report its exit status.
    #[cfg(not(target_os = "windows"))]
    #[gpui::test]
    async fn test_no_pty_task_terminal_captures_output(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (program, args) = ShellBuilder::new(&Shell::System, false)
            .non_interactive()
            .build(Some("echo hello-from-subprocess".to_owned()), &[]);
        let (terminal, completion_rx) = build_test_subprocess_terminal(cx, program, args).await;

        assert!(
            !terminal.update(cx, |term, _| term.is_pty()),
            "no_pty terminal should not be PTY-backed"
        );
        assert_eq!(
            completion_rx.recv().await.unwrap(),
            Some(ExitStatus::default())
        );
        assert_content_eventually(&terminal, "hello-from-subprocess", cx).await;
    }

    fn init_ctrl_click_hyperlink_test(cx: &mut TestAppContext, output: &[u8]) -> Entity<Terminal> {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(output, cx);
        });

        cx.run_until_parked();

        terminal.update(cx, |terminal, _cx| {
            let term_lock = terminal.term.lock();
            terminal.last_content = make_content(
                &term_lock,
                &terminal.last_content,
                &terminal.image_placements,
            );
            drop(term_lock);

            let terminal_bounds = TerminalBounds::new(
                px(20.0),
                px(10.0),
                bounds(point(px(0.0), px(0.0)), size(px(400.0), px(400.0))),
            );
            terminal.last_content.terminal_bounds = terminal_bounds;
            terminal.events.clear();
            terminal.take_pty_write_log();
        });

        terminal
    }

    fn ctrl_mouse_down_at(
        terminal: &mut Terminal,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let mouse_down = MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::secondary_key(),
            click_count: 1,
            first_mouse: true,
        };
        terminal.mouse_down(&mouse_down, cx);
    }

    fn ctrl_mouse_move_to(
        terminal: &mut Terminal,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let terminal_bounds = terminal.last_content.terminal_bounds.bounds;
        let drag_event = MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::secondary_key(),
        };
        terminal.mouse_drag(&drag_event, terminal_bounds, cx);
    }

    fn ctrl_mouse_up_at(
        terminal: &mut Terminal,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let mouse_up = MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::secondary_key(),
            click_count: 1,
        };
        terminal.mouse_up(&mouse_up, cx);
    }

    fn left_mouse_down_at(
        terminal: &mut Terminal,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let mouse_down = MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: true,
        };
        terminal.mouse_down(&mouse_down, cx);
    }

    fn left_mouse_up_at(
        terminal: &mut Terminal,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let mouse_up = MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::none(),
            click_count: 1,
        };
        terminal.mouse_up(&mouse_up, cx);
    }

    fn left_mouse_drag_to(
        terminal: &mut Terminal,
        position: GpuiPoint<Pixels>,
        cx: &mut Context<Terminal>,
    ) {
        let region = terminal.last_content.terminal_bounds.bounds;
        let drag_event = MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::none(),
        };
        terminal.mouse_drag(&drag_event, region, cx);
    }

    /// A left click that jitters by a pixel or two (e.g. the window-focusing
    /// click) must not begin a selection, otherwise `copy_on_select` would
    /// overwrite the clipboard. Regression test for #58970.
    #[gpui::test]
    async fn test_terminal_click_jitter_does_not_start_selection(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"hello world\r\n");

        terminal.update(cx, |terminal, cx| {
            left_mouse_down_at(terminal, point(px(50.0), px(10.0)), cx);
            terminal.events.clear();

            // One pixel of movement is below the drag threshold.
            left_mouse_drag_to(terminal, point(px(51.0), px(10.0)), cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::UpdateSelection(_))),
                "a sub-threshold click jitter should not start a selection"
            );
            assert!(terminal.selection_phase == SelectionPhase::Ended);
        });
    }

    /// A deliberate drag past the threshold must still start a selection.
    #[gpui::test]
    async fn test_terminal_deliberate_drag_starts_selection(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"hello world\r\n");

        terminal.update(cx, |terminal, cx| {
            left_mouse_down_at(terminal, point(px(50.0), px(10.0)), cx);
            terminal.events.clear();

            // Well beyond the drag threshold.
            left_mouse_drag_to(terminal, point(px(90.0), px(10.0)), cx);

            assert!(
                terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::UpdateSelection(_))),
                "a deliberate drag should start a selection"
            );
            assert!(terminal.selection_phase == SelectionPhase::Selecting);
        });
    }

    #[gpui::test]
    async fn test_basic_terminal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (terminal, completion_rx) = build_test_terminal(cx, "echo", &["hello"]).await;
        assert_eq!(
            completion_rx.recv().await.unwrap(),
            Some(ExitStatus::default())
        );
        assert_content_eventually(&terminal, "hello", cx).await;

        // Inject additional output directly into the emulator (display-only path)
        terminal.update(cx, |term, cx| {
            term.write_output(b"\nfrom_injection", cx);
        });

        let content_after = terminal.update(cx, |term, _| term.get_content());
        assert!(
            content_after.contains("from_injection"),
            "expected injected output to appear, got: {content_after}"
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_foreground_process_command_tracks_path_command(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (terminal, completion_rx) =
            build_test_terminal_with_arguments(cx, "sleep".to_string(), vec!["1".to_string()])
                .await;

        assert_foreground_process_command_eventually(&terminal, "sleep", cx).await;

        assert!(
            completion_rx.recv().await.is_ok(),
            "expected terminal completion after sleep exits"
        );
    }

    // TODO should be tested on Linux too, but does not work there well
    #[cfg(target_os = "macos")]
    #[gpui::test(iterations = 10)]
    async fn test_terminal_eof(cx: &mut TestAppContext) {
        init_test(cx);

        cx.executor().allow_parking();

        let (completion_tx, completion_rx) = async_channel::unbounded();
        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    util::shell::Shell::System,
                    HashMap::default(),
                    SettingsCursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    Vec::new(),
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        // Build an empty command, which will result in a tty shell spawned.
        let terminal = cx.new(|cx| builder.subscribe(cx));

        let (event_tx, event_rx) = async_channel::unbounded::<Event>();
        cx.update(|cx| {
            cx.subscribe(&terminal, move |_, e, _| {
                event_tx.send_blocking(e.clone()).unwrap();
            })
        })
        .detach();
        cx.background_spawn(async move {
            assert_eq!(
                completion_rx.recv().await.unwrap(),
                Some(ExitStatus::default()),
                "EOF should result in the tty shell exiting successfully",
            );
        })
        .detach();

        let first_event = event_rx.recv().await.expect("No wakeup event received");

        terminal.update(cx, |terminal, _| {
            let success = terminal.try_keystroke(&Keystroke::parse("ctrl-d").unwrap(), false);
            assert!(success, "Should have registered ctrl-d sequence");
        });

        let mut all_events = vec![first_event];
        while let Ok(new_event) = event_rx.recv().await {
            all_events.push(new_event.clone());
            if new_event == Event::CloseTerminal {
                break;
            }
        }
        assert!(
            all_events.contains(&Event::CloseTerminal),
            "EOF command sequence should have triggered a TTY terminal exit, but got events: {all_events:?}",
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[gpui::test(iterations = 10)]
    async fn test_terminal_closes_after_nonzero_exit(cx: &mut TestAppContext) {
        init_test(cx);

        cx.executor().allow_parking();

        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    util::shell::Shell::System,
                    HashMap::default(),
                    SettingsCursorShape::default(),
                    AlternateScroll::On,
                    None,
                    vec![],
                    0,
                    false,
                    0,
                    None,
                    cx,
                    Vec::new(),
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));

        let (event_tx, event_rx) = async_channel::unbounded::<Event>();
        cx.update(|cx| {
            cx.subscribe(&terminal, move |_, e, _| {
                event_tx.send_blocking(e.clone()).unwrap();
            })
        })
        .detach();

        let first_event = event_rx.recv().await.expect("No wakeup event received");

        terminal.update(cx, |terminal, _| {
            terminal.input(b"false\r".to_vec());
        });
        cx.executor().timer(Duration::from_millis(500)).await;
        terminal.update(cx, |terminal, _| {
            terminal.input(b"exit\r".to_vec());
        });

        let mut all_events = vec![first_event];
        while let Ok(new_event) = event_rx.recv().await {
            all_events.push(new_event.clone());
            if new_event == Event::CloseTerminal {
                break;
            }
        }
        assert!(
            all_events.contains(&Event::CloseTerminal),
            "Shell exiting after `false && exit` should close terminal, but got events: {all_events:?}",
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_terminal_no_exit_on_spawn_failure(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (completion_tx, completion_rx) = async_channel::unbounded();
        let (program, args) = ShellBuilder::new(&Shell::System, false)
            .build(Some("asdasdasdasd".to_owned()), &["@@@@@".to_owned()]);
        let builder = cx
            .update(|cx| {
                TerminalBuilder::new(
                    None,
                    None,
                    util::shell::Shell::WithArguments {
                        program,
                        args,
                        title_override: None,
                    },
                    HashMap::default(),
                    SettingsCursorShape::default(),
                    AlternateScroll::On,
                    None,
                    Vec::new(),
                    0,
                    false,
                    0,
                    Some(completion_tx),
                    cx,
                    Vec::new(),
                    PathStyle::local(),
                )
            })
            .await
            .unwrap();
        let terminal = cx.new(|cx| builder.subscribe(cx));

        let all_events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        cx.update({
            let all_events = all_events.clone();
            |cx| {
                cx.subscribe(&terminal, move |_, e, _| {
                    all_events.lock().push(e.clone());
                })
            }
        })
        .detach();
        let completion_check_task = cx.background_spawn(async move {
            // The channel may be closed if the terminal is dropped before sending
            // the completion signal, which can happen with certain task scheduling orders.
            let exit_status = completion_rx.recv().await.ok().flatten();
            if let Some(exit_status) = exit_status {
                assert!(
                    !exit_status.success(),
                    "Wrong shell command should result in a failure"
                );
                #[cfg(target_os = "windows")]
                assert_eq!(exit_status.code(), Some(1));
                #[cfg(not(target_os = "windows"))]
                assert_eq!(exit_status.code(), Some(127)); // code 127 means "command not found" on Unix
            }
        });

        completion_check_task.await;
        cx.executor().timer(Duration::from_millis(500)).await;

        assert!(
            !all_events
                .lock()
                .iter()
                .any(|event| event == &Event::CloseTerminal),
            "Wrong shell command should update the title but not should not close the terminal to show the error message, but got events: {all_events:?}",
        );
    }

    #[test]
    fn test_rgb_for_index() {
        // Test every possible value in the color cube.
        for i in 16..=231 {
            let (r, g, b) = rgb_for_index(i);
            assert_eq!(i, 16 + 36 * r + 6 * g + b);
        }
    }

    #[gpui::test]
    fn test_mouse_to_cell_test(mut rng: StdRng) {
        const ITERATIONS: usize = 10;
        const PRECISION: usize = 1000;

        for _ in 0..ITERATIONS {
            let viewport_cells = rng.random_range(15..20);
            let cell_size =
                rng.random_range(5 * PRECISION..20 * PRECISION) as f32 / PRECISION as f32;

            let size = crate::TerminalBounds {
                cell_width: Pixels::from(cell_size),
                line_height: Pixels::from(cell_size),
                bounds: bounds(
                    GpuiPoint::default(),
                    size(
                        Pixels::from(cell_size * (viewport_cells as f32)),
                        Pixels::from(cell_size * (viewport_cells as f32)),
                    ),
                ),
            };

            let cells = get_cells(size, &mut rng);
            let content = convert_cells_to_content(size, &cells);

            for row in 0..(viewport_cells - 1) {
                let row = row as usize;
                for col in 0..(viewport_cells - 1) {
                    let col = col as usize;

                    let row_offset = rng.random_range(0..PRECISION) as f32 / PRECISION as f32;
                    let col_offset = rng.random_range(0..PRECISION) as f32 / PRECISION as f32;

                    let mouse_pos = point(
                        Pixels::from(col as f32 * cell_size + col_offset),
                        Pixels::from(row as f32 * cell_size + row_offset),
                    );

                    let content_index =
                        content_index_for_mouse(mouse_pos, &content.terminal_bounds);
                    let mouse_cell = content.cells[content_index].character();
                    let real_cell = cells[row][col];

                    assert_eq!(mouse_cell, real_cell);
                }
            }
        }
    }

    #[gpui::test]
    fn test_mouse_to_cell_clamp(mut rng: StdRng) {
        let size = crate::TerminalBounds {
            cell_width: Pixels::from(10.),
            line_height: Pixels::from(10.),
            bounds: bounds(
                GpuiPoint::default(),
                size(Pixels::from(100.), Pixels::from(100.)),
            ),
        };

        let cells = get_cells(size, &mut rng);
        let content = convert_cells_to_content(size, &cells);

        assert_eq!(
            content.cells[content_index_for_mouse(
                point(Pixels::from(-10.), Pixels::from(-10.)),
                &content.terminal_bounds,
            )]
            .character(),
            cells[0][0]
        );
        assert_eq!(
            content.cells[content_index_for_mouse(
                point(Pixels::from(1000.), Pixels::from(1000.)),
                &content.terminal_bounds,
            )]
            .character(),
            cells[9][9]
        );
    }

    #[gpui::test]
    async fn test_set_size_coalesces_pixel_only_changes(cx: &mut TestAppContext) {
        let builder = cx.update(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::Block,
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
        });
        let mut terminal = builder.terminal;

        let base_bounds = TerminalBounds {
            cell_width: Pixels::from(10.),
            line_height: Pixels::from(10.),
            bounds: bounds(
                GpuiPoint::default(),
                size(Pixels::from(100.), Pixels::from(100.)),
            ),
        };

        terminal.set_size(base_bounds);
        terminal.events.clear();
        assert_eq!(terminal.last_content.terminal_bounds, base_bounds);

        // Pixel-only change: height grows by 1px but still the same number of rows/cols.
        let mut pixel_changed = base_bounds;
        pixel_changed.bounds.size.height = Pixels::from(101.);
        terminal.set_size(pixel_changed);
        assert!(terminal.events.is_empty());
        assert_eq!(terminal.last_content.terminal_bounds, pixel_changed);

        // Grid change: height increases enough to add a row.
        let mut grid_changed = base_bounds;
        grid_changed.bounds.size.height = Pixels::from(110.);
        terminal.set_size(grid_changed);
        assert!(matches!(
            terminal.events.back(),
            Some(InternalEvent::Resize(_))
        ));
    }

    fn get_cells(size: TerminalBounds, rng: &mut StdRng) -> Vec<Vec<char>> {
        let mut cells = Vec::new();

        for _ in 0..size.num_lines() {
            let mut row_vec = Vec::new();
            for _ in 0..size.num_columns() {
                let cell_char = rng.sample(distr::Alphanumeric) as char;
                row_vec.push(cell_char)
            }
            cells.push(row_vec)
        }

        cells
    }

    fn convert_cells_to_content(terminal_bounds: TerminalBounds, cells: &[Vec<char>]) -> Content {
        let mut ic = Vec::new();

        for (index, row) in cells.iter().enumerate() {
            for (cell_index, cell_char) in row.iter().enumerate() {
                let mut cell = Cell::default();
                cell.set_character(*cell_char);
                ic.push(IndexedCell {
                    point: Point::new(index as i32, cell_index),
                    cell,
                });
            }
        }

        Content {
            cells: ic,
            terminal_bounds,
            ..Default::default()
        }
    }

    #[gpui::test]
    async fn test_write_init_command_after_startup_clears_without_shell_command(
        cx: &mut TestAppContext,
    ) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"startup output\nprompt", cx);
        });

        let wrote = terminal.update(cx, |terminal, cx| {
            terminal.write_init_command_after_startup(b"agent\r".to_vec(), cx)
        });
        assert!(wrote);
        let content = terminal.update(cx, |terminal, _| terminal.get_content());
        assert!(
            !content.contains("startup output"),
            "startup output should be cleared internally before writing the init command"
        );
        let input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert_eq!(input_log, vec![b"agent\r".to_vec()]);
    }

    #[gpui::test]
    async fn test_write_init_command_after_startup_skips_after_keyboard_input(
        cx: &mut TestAppContext,
    ) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        let wrote = terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"startup output\nprompt", cx);
            terminal.input(b"user input".to_vec());
            terminal.write_init_command_after_startup(b"agent\r".to_vec(), cx)
        });
        assert!(!wrote);
        let content = terminal.update(cx, |terminal, _| terminal.get_content());
        assert!(
            content.contains("startup output"),
            "startup output should be left alone when the init command is skipped"
        );
        let input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert_eq!(input_log, vec![b"user input".to_vec()]);
    }

    #[gpui::test]
    async fn test_write_init_command_after_startup_skips_after_child_exit(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"shell failed to start\nprompt", cx);
            #[cfg(unix)]
            let exit_status =
                <ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(1 << 8);
            #[cfg(windows)]
            let exit_status = <ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw(1);
            terminal.register_task_finished(Some(exit_status), cx);
        });

        let wrote = terminal.update(cx, |terminal, cx| {
            terminal.write_init_command_after_startup(b"agent\r".to_vec(), cx)
        });
        assert!(!wrote);
        let content = terminal.update(cx, |terminal, _| terminal.get_content());
        assert!(
            content.contains("shell failed to start"),
            "startup failure output should be preserved when the init command is skipped"
        );
        let input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert!(
            input_log.is_empty(),
            "init command should not be written after the child has exited, got {input_log:?}"
        );
    }

    #[gpui::test]
    async fn test_write_output_converts_lf_to_crlf(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        // Test simple LF conversion
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"line1\nline2\n", cx);
        });

        // Get the content by directly accessing the term
        let content = terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock_unfair();
            make_content(&term, &terminal.last_content, &terminal.image_placements)
        });

        // If LF is properly converted to CRLF, each line should start at column 0
        // The diagonal staircase bug would cause increasing column positions

        // Get the cells and check that lines start at column 0
        let cells = &content.cells;
        let mut line1_col0 = false;
        let mut line2_col0 = false;

        for cell in cells {
            if cell.character() == 'l' && cell.point.column == 0 {
                if cell.point.line == 0 && !line1_col0 {
                    line1_col0 = true;
                } else if cell.point.line == 1 && !line2_col0 {
                    line2_col0 = true;
                }
            }
        }

        assert!(line1_col0, "First line should start at column 0");
        assert!(line2_col0, "Second line should start at column 0");
    }

    fn kitty_png_sequence(width: u32, height: u32, control: &str) -> Vec<u8> {
        use base64::Engine as _;

        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 9, 9, 255]));
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("write test png");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        format!("\x1b_G{control};{encoded}\x1b\\").into_bytes()
    }

    fn display_only_terminal(cx: &mut TestAppContext) -> Entity<Terminal> {
        cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                Some(10_000),
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        })
    }

    #[gpui::test]
    async fn kitty_graphics_sequence_reaches_the_renderable_content(cx: &mut TestAppContext) {
        let terminal = display_only_terminal(cx);

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"before\r\n", cx);
            terminal.write_output(&kitty_png_sequence(32, 16, "a=T,f=100,t=d,c=4,r=2"), cx);
        });

        terminal.update(cx, |terminal, _| {
            assert_eq!(terminal.image_cache().len(), 1, "image must be cached");
            assert_eq!(
                terminal.image_placements().len(),
                1,
                "the sequence must produce a placement"
            );
            let placement = terminal.image_placements()[0];
            assert_eq!(placement.columns, 4);
            assert_eq!(placement.rows, 2);

            // The overlay only lands in `Content` once the emulator state is
            // projected back into viewport coordinates.
            let term = terminal.term.lock_unfair();
            let content = make_content(&term, &terminal.last_content, &terminal.image_placements);
            assert_eq!(content.images.len(), 1);
            assert_eq!(content.images[0].id, placement.id);
            assert_eq!(content.images[0].row, 1, "the cursor was on the second row");
            assert_eq!(content.images[0].columns, 4);
            assert_eq!(content.images[0].rows, 2);
        });
    }

    #[gpui::test]
    async fn iterm_inline_image_reaches_the_renderable_content(cx: &mut TestAppContext) {
        use base64::Engine as _;

        let terminal = display_only_terminal(cx);
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("write test png");
        let sequence = format!(
            "\x1b]1337;File=inline=1;width=3;height=2:{}\x07",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(sequence.as_bytes(), cx);
        });

        terminal.update(cx, |terminal, _| {
            assert_eq!(terminal.image_cache().len(), 1);
            let placement = terminal.image_placements()[0];
            assert_eq!(placement.columns, 3);
            assert_eq!(placement.rows, 2);
        });
    }

    #[gpui::test]
    async fn graphics_overlay_follows_scrollback(cx: &mut TestAppContext) {
        let terminal = display_only_terminal(cx);

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(&kitty_png_sequence(16, 16, "a=T,f=100,c=2,r=1"), cx);
        });
        let anchor = terminal.update(cx, |terminal, _| terminal.image_placements()[0].anchor_line);

        // Push the anchored row far up into scrollback.
        terminal.update(cx, |terminal, cx| {
            let mut bytes = Vec::new();
            for index in 0..200u32 {
                bytes.extend_from_slice(format!("line {index}\r\n").as_bytes());
            }
            terminal.write_output(&bytes, cx);
        });

        terminal.update(cx, |terminal, _| {
            assert_eq!(
                terminal.image_placements()[0].anchor_line,
                anchor,
                "the anchor is scroll independent"
            );
            let term = terminal.term.lock_unfair();
            let content = make_content(&term, &terminal.last_content, &terminal.image_placements);
            assert!(
                content.images.is_empty(),
                "an image scrolled out of the viewport must not be drawn"
            );
        });
    }

    #[gpui::test]
    async fn kitty_delete_removes_the_placement(cx: &mut TestAppContext) {
        let terminal = display_only_terminal(cx);

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(&kitty_png_sequence(8, 8, "a=T,f=100,i=17,c=2,r=1"), cx);
            assert_eq!(terminal.image_placements().len(), 1);

            terminal.write_output(b"\x1b_Ga=d,d=I,i=17\x1b\\", cx);
        });

        terminal.update(cx, |terminal, _| {
            assert!(terminal.image_placements().is_empty());
            assert!(terminal.image_cache().is_empty(), "d=I frees the data");
        });
    }

    /// The PTY path routes bytes through alacritty's own event loop, so the
    /// graphics tap can only be verified with a real child process.
    #[cfg(not(target_os = "windows"))]
    #[gpui::test]
    async fn kitty_graphics_survives_the_pty_event_loop(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();

        let sequence = String::from_utf8(kitty_png_sequence(16, 16, "a=T,f=100,c=2,r=1"))
            .expect("kitty sequences are ascii");
        let (terminal, _completion_rx) = build_test_terminal_with_arguments(
            cx,
            "/bin/sh".to_string(),
            vec![
                "-c".to_string(),
                format!("printf '%s' '{sequence}'; sleep 30"),
            ],
        )
        .await;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if terminal.update(cx, |terminal, _| terminal.image_cache().len() == 1) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the PTY graphics tap never delivered the image"
            );
            cx.background_executor
                .timer(Duration::from_millis(20))
                .await;
        }

        terminal.update(cx, |terminal, _| {
            assert_eq!(terminal.image_placements().len(), 1);
            assert_eq!(terminal.image_placements()[0].columns, 2);
        });
    }

    #[gpui::test]
    async fn kitty_query_writes_a_protocol_response(cx: &mut TestAppContext) {
        let terminal = display_only_terminal(cx);

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(&kitty_png_sequence(1, 1, "a=q,f=100,i=31"), cx);
        });

        terminal.update(cx, |terminal, _| {
            assert!(terminal.image_cache().is_empty(), "a query stores nothing");
            let writes = terminal.pty_write_log.borrow();
            assert!(
                writes
                    .iter()
                    .any(|write| write == b"\x1b_Gi=31;OK\x1b\\".as_slice()),
                "expected an OK response, got {writes:?}"
            );
        });
    }

    #[gpui::test]
    async fn test_write_output_preserves_existing_crlf(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        // Test that existing CRLF doesn't get doubled
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"line1\r\nline2\r\n", cx);
        });

        // Get the content by directly accessing the term
        let content = terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock_unfair();
            make_content(&term, &terminal.last_content, &terminal.image_placements)
        });

        let cells = &content.cells;

        // Check that both lines start at column 0
        let mut found_lines_at_column_0 = 0;
        for cell in cells {
            if cell.character() == 'l' && cell.point.column == 0 {
                found_lines_at_column_0 += 1;
            }
        }

        assert!(
            found_lines_at_column_0 >= 2,
            "Both lines should start at column 0"
        );
    }

    #[gpui::test]
    async fn test_write_output_preserves_bare_cr(cx: &mut TestAppContext) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        // Test that bare CR (without LF) is preserved
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"hello\rworld", cx);
        });

        // Get the content by directly accessing the term
        let content = terminal.update(cx, |terminal, _cx| {
            let term = terminal.term.lock_unfair();
            make_content(&term, &terminal.last_content, &terminal.image_placements)
        });

        let cells = &content.cells;

        // Check that we have "world" at the beginning of the line
        let mut text = String::new();
        for cell in cells.iter().take(5) {
            if cell.point.line == 0 {
                text.push(cell.character());
            }
        }

        assert!(
            text.starts_with("world"),
            "Bare CR should allow overwriting: got '{}'",
            text
        );
    }

    /// §15.12 Reconnect recovery: `scroll_to_display_offset` must drive the
    /// embedded Terminal's real scroll position so that, after `sync`,
    /// `last_content().display_offset` equals the restored nonzero offset.
    #[gpui::test]
    async fn test_scroll_to_display_offset_restores_scroll_position(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                Some(10_000),
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        // Write far more lines than the viewport to materialize scrollback.
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                let mut bytes = Vec::new();
                for index in 0..300u32 {
                    bytes.extend_from_slice(format!("line {}\r\n", index).as_bytes());
                }
                terminal.write_output(&bytes, cx);
            });
        });

        // Flush the write and confirm scrollback history exists. history is
        // total_lines - visible screen rows.
        let history_size = cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.sync(window, cx);
            });
            let term = terminal.read(cx).term.lock_unfair();
            crate::alacritty::total_lines(&term)
                .saturating_sub(crate::alacritty::screen_lines(&term))
        });
        assert!(
            history_size > 0,
            "precondition: scrollback history must exist, got history={}",
            history_size
        );

        // Pick a nonzero target strictly within history, then restore it.
        let target_offset = (history_size / 2).max(1);
        cx.update(|_window, cx| {
            terminal.update(cx, |terminal, _cx| {
                terminal.scroll_to_display_offset(target_offset);
            });
        });

        let restored = cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.sync(window, cx);
                terminal.last_content.display_offset
            })
        });
        assert_eq!(
            restored, target_offset,
            "last_content().display_offset must equal the restored nonzero offset"
        );
    }

    /// §12 Plan 31 — a confirmed query drives `n` / `N` across every match in
    /// the grid, wrapping in both directions.
    #[gpui::test]
    async fn test_copy_mode_search_walks_matches_in_both_directions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        let match_count = cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(
                    b"needle one\r\nfiller\r\nneedle two\r\nneedle three\r\n",
                    cx,
                );
                terminal.toggle_vi_mode();
                terminal.sync(window, cx);
                terminal.set_search_query("needle")
            })
        });
        assert_eq!(
            match_count.unwrap_or_else(|error| panic!("confirm search query: {error}")),
            3
        );

        let mut visited = Vec::new();
        for _ in 0..4 {
            visited.push(cx.update(|window, cx| {
                terminal.update(cx, |terminal, cx| {
                    assert!(terminal.search_next(), "search_next must find a match");
                    terminal.sync(window, cx);
                    terminal.last_content.cursor.point
                })
            }));
        }
        // "needle" ends at column 5; the cursor starts past every match so the
        // first step wraps to the top, and the fourth wraps again.
        assert_eq!(
            visited,
            vec![
                Point::new(0, 5),
                Point::new(2, 5),
                Point::new(3, 5),
                Point::new(0, 5),
            ]
        );

        let mut reversed = Vec::new();
        for _ in 0..2 {
            reversed.push(cx.update(|window, cx| {
                terminal.update(cx, |terminal, cx| {
                    assert!(
                        terminal.search_previous(),
                        "search_previous must find a match"
                    );
                    terminal.sync(window, cx);
                    terminal.last_content.cursor.point
                })
            }));
        }
        assert_eq!(reversed, vec![Point::new(3, 5), Point::new(2, 5)]);

        cx.update(|_window, cx| {
            terminal.update(cx, |terminal, _cx| {
                assert_eq!(terminal.search_query(), Some("needle"));
                terminal.clear_search();
                assert_eq!(terminal.search_query(), None);
                assert!(terminal.matches.is_empty());
                assert!(
                    !terminal.search_next(),
                    "search_next must be inert without a query"
                );
            });
        });
    }

    /// §12 Plan 31 — vi mode routes `n` / `N` into the search so copy mode and
    /// plain vi mode share one implementation.
    #[gpui::test]
    async fn test_vi_motion_n_navigates_search_matches(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"match one\r\nfiller\r\nmatch two\r\n", cx);
                terminal.toggle_vi_mode();
                terminal.sync(window, cx);
                terminal
                    .set_search_query("match")
                    .unwrap_or_else(|error| panic!("confirm search query: {error}"));
            });
        });

        let mut next = Keystroke::default();
        next.key = "n".to_string();
        let mut previous = Keystroke::default();
        previous.key = "n".to_string();
        previous.modifiers.shift = true;

        let first = cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.vi_motion(&next);
                terminal.sync(window, cx);
                terminal.last_content.cursor.point
            })
        });
        assert_eq!(first, Point::new(0, 4));

        let second = cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.vi_motion(&next);
                terminal.sync(window, cx);
                terminal.last_content.cursor.point
            })
        });
        assert_eq!(second, Point::new(2, 4));

        let back = cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.vi_motion(&previous);
                terminal.sync(window, cx);
                terminal.last_content.cursor.point
            })
        });
        assert_eq!(back, Point::new(0, 4));
    }

    /// §12 Plan 31 — an uncompilable query is reported instead of leaving the
    /// previous match list in place.
    #[gpui::test]
    async fn test_search_query_rejects_invalid_regex(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let cx = cx.add_empty_window();
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"needle\r\n", cx);
                terminal.sync(window, cx);
                terminal
                    .set_search_query("needle")
                    .unwrap_or_else(|error| panic!("confirm search query: {error}"));
                assert_eq!(terminal.matches.len(), 1);

                assert!(terminal.set_search_query("(unclosed").is_err());
                assert_eq!(terminal.search_query(), None);
                assert!(
                    terminal.matches.is_empty(),
                    "a rejected query must not leave stale matches highlighted"
                );
                assert!(terminal.set_search_query("").is_err());
            });
        });
    }

    #[gpui::test]
    async fn test_display_only_write_output_ignores_osc52(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.write_to_clipboard(ClipboardItem::new_string("original".to_string()));
        });

        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });

        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"\x1b]52;c;b3ZlcndyaXR0ZW4=\x07", cx);
        });
        cx.run_until_parked();

        let clipboard_text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(clipboard_text.as_deref(), Some("original"));
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_same_position(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            let click_position = point(px(80.0), px(10.0));
            ctrl_mouse_down_at(terminal, click_position, cx);
            ctrl_mouse_up_at(terminal, click_position, cx);

            assert!(
                terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, true))),
                "Should have ProcessHyperlink event when ctrl+clicking on same hyperlink position"
            );
        });
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_same_position_in_mouse_mode(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            terminal.last_content.mode = Modes::MOUSE_MODE;

            let click_position = point(px(80.0), px(10.0));
            ctrl_mouse_down_at(terminal, click_position, cx);
            ctrl_mouse_up_at(terminal, click_position, cx);

            assert!(
                terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, true))),
                "Should have ProcessHyperlink event when ctrl+clicking on same hyperlink position in mouse mode"
            );
            assert!(
                terminal.take_pty_write_log().is_empty(),
                "a consumed link click must not be reported to the PTY"
            );
        });
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_mismatch_in_mouse_mode_consumes_gesture(
        cx: &mut TestAppContext,
    ) {
        let terminal = init_ctrl_click_hyperlink_test(
            cx,
            b"Visit https://zed.dev/ for more\r\nThis is another line\r\n",
        );

        terminal.update(cx, |terminal, cx| {
            terminal.last_content.mode = Modes::MOUSE_MODE;
            terminal.take_pty_write_log();

            let down_position = point(px(80.0), px(10.0));
            let up_position = point(px(10.0), px(30.0));

            ctrl_mouse_down_at(terminal, down_position, cx);
            terminal.mouse_move(
                &MouseMoveEvent {
                    position: up_position,
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::secondary_key(),
                },
                cx,
            );
            ctrl_mouse_up_at(terminal, up_position, cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, _))),
                "Should NOT open a link when press and release land on different hyperlinks"
            );
            let pty_writes = terminal.take_pty_write_log();
            assert!(
                pty_writes.is_empty(),
                "a captured press must consume the whole gesture, but reports leaked to the PTY: {pty_writes:?}"
            );
        });
    }

    #[gpui::test]
    async fn test_plain_click_on_hyperlink_in_mouse_mode_is_reported(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            terminal.last_content.mode = Modes::MOUSE_MODE;
            terminal.take_pty_write_log();

            let click_position = point(px(80.0), px(10.0));
            left_mouse_down_at(terminal, click_position, cx);
            left_mouse_up_at(terminal, click_position, cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, _))),
                "a plain click must not open a link"
            );
            let pty_writes = terminal.take_pty_write_log();
            assert_eq!(
                pty_writes.len(),
                2,
                "expected press and release reports, got {pty_writes:?}"
            );
        });
    }

    #[gpui::test]
    async fn test_ctrl_click_on_non_hyperlink_in_mouse_mode_is_reported(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            terminal.last_content.mode = Modes::MOUSE_MODE;
            terminal.take_pty_write_log();

            // Past the end of the line: nothing link-like under the cursor.
            let click_position = point(px(370.0), px(10.0));
            ctrl_mouse_down_at(terminal, click_position, cx);
            ctrl_mouse_up_at(terminal, click_position, cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, _))),
                "a secondary click off a link must not open anything"
            );
            let pty_writes = terminal.take_pty_write_log();
            assert_eq!(
                pty_writes.len(),
                2,
                "expected press and release reports, got {pty_writes:?}"
            );
        });
    }

    #[gpui::test]
    async fn test_ctrl_click_in_mouse_mode_forwards_when_setting_disabled(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        cx.update_global(|store: &mut settings::SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .terminal
                    .get_or_insert_default()
                    .open_links_in_mouse_mode = Some(false);
            });
        });

        terminal.update(cx, |terminal, cx| {
            terminal.last_content.mode = Modes::MOUSE_MODE;

            let click_position = point(px(80.0), px(10.0));
            ctrl_mouse_down_at(terminal, click_position, cx);
            ctrl_mouse_up_at(terminal, click_position, cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, _))),
                "with the setting disabled, ctrl+click must not open links in mouse mode"
            );
            let pty_writes = terminal.take_pty_write_log();
            assert_eq!(
                pty_writes.len(),
                2,
                "expected press and release reports, got {pty_writes:?}"
            );
        });
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_drag_outside_bounds(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(
            cx,
            b"Visit https://zed.dev/ for more\r\nThis is another line\r\n",
        );

        terminal.update(cx, |terminal, cx| {
            let down_position = point(px(80.0), px(10.0));
            let up_position = point(px(10.0), px(50.0));

            ctrl_mouse_down_at(terminal, down_position, cx);
            ctrl_mouse_move_to(terminal, up_position, cx);
            ctrl_mouse_up_at(terminal, up_position, cx);

            assert!(
                !terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, _))),
                "Should NOT have ProcessHyperlink event when dragging outside the hyperlink"
            );
        });
    }

    #[gpui::test]
    async fn test_hyperlink_ctrl_click_drag_within_bounds(cx: &mut TestAppContext) {
        let terminal = init_ctrl_click_hyperlink_test(cx, b"Visit https://zed.dev/ for more\r\n");

        terminal.update(cx, |terminal, cx| {
            let down_position = point(px(70.0), px(10.0));
            let up_position = point(px(130.0), px(10.0));

            ctrl_mouse_down_at(terminal, down_position, cx);
            ctrl_mouse_move_to(terminal, up_position, cx);
            ctrl_mouse_up_at(terminal, up_position, cx);

            assert!(
                terminal
                    .events
                    .iter()
                    .any(|event| matches!(event, InternalEvent::ProcessHyperlink(_, true))),
                "Should have ProcessHyperlink event when dragging within hyperlink bounds"
            );
        });
    }

    /// Polls the terminal content until `expected` appears, or panics after ~1s.
    /// The PTY IO thread writes into the terminal grid independently of the
    /// GPUI executor, so we need a real-time polling loop to synchronize.
    async fn assert_content_eventually(
        terminal: &Entity<Terminal>,
        expected: &str,
        cx: &mut TestAppContext,
    ) {
        let mut content = String::new();
        for _ in 0..100 {
            content = terminal.update(cx, |term, _| term.get_content());
            if content.contains(expected) {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        panic!("Expected terminal content to contain {expected:?}, got: {content}");
    }

    #[cfg(unix)]
    async fn assert_foreground_process_command_eventually(
        terminal: &Entity<Terminal>,
        expected: &str,
        cx: &mut TestAppContext,
    ) {
        let mut command_name = None;
        for _ in 0..100 {
            terminal.update(cx, |terminal, _| {
                if let TerminalType::Pty { info, .. } = &terminal.terminal_type {
                    info.load_for_test();
                }
            });
            command_name =
                terminal.update(cx, |terminal, _| terminal.foreground_process_command_name());
            if command_name.as_deref() == Some(expected) {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        let process_info = terminal.update(cx, |terminal, _| match &terminal.terminal_type {
            TerminalType::Pty { info, .. } => format!(
                "pid={:?}, fallback_pid={:?}, has_current_info={}",
                info.pid(),
                info.pid_getter().fallback_pid(),
                info.current.read().is_some()
            ),
            TerminalType::DisplayOnly => "display-only".to_string(),
        });
        panic!(
            "Expected foreground process command name to be {expected:?}, got {command_name:?}; process info: {process_info:?}"
        );
    }

    /// Test that kill_active_task properly terminates both the foreground process
    /// and the shell, allowing wait_for_completed_task to complete and output to be captured.
    #[cfg(unix)]
    #[gpui::test]
    async fn test_kill_active_task_completes_and_captures_output(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        // Run a command that prints output then sleeps for a long time
        // The echo ensures we have output to capture before killing
        let (terminal, completion_rx) =
            build_test_terminal(cx, "echo", &["test_output_before_kill; sleep 60"]).await;

        assert_content_eventually(&terminal, "test_output_before_kill", cx).await;

        // Kill the active task
        terminal.update(cx, |term, _cx| {
            term.kill_active_task();
        });

        // wait_for_completed_task should complete within a reasonable time (not hang)
        let completion_result = completion_rx.recv().await;
        assert!(
            completion_result.is_ok(),
            "wait_for_completed_task should complete after kill_active_task, but it timed out"
        );

        // The exit status should indicate the process was killed (not a clean exit)
        let exit_status = completion_result.unwrap();
        assert!(
            exit_status.is_some(),
            "Should have received an exit status after killing"
        );

        // Verify that output captured before killing is still available
        let content = terminal.update(cx, |term, _| term.get_content());
        assert!(
            content.contains("test_output_before_kill"),
            "Output from before kill should be captured, got: {content}"
        );
    }

    /// Test that kill_active_task on a task that's not running is a no-op
    #[gpui::test]
    async fn test_kill_active_task_on_completed_task_is_noop(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        // Run a command that exits immediately
        let (terminal, completion_rx) = build_test_terminal(cx, "echo", &["done"]).await;

        // Wait for the command to complete naturally
        let exit_status = completion_rx
            .recv()
            .await
            .expect("Should receive exit status");
        assert_eq!(exit_status, Some(ExitStatus::default()));

        assert_content_eventually(&terminal, "done", cx).await;

        // Now try to kill - should be a no-op since task already completed
        terminal.update(cx, |term, _cx| {
            term.kill_active_task();
        });

        // Content should still be there
        let content = terminal.update(cx, |term, _| term.get_content());
        assert!(
            content.contains("done"),
            "Output should still be present after no-op kill, got: {content}"
        );
    }

    mod perf {
        use super::super::*;
        use gpui::{
            Entity, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualContext,
            VisualTestContext, point,
        };
        use util::default;
        use util_macros::perf;

        async fn init_scroll_perf_test(
            cx: &mut TestAppContext,
        ) -> (Entity<Terminal>, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings_store = settings::SettingsStore::test(cx);
                cx.set_global(settings_store);
            });

            cx.executor().allow_parking();

            let window = cx.add_empty_window();
            let builder = window
                .update(|window, cx| {
                    let settings = TerminalSettings::get_global(cx);
                    let test_path_hyperlink_timeout_ms = 100;
                    TerminalBuilder::new(
                        None,
                        None,
                        util::shell::Shell::System,
                        HashMap::default(),
                        SettingsCursorShape::default(),
                        AlternateScroll::On,
                        None,
                        settings.path_hyperlink_regexes.clone(),
                        test_path_hyperlink_timeout_ms,
                        false,
                        window.window_handle().window_id().as_u64(),
                        None,
                        cx,
                        vec![],
                        PathStyle::local(),
                    )
                })
                .await
                .unwrap();
            let terminal = window.new(|cx| builder.subscribe(cx));

            terminal.update(window, |term, cx| {
                term.write_output("long line ".repeat(1000).as_bytes(), cx);
            });

            (terminal, window)
        }

        #[perf]
        #[gpui::test]
        async fn scroll_long_line_benchmark(cx: &mut TestAppContext) {
            let (terminal, window) = init_scroll_perf_test(cx).await;
            let wobble = point(FIND_HYPERLINK_THROTTLE_PX, px(0.0));
            let mut scroll_by = |lines: i32| {
                window.update_window_entity(&terminal, |terminal, window, cx| {
                    let bounds = terminal.last_content.terminal_bounds.bounds;
                    let center = bounds.origin + bounds.center();
                    let position = center + wobble * lines as f32;

                    terminal.mouse_move(
                        &MouseMoveEvent {
                            position,
                            ..default()
                        },
                        cx,
                    );

                    terminal.scroll_wheel(
                        &ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Lines(GpuiPoint::new(0.0, lines as f32)),
                            ..default()
                        },
                        1.0,
                    );

                    assert!(
                        terminal
                            .events
                            .iter()
                            .any(|event| matches!(event, InternalEvent::Scroll(_))),
                        "Should have Scroll event when scrolling within terminal bounds"
                    );
                    terminal.sync(window, cx);
                });
            };

            for _ in 0..20000 {
                scroll_by(1);
                scroll_by(-1);
            }
        }

        #[test]
        fn test_num_lines_float_precision() {
            let line_heights = [
                20.1f32, 16.7, 18.3, 22.9, 14.1, 15.6, 17.8, 19.4, 21.3, 23.7,
            ];
            for &line_height in &line_heights {
                for n in 1..=100 {
                    let height = n as f32 * line_height;
                    let bounds = TerminalBounds::new(
                        px(line_height),
                        px(8.0),
                        Bounds {
                            origin: GpuiPoint::default(),
                            size: Size {
                                width: px(800.0),
                                height: px(height),
                            },
                        },
                    );
                    assert_eq!(
                        bounds.num_lines(),
                        n,
                        "num_lines() should be {n} for height={height}, line_height={line_height}"
                    );
                }
            }
        }

        #[test]
        fn test_num_columns_float_precision() {
            let cell_widths = [8.1f32, 7.3, 9.7, 6.9, 10.1];
            for &cell_width in &cell_widths {
                for n in 1..=200 {
                    let width = n as f32 * cell_width;
                    let bounds = TerminalBounds::new(
                        px(20.0),
                        px(cell_width),
                        Bounds {
                            origin: GpuiPoint::default(),
                            size: Size {
                                width: px(width),
                                height: px(400.0),
                            },
                        },
                    );
                    assert_eq!(
                        bounds.num_columns(),
                        n,
                        "num_columns() should be {n} for width={width}, cell_width={cell_width}"
                    );
                }
            }
        }
    }
}
