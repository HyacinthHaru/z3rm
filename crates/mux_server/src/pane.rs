// §3.1 Pane — PTY + alacritty terminal emulator + grid diff ring.
//
// mux_server 是 server-canonical 模型中终端状态的唯一拥有者 (spec §3.1):
// PTY fd、alacritty Term、scrollback、generation counter 全部在此进程内。
// 客户端只渲染我们 push 过来的 grid diff / snapshot。

use crate::coalescing::{AdaptiveCoalescer, KeyboardActivity};
use crate::dec2026::Dec2026Parser;
use crate::grid_sync::{
    self, FullGridSnapshot, GridDiff, GridDiffRing, diff_from_dirty, modes_from_alacritty,
    snapshot_from_term,
};
use crate::pty::{ChildBox, MasterPtyBox, PtySize};
use crate::rt::mpsc;
use crate::terminal_media::{ScanEvent, ScanOutput, TerminalMediaScanner};
use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    ClearMode, Handler, NamedPrivateMode, PrivateMode, Processor, Rgb, StdSyncHandler,
};
use anyhow::Context as _;
use mux_protocol::Notification as MuxNotification;
use mux_protocol::proto::{PaneAction, PaneMedia};
use parking_lot::Mutex;
#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
use portable_pty::{CommandBuilder, MasterPty, PtyPair, PtySystem};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use web_time::Instant;
/// §16.13 A slow connection setup must not lose typed events. Session-scoped
/// panes retain pending events until both matching hooks are installed; panes
/// without a session retain only events that already have a matching hook.

enum PendingTypedEvent {
    Media(PaneMedia),
    Action(PaneAction),
}

struct PendingTypedEvents {
    queue: VecDeque<PendingTypedEvent>,
    draining: bool,
    retry_requested: bool,
}

/// §3.1 真正拥有 alacritty Term + PTY pair 的 Pane (server-canonical)。
pub struct Pane {
    pub id: String,
    pub cwd: Arc<parking_lot::RwLock<String>>,
    pub title: Arc<parking_lot::RwLock<String>>,
    pub command: Option<String>,
    /// Serializes every render-state mutation with its generation publication.
    /// This lock is always acquired before PTY master, terminal, or diff-ring locks.
    commit: parking_lot::Mutex<()>,
    /// §16.2 Per-client viewport constraints keyed by attached client identity.
    /// The applied pane size is the min-fit across all entries, so the smallest
    /// attached client still sees the whole grid instead of the last resize
    /// request winning. Held across `resize` to serialize concurrent client
    /// reports, so it sits *before* `commit` in the lock order; no path takes
    /// it while holding `commit`.
    client_viewports: parking_lot::Mutex<HashMap<String, PaneViewport>>,
    /// §16.3 Last user-input timestamp, shared with the PTY reader thread's
    /// coalescer so keystrokes select the Interactive (0ms) tier.
    keyboard_activity: KeyboardActivity,
    /// §3.1 alacritty 终端实例 (server-canonical, 真实 VT 解析)。
    pub term: Arc<parking_lot::Mutex<Term<PaneEventListener>>>,
    /// §3.3 generation counter (每次 grid-affecting 变化递增)。
    pub generation: AtomicU64,
    /// Monotonic sequence for raw PaneOutput byte batches. Read/written under
    /// `commit` so fetch_grid_update can return an atomic grid/byte-stream fence.
    output_sequence: AtomicU64,
    /// §3.3 grid diff ring (默认 64 entries)。
    pub grid_diff_ring: Arc<parking_lot::RwLock<GridDiffRing>>,
    pub alive: AtomicBool,
    pub cols: AtomicU64,
    pub rows: AtomicU64,
    pub bracketed_paste_mode: AtomicBool,
    /// §3.3 Pane zoom 状态 (zoomed = 最大化, 隐藏其他 pane)。
    pub zoomed: AtomicBool,
    /// §3.3 OSC 133 prompt marker 计数器。
    pub prompt_marker: AtomicU64,
    /// §3.3 Absolute row id of viewport line 0, counted from pane start.
    ///
    /// Alacritty addresses scrollback as `0..history_size` with 0 = oldest, so
    /// every eviction renumbers every row. This base is advanced by exactly the
    /// number of rows the emulator pushed above the viewport, which makes
    /// `absolute_row - (base - history_size)` a scrollback index that survives
    /// scrolling. Only meaningful together with `row_addressing_epoch`.
    viewport_top_absolute: AtomicU64,
    /// §3.3 Generation of the absolute row numbering.
    ///
    /// Alacritty exposes scrollback growth but not how many rows it evicted, so
    /// any movement whose size cannot be derived from that growth — reflow on
    /// resize, rotation once scrollback is at capacity, RIS, a scrollback clear,
    /// an alternate-screen switch — retires the numbering by bumping this.
    /// Recorded rows then resolve to `Unavailable` instead of to a wrong row.
    row_addressing_epoch: AtomicU64,
    marker_sequence: AtomicU64,
    /// §3.3 Recorded OSC 133 markers, oldest first, capped at
    /// `MAX_RECORDED_SHELL_MARKERS`. Taken after `commit` and `term`.
    shell_markers: parking_lot::Mutex<VecDeque<ShellMarker>>,
    scrollback_capacity: AtomicU64,
    history_version: AtomicU64,
    /// §3.1 PTY master (用于 resize / reader clone)。
    pty_master: Arc<Mutex<MasterPtyBox>>,
    /// §3.1 PTY writer (单一 writer)。
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// §3.5 child 进程 handle (用于 kill/wait)。
    child: Arc<Mutex<Option<ChildBox>>>,
    /// §3.1 What the native reader thread owns on its stack. There is no
    /// reader thread in the browser: bytes arrive through
    /// `push_guest_output`, so the same state lives here instead.
    #[cfg(target_family = "wasm")]
    guest_output_state: parking_lot::Mutex<(Dec2026Parser, AdaptiveCoalescer, ReadLoopState)>,
    /// §3.3 event 收集: alacritty 事件 → main loop。
    pub events: Arc<parking_lot::Mutex<Vec<AlacEvent>>>,
    /// §3.3 Pane notification subscribers keyed by attached client identity.
    /// Re-attach replaces the prior sender; detach removes it synchronously.
    subscribers: Arc<parking_lot::RwLock<HashMap<String, mpsc::UnboundedSender<MuxNotification>>>>,
    /// §3.4 所属 session id (供 spawn_with_session 设置, 普通 spawn 为 None)。
    /// 强引用未持有 Session 因此不会出现循环, Session 删除后 Pane 实例随之 drop。
    session_id: parking_lot::Mutex<Option<String>>,
    /// §3.4 自然退出钩子: PTY EOF 或 alacritty Exit/ChildExit 事件被触发时
    /// 调用一次。连接到 connection 层注册一个 closure, 该 closure 在自己的线程里
    /// 走会话级 lifecycle fan-out 路径广播 PaneRemoved + 从 session.layout /
    /// session.panes 中清理。该字段用 Mutex<Option<...>> 以支持一次性 take,
    /// 避免 EOF 路径 + Exit 事件路径重复广播。
    exit_hook: parking_lot::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// §16.8 Daemon-side observer for pane notifications.
    notification_hook: parking_lot::Mutex<Option<Arc<dyn Fn(MuxNotification) + Send + Sync>>>,
    /// §16.6 Optional hook for ClipboardStore events from the emulator.
    clipboard_hook: parking_lot::Mutex<Option<Box<dyn Fn(String) + Send>>>,
    /// §16.13 Per-pane monotonic sequence shared by media and actions, in
    /// cross-type arrival order. Read/written under `commit`.
    media_sequence: AtomicU64,
    /// §16.13 Observer for each completed `PaneMedia`, in sequence order.
    media_hook: parking_lot::Mutex<Option<Arc<dyn Fn(Vec<PaneMedia>) + Send + Sync>>>,
    /// §16.13 Observer for each `PaneAction`, in sequence order.
    action_hook: parking_lot::Mutex<Option<Arc<dyn Fn(Vec<PaneAction>) + Send + Sync>>>,
    /// §16.13 Typed events buffered while session-scoped hooks are being
    /// installed. The queue is dropped with the pane and never used as an
    /// unobserved queue for panes without a session.
    pending_typed_events: parking_lot::Mutex<PendingTypedEvents>,
}
/// §3.3 Pane 事件收集器 — alacritty `EventListener` 的实现。
///
/// alacritty 在 VT 处理过程中通过 `event_proxy.send_event(...)` 通知 UI
/// 有需要处理的副作用 (title 变化、bell、pty write 请求等)。我们把所有
/// 事件 push 到一个 Vec 里, 由 PTY read loop 在每次 advance() 之后消费。
#[derive(Clone)]
pub struct PaneEventListener {
    pub events: Arc<parking_lot::Mutex<Vec<AlacEvent>>>,
}

impl EventListener for PaneEventListener {
    fn send_event(&self, event: AlacEvent) {
        self.events.lock().push(event);
    }
}

/// §16.2 One attached client's reported viewport for a pane, in grid cells.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaneViewport {
    pub cols: u32,
    pub rows: u32,
}

/// §16.2 Min-fit across every attached client's viewport. Each dimension is
/// minimized independently: a 100x20 and an 80x40 client yield 80x20.
fn min_fit(viewports: &HashMap<String, PaneViewport>) -> Option<PaneViewport> {
    viewports
        .values()
        .copied()
        .reduce(|smallest, viewport| PaneViewport {
            cols: smallest.cols.min(viewport.cols),
            rows: smallest.rows.min(viewport.rows),
        })
}

/// §3.3 The four semantic markers OSC 133 defines for one shell command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellMarkerKind {
    /// `A` — the shell is about to draw a prompt.
    PromptStart,
    /// `B` — the prompt ended; the typed command line starts here.
    CommandStart,
    /// `C` — the command line ended; command output starts here.
    OutputStart,
    /// `D` — the command finished, optionally carrying its exit status.
    CommandEnd,
}

impl ShellMarkerKind {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'A' => Some(Self::PromptStart),
            b'B' => Some(Self::CommandStart),
            b'C' => Some(Self::OutputStart),
            b'D' => Some(Self::CommandEnd),
            _ => None,
        }
    }
}

/// §3.3 One OSC 133 marker recorded at the row the shell emitted it on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellMarker {
    /// 1-based and monotonic for the lifetime of the pane.
    pub sequence: u64,
    pub kind: ShellMarkerKind,
    /// Row id in `Pane::viewport_top_absolute`'s numbering. Resolve it with
    /// `Pane::locate_shell_marker` rather than interpreting it directly.
    pub absolute_row: u64,
    pub column: u32,
    /// Exit status from `OSC 133 ; D ; <status>`. `None` when the shell omitted
    /// it, when the status is unparsable, or for the other marker kinds.
    pub exit_code: Option<i32>,
    /// Row-numbering epoch the id belongs to.
    pub epoch: u64,
}

/// §3.3 Where a recorded marker's row can be addressed right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellMarkerPosition {
    /// Scrollback row, `0` = oldest, same addressing as `fetch_scrollback`.
    History { index: u32 },
    /// On-screen row, `0` = top of the viewport.
    Viewport { line: u32 },
    /// The row was evicted, or its id predates a reflow/reset/uncounted
    /// rotation. Never guess a row here: a wrong row is worse than no row.
    Unavailable,
}

/// Four markers per command, so this keeps roughly the last 256 commands. Sized
/// against the 10 000-line default scrollback: 256 commands of output normally
/// exceed that, so entries usually become unresolvable before they are dropped,
/// and 1024 entries cost about 48 KiB per pane.
const MAX_RECORDED_SHELL_MARKERS: usize = 1024;

/// Rows of scrollback the emulator may hold above the configured capacity while
/// a PTY batch is being parsed.
///
/// `Grid::increase_scroll_limit` clamps growth at the configured limit, so at
/// capacity the emulator evicts exactly as many rows as it appends and reports
/// neither — the distance rows moved becomes unobservable. Parsing against a
/// raised limit and performing the eviction here instead keeps that distance
/// equal to the scrollback growth, at the cost of holding at most this many
/// extra rows while a batch is being parsed. Those rows come out of the spare
/// capacity alacritty already keeps allocated, so this does not add memory.
///
/// Staying under alacritty's `MAX_CACHE_SIZE` (1000) spare-row cache is what
/// keeps every raise/trim pair inside already-allocated storage:
/// `Storage::initialize` reallocates only when the live length passes the raw
/// buffer, and `Storage::shrink_lines` frees only when more than
/// `MAX_CACHE_SIZE` rows sit unused. Crossing that line is not a small
/// regression — a headroom of 8192 measured 6x slower on line-dense output,
/// because every batch then pays a `rezero` plus a realloc and a truncate.
/// The raised limit also caps the rows one step can append, so a trim never
/// has more than this to give back.
///
/// The same value bounds the bytes per parse step. Scrolling driven by line
/// feeds appends at most one row per byte, so a step this size cannot outgrow
/// the headroom. `SU`/`DL` scroll by up to a screen height per sequence and can
/// outrun that; those hit the saturation check in `RowAddressing::advance_to`
/// and retire the numbering instead of drifting.
const ROW_ADDRESSING_HEADROOM: usize = 960;

/// Row-addressing bookkeeping for one PTY batch, threaded through the parse
/// steps that `advance_recording_markers` splits the batch into.
struct RowAddressing {
    /// Absolute row id of viewport line 0, in `Pane::viewport_top_absolute`'s
    /// numbering.
    viewport_top: u64,
    /// Scrollback rows the emulator holds, re-read after every parse step.
    history_size: usize,
    /// Configured scrollback capacity. The emulator is trimmed back to it by
    /// every step that grew past it, and by `finish` before the batch ends, so
    /// nothing outside the read loop ever sees more than this.
    capacity: usize,
    /// Bytes of the batch already handed to the emulator.
    consumed: usize,
    /// Whether the primary grid was active at the end of the last parse step.
    /// The alternate grid has no scrollback of its own, so its size says nothing
    /// about rows leaving the primary viewport.
    primary_screen: bool,
    /// Whether the primary grid currently carries the raised limit. Restoring
    /// the configured limit is what performs the eviction, so the two track
    /// each other.
    headroom_granted: bool,
    /// Set when a parse step filled the raised limit, which is the one case
    /// where the emulator can still have evicted rows it did not report.
    saturated: bool,
}

impl RowAddressing {
    fn raised_limit(&self) -> usize {
        self.capacity.saturating_add(ROW_ADDRESSING_HEADROOM)
    }

    /// Feed the batch up to `offset`, keeping `viewport_top` in step with the
    /// rows the emulator pushed above the viewport.
    fn advance_to(
        &mut self,
        term: &mut Term<PaneEventListener>,
        processor: &mut Processor<StdSyncHandler>,
        bytes: &[u8],
        offset: usize,
    ) {
        let offset = offset.min(bytes.len());
        while self.consumed < offset {
            let primary_before = self.primary_screen;
            if primary_before && !self.headroom_granted {
                term.grid_mut().update_history(self.raised_limit());
                self.headroom_granted = true;
            }
            // A step is capped at the rows the raised limit can still absorb,
            // so line-feed scrolling cannot outgrow it mid-step. Batches
            // shorter than that — every interactive one — are still fed whole,
            // and so is anything arriving while a full-screen app holds the
            // alternate grid, which has no scrollback to account for.
            let room = if primary_before {
                self.raised_limit().saturating_sub(self.history_size).max(1)
            } else {
                usize::MAX
            };
            let step_end = offset.min(self.consumed.saturating_add(room));
            processor.advance(&mut *term, &bytes[self.consumed..step_end]);
            self.consumed = step_end;

            self.primary_screen = !term.mode().contains(TermMode::ALT_SCREEN);
            if !self.primary_screen {
                // The primary grid is now inactive and out of reach, so it keeps
                // the raised limit until the batch that switches back settles it.
                continue;
            }
            let grown = term.grid().history_size();
            if primary_before {
                if grown >= self.raised_limit() {
                    self.saturated = true;
                }
                self.viewport_top = self
                    .viewport_top
                    .saturating_add(grown.saturating_sub(self.history_size) as u64);
            }
            self.history_size = grown;
            if grown > self.capacity {
                self.evict_down_to_capacity(term);
            }
        }
    }

    /// Restore the configured limit, which drops exactly the oldest rows above
    /// it. Doing the eviction here rather than letting the emulator do it
    /// silently is what keeps the movement measurable; only the oldest rows go,
    /// so the viewport does not move and `viewport_top` still holds.
    fn evict_down_to_capacity(&mut self, term: &mut Term<PaneEventListener>) {
        term.grid_mut().update_history(self.capacity);
        self.headroom_granted = false;
        self.history_size = term.grid().history_size();
    }

    /// Hand the configured limit back before anything outside the read loop can
    /// observe the grid, so the raised limit never outlives one batch. This runs
    /// unconditionally on the primary grid rather than only when this batch
    /// raised the limit, because a batch that switched to the alternate screen
    /// leaves the primary grid raised and out of reach until one does.
    fn finish(&mut self, term: &mut Term<PaneEventListener>) {
        if self.primary_screen {
            self.evict_down_to_capacity(term);
        }
    }
}

/// §3.10 Shell command (从 proto ShellCommand 转换)
#[derive(Clone, Debug, Default)]
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

pub(crate) struct PaneMetadataSnapshot {
    pub title: String,
    pub generation: u64,
    pub cols: u32,
    pub rows: u32,
    pub is_alive: bool,
    pub zoomed: bool,
}

#[derive(Default)]
struct HistoryMutationObserver {
    may_rotate: bool,
    /// Set by the operations that discard or renumber scrollback wholesale, as
    /// opposed to appending to it. Their size is not derivable from scrollback
    /// growth, so they retire the absolute row numbering.
    may_break_addressing: bool,
}

impl HistoryMutationObserver {
    fn reset(&mut self) {
        self.may_rotate = false;
        self.may_break_addressing = false;
    }

    fn mark_rotation(&mut self) {
        self.may_rotate = true;
    }

    fn mark_addressing_break(&mut self) {
        self.may_break_addressing = true;
    }
}

impl Handler for HistoryMutationObserver {
    fn input(&mut self, _: char) {}

    fn linefeed(&mut self) {
        self.mark_rotation();
    }

    fn newline(&mut self) {
        self.mark_rotation();
    }

    fn scroll_up(&mut self, _: usize) {
        self.mark_rotation();
    }

    fn scroll_down(&mut self, _: usize) {
        self.mark_rotation();
    }

    fn insert_blank_lines(&mut self, _: usize) {
        self.mark_rotation();
    }

    fn delete_lines(&mut self, _: usize) {
        self.mark_rotation();
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        // CSI 2J (ClearMode::All) 在主屏把整屏内容滚入历史
        // (alacritty grid::clear_viewport), 满容量时原地轮换历史而不改变
        // history_size; CSI 3J (ClearMode::Saved) 清空整个历史。两者都可能
        // 在 size 不变的情况下改变历史内容, 必须标记为 rotation。
        if matches!(mode, ClearMode::All | ClearMode::Saved) {
            self.mark_rotation();
        }
        if matches!(mode, ClearMode::Saved) {
            self.mark_addressing_break();
        }
    }

    fn reset_state(&mut self) {
        // RIS 清空 grid 与整个 scrollback; history_size 归零已命中
        // size-change 分支, 但显式标记避免任何"reset 不动历史"的错误假设。
        self.mark_rotation();
        self.mark_addressing_break();
    }

    fn reverse_index(&mut self) {
        self.mark_rotation();
    }

    fn set_color(&mut self, _: usize, _: Rgb) {}

    fn reset_color(&mut self, _: usize) {}

    fn decaln(&mut self) {}

    fn set_private_mode(&mut self, mode: PrivateMode) {
        // DECCOLM reflows the grid and 1049 swaps to a grid with no scrollback;
        // both renumber rows by an amount scrollback growth cannot express.
        if matches!(
            mode,
            PrivateMode::Named(
                NamedPrivateMode::ColumnMode | NamedPrivateMode::SwapScreenAndSetRestoreCursor
            )
        ) {
            self.mark_addressing_break();
        }
    }

    fn unset_private_mode(&mut self, _: PrivateMode) {}
}

/// §3.3 One OSC sequence the mux consumes itself.
#[derive(Debug)]
enum OscEvent {
    /// OSC 7 payload, the raw `file://HOST/PATH` URI.
    Cwd(String),
    /// OSC 133 marker plus the batch offset just past its terminator. The
    /// emulator must be advanced exactly that far before the cursor is read,
    /// otherwise the marker lands on whatever row the batch happened to end on.
    ShellMarker {
        kind: ShellMarkerKind,
        exit_code: Option<i32>,
        end_offset: usize,
    },
}

/// Payload bytes kept for one OSC. Long enough for any real cwd path; a longer
/// payload is dropped rather than buffered, so a hostile PTY cannot grow this.
const MAX_OSC_PAYLOAD: usize = 4096;
const MAX_OSC_NUMBER_DIGITS: u32 = 5;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum OscScanState {
    #[default]
    Ground,
    Escape,
    Number,
    Payload,
    PayloadEscape,
    Skip,
    SkipEscape,
}

/// §3.3 Incremental scanner for the OSC sequences the mux consumes itself.
///
/// Alacritty's `Handler` has no hook for OSC 7 or OSC 133 — both fall into
/// vte's `unhandled` path — so the read loop scans the same bytes. The state
/// lives across PTY reads because a real PTY splits sequences at arbitrary byte
/// boundaries; a scanner restarted per batch silently loses those markers.
#[derive(Default)]
struct OscScanner {
    state: OscScanState,
    number: u32,
    digits: u32,
    payload: Vec<u8>,
    overflowed: bool,
}

impl OscScanner {
    fn scan(&mut self, bytes: &[u8], events: &mut Vec<OscEvent>) {
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            index += 1;
            match self.state {
                OscScanState::Ground => {
                    if byte == 0x1b {
                        self.state = OscScanState::Escape;
                    }
                }
                OscScanState::Escape => match byte {
                    b']' => self.start_sequence(),
                    0x1b => {}
                    _ => self.state = OscScanState::Ground,
                },
                OscScanState::Number => match byte {
                    b'0'..=b'9' => {
                        self.digits += 1;
                        if self.digits > MAX_OSC_NUMBER_DIGITS {
                            self.state = OscScanState::Skip;
                        } else {
                            self.number = self.number * 10 + u32::from(byte - b'0');
                        }
                    }
                    b';' if self.digits > 0 && matches!(self.number, 7 | 133) => {
                        self.state = OscScanState::Payload;
                    }
                    0x07 => self.state = OscScanState::Ground,
                    0x1b => self.state = OscScanState::SkipEscape,
                    _ => self.state = OscScanState::Skip,
                },
                OscScanState::Payload => match byte {
                    0x07 => self.finish(index, events),
                    0x1b => self.state = OscScanState::PayloadEscape,
                    _ => {
                        if self.payload.len() < MAX_OSC_PAYLOAD {
                            self.payload.push(byte);
                        } else {
                            self.overflowed = true;
                        }
                    }
                },
                OscScanState::PayloadEscape => {
                    if byte == b'\\' {
                        self.finish(index, events);
                    } else {
                        // An ESC that is not ST aborts the OSC and starts a new
                        // sequence, so re-dispatch this byte after the ESC.
                        self.abort();
                        self.state = OscScanState::Escape;
                        index -= 1;
                    }
                }
                OscScanState::Skip => match byte {
                    0x07 => self.state = OscScanState::Ground,
                    0x1b => self.state = OscScanState::SkipEscape,
                    _ => {}
                },
                OscScanState::SkipEscape => {
                    if byte == b'\\' {
                        self.state = OscScanState::Ground;
                    } else {
                        self.state = OscScanState::Escape;
                        index -= 1;
                    }
                }
            }
        }
    }

    fn start_sequence(&mut self) {
        self.number = 0;
        self.digits = 0;
        self.payload.clear();
        self.overflowed = false;
        self.state = OscScanState::Number;
    }

    fn abort(&mut self) {
        self.payload.clear();
        self.overflowed = false;
        self.state = OscScanState::Ground;
    }

    fn finish(&mut self, end_offset: usize, events: &mut Vec<OscEvent>) {
        if !self.overflowed {
            match self.number {
                7 => {
                    if let Ok(uri) = std::str::from_utf8(&self.payload) {
                        events.push(OscEvent::Cwd(uri.to_string()));
                    }
                }
                133 => {
                    if let Some((kind, exit_code)) = parse_osc133_payload(&self.payload) {
                        events.push(OscEvent::ShellMarker {
                            kind,
                            exit_code,
                            end_offset,
                        });
                    }
                }
                _ => {}
            }
        }
        self.abort();
    }
}

/// Parse the parameters after `OSC 133 ;`. The first field is the marker
/// letter; `D` may carry the command's exit status as its second field.
fn parse_osc133_payload(payload: &[u8]) -> Option<(ShellMarkerKind, Option<i32>)> {
    let mut fields = payload.split(|byte| *byte == b';');
    let kind_field = fields.next()?;
    let [kind_byte] = kind_field else {
        return None;
    };
    let kind = ShellMarkerKind::from_byte(*kind_byte)?;
    if kind != ShellMarkerKind::CommandEnd {
        return Some((kind, None));
    }
    let exit_code = fields
        .next()
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.trim().parse::<i32>().ok());
    Some((kind, exit_code))
}

#[derive(Clone, Copy)]
struct PendingMediaCursor {
    image_id: u32,
    row: i32,
    column: u32,
}

/// §3.3 PTY read-loop 本地状态: DEC-2026 同步延迟 + coalescing 通知节流。
/// 仅在单一 PTY read 线程内顺序访问, 无需同步原语。
pub(crate) struct ReadLoopState {
    /// Persistent parsers preserve escape sequences split across PTY reads.
    terminal_processor: Processor<StdSyncHandler>,
    history_processor: Processor<StdSyncHandler>,
    history_observer: HistoryMutationObserver,
    osc_scanner: OscScanner,
    /// Reused so a batch without OSC 7 / OSC 133 allocates nothing.
    osc_events: Vec<OscEvent>,
    /// BSU..ESU 同步窗口内累积了尚未发布的变更
    pending_sync: bool,
    /// Dirty rows accumulated across a DEC-2026 synchronized update window.
    pending_dirty_rows: Vec<usize>,
    /// Whether the window changed state absent from row diffs.
    pending_full_snapshot: bool,
    /// 有被 coalescing 推迟、待窗口到期补发的 PaneDirty
    pending_notify: bool,
    /// §16.13 Server-side Kitty / OSC 9 scanner for this pane's byte stream.
    /// State persists across feeds (sequences split at PTY read boundaries).
    terminal_scanner: TerminalMediaScanner,
    /// §16.13 Event ledger from the last batch: media/action arrival order
    /// plus each event's grid-byte offset, so cursor placement and dispatch
    /// can be merged with the OSC 133 marker walk without guessing.
    scan_events: Vec<ScanEvent>,
    /// §16.13 Completed media from the last batch, indexed by `scan_events`.
    media: Vec<PaneMedia>,
    /// §16.13 Completed actions from the last batch, indexed by `scan_events`.
    actions: Vec<PaneAction>,
    /// Cursor captured at the initial `m=1,a=T` chunk, retained until the
    /// matching final media event arrives (which may be a later PTY feed).
    pending_media_cursor: Option<PendingMediaCursor>,
}

impl Default for ReadLoopState {
    fn default() -> Self {
        Self {
            terminal_processor: Processor::new(),
            history_processor: Processor::new(),
            history_observer: HistoryMutationObserver::default(),
            osc_scanner: OscScanner::default(),
            osc_events: Vec::new(),
            pending_sync: false,
            pending_dirty_rows: Vec::new(),
            pending_full_snapshot: false,
            pending_notify: false,
            terminal_scanner: TerminalMediaScanner::new(),
            pending_media_cursor: None,
            scan_events: Vec::new(),
            media: Vec::new(),
            actions: Vec::new(),
        }
    }
}

#[cfg(target_family = "wasm")]
static NEXT_WASM_HISTORY_VERSION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn initial_history_version() -> u64 {
    #[cfg(all(
        not(target_family = "wasm"),
        any(feature = "desktop", feature = "guest")
    ))]
    {
        use std::hash::{DefaultHasher, Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        nanoid::nanoid!().hash(&mut hasher);
        hasher.finish().max(1)
    }
    #[cfg(target_family = "wasm")]
    {
        NEXT_WASM_HISTORY_VERSION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}

impl Pane {
    /// §3.10 创建新 pane: spawn PTY + alacritty Term + 启动 read loop。
    ///
    /// 返回 Arc 因为 PTY read 线程持有弱引用, pane drop 时自动结束。
    ///
    /// Scrollback capacity comes from `ServerSettings::scrollback_lines()` via
    /// the connection layer so new panes honor a live `server.json` value, not
    /// just the `Z3RM_SCROLLBACK_LINES` env snapshot at boot. This `spawn` entry
    /// point (used by tests and `Pane::spawn_with_session` fallbacks) falls back
    /// to `default_scrollback_lines()` when no live settings are threaded in.
    pub fn spawn(
        id: String,
        cwd: String,
        cols: u32,
        rows: u32,
        command: Option<ShellCommand>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::spawn_with_session(
            id,
            String::new(),
            cwd,
            cols,
            rows,
            command,
            crate::server_settings::default_scrollback_lines(),
        )
    }

    /// §3.10 / §16.11 Create a pane bound to a session.
    ///
    /// `scrollback_lines` is the live capacity from `ServerSettings` (env +
    /// `server.json`, hot-reloaded); the caller in `connection.rs` forwards
    /// `settings.scrollback_lines()`. Passing it explicitly (rather than
    /// re-reading the env here) is what lets a daemon-wide capacity change take
    /// effect for every subsequently spawned pane without a restart.
    pub fn spawn_with_session(
        id: String,
        session_id: String,
        cwd: String,
        cols: u32,
        rows: u32,
        command: Option<ShellCommand>,
        scrollback_lines: usize,
    ) -> anyhow::Result<Arc<Self>> {
        let scrollback_lines = scrollback_lines.min(100_000);
        let cols_usize = usize::try_from(cols).context("pane column count exceeds host limit")?;
        let rows_usize = usize::try_from(rows).context("pane row count exceeds host limit")?;
        mux_protocol::checked_grid_cell_count(cols_usize, rows_usize)
            .map_err(|message| anyhow::anyhow!("invalid pane size {cols}x{rows}: {message}"))?;
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let listener = PaneEventListener {
            events: events.clone(),
        };

        let term_config = TermConfig {
            scrolling_history: scrollback_lines,
            ..TermConfig::default()
        };
        let size = TermSize::new(cols_usize, rows_usize);
        let term = Term::new(term_config, &size, listener);

        #[cfg(all(
            not(target_family = "wasm"),
            any(feature = "desktop", feature = "guest")
        ))]
        let (pty_master, writer, child, master_raw_fd, reader) = {
            // §3.1 打开 PTY pair
            let pty_system: Box<dyn PtySystem + Send> = portable_pty::native_pty_system();
            let pair: PtyPair = pty_system.openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })?;

            // §3.10 构建 shell 命令 (默认用 user shell 或 /bin/sh)
            let mut cmd = if let Some(ref c) = command {
                let mut builder = CommandBuilder::new(&c.program);
                for arg in &c.args {
                    builder.arg(arg);
                }
                for (k, v) in &c.env {
                    builder.env(k, v);
                }
                builder
            } else {
                // 默认: $SHELL, 若未设置则 /bin/sh
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                crate::shell_integration::default_shell_command(&shell)
            };

            // §3.1 设置 cwd
            let cwd_path = if cwd.is_empty() {
                dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"))
            } else {
                std::path::PathBuf::from(&cwd)
            };
            cmd.cwd(cwd_path);

            // §3.1 标准终端环境变量
            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");
            cmd.env("Z3RM_PANE_ID", &id);
            cmd.env("Z3RM_PANE", &id);
            if !session_id.is_empty() {
                cmd.env("Z3RM_SESSION", &session_id);
            }

            // §3.1 spawn 子进程
            let child = pair.slave.spawn_command(cmd)?;

            // §3.1 获取 reader / writer
            let reader = pair.master.try_clone_reader()?;
            let writer = pair.master.take_writer()?;
            // §3.3 raw fd for poll-based BSU timeout (None on platforms without it).
            let master_raw_fd = pair.master.as_raw_fd().map(|fd| fd as i32);

            // slave 端已经不需要了 (drop 让 child 持有)
            drop(pair.slave);
            (
                pair.master as crate::pty::MasterPtyBox,
                writer,
                Some(child as crate::pty::ChildBox),
                master_raw_fd,
                reader,
            )
        };

        // §3.1 The browser has no pty and no child: the guest's bytes are
        // pushed in from JS (#56) and pane writes go back out the same way.
        // `command` and `cwd` are the guest's business, not this side's.
        #[cfg(target_family = "wasm")]
        let (pty_master, writer, child, master_raw_fd) = {
            let _ = (&command, &cwd);
            let pty = crate::pty::WasmPty::new();
            pty.resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            let writer = pty.writer();
            (
                Box::new(pty) as crate::pty::MasterPtyBox,
                writer,
                None::<crate::pty::ChildBox>,
                None::<i32>,
            )
        };

        let command_str = command
            .as_ref()
            .map(|c| format!("{} {}", c.program, c.args.join(" ")));

        let pane = Arc::new(Pane {
            id: id.clone(),
            cwd: Arc::new(parking_lot::RwLock::new(cwd)),
            commit: parking_lot::Mutex::new(()),
            client_viewports: parking_lot::Mutex::new(HashMap::new()),
            keyboard_activity: KeyboardActivity::new(),
            title: Arc::new(parking_lot::RwLock::new(String::new())),
            command: command_str,
            term: Arc::new(parking_lot::Mutex::new(term)),
            generation: AtomicU64::new(0),
            output_sequence: AtomicU64::new(0),
            grid_diff_ring: Arc::new(parking_lot::RwLock::new(GridDiffRing::new(64))),
            alive: AtomicBool::new(true),
            cols: AtomicU64::new(cols as u64),
            rows: AtomicU64::new(rows as u64),
            bracketed_paste_mode: AtomicBool::new(false),
            zoomed: AtomicBool::new(false),
            prompt_marker: AtomicU64::new(0),
            viewport_top_absolute: AtomicU64::new(0),
            // Epoch 0 is reserved so a default-constructed marker can never
            // match a live pane's numbering.
            row_addressing_epoch: AtomicU64::new(1),
            marker_sequence: AtomicU64::new(0),
            shell_markers: parking_lot::Mutex::new(VecDeque::new()),
            scrollback_capacity: AtomicU64::new(scrollback_lines as u64),
            // A random non-zero authority epoch prevents a client from reusing
            // cached history after the daemon reconstructs this pane.
            history_version: AtomicU64::new(initial_history_version()),
            pty_master: Arc::new(Mutex::new(pty_master)),
            pty_writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(child)),
            events,
            subscribers: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            // §3.4 spawn_with_session 携带的 session_id 让 PTY read loop 在自然退出
            // 时能定位会话级 lifecycle 订阅者; 空字符串表示未连接会话, 等价于 None。
            session_id: parking_lot::Mutex::new(if session_id.is_empty() {
                None
            } else {
                Some(session_id)
            }),
            exit_hook: parking_lot::Mutex::new(None),
            clipboard_hook: parking_lot::Mutex::new(None),
            notification_hook: parking_lot::Mutex::new(None),
            media_sequence: AtomicU64::new(0),
            media_hook: parking_lot::Mutex::new(None),
            action_hook: parking_lot::Mutex::new(None),
            pending_typed_events: parking_lot::Mutex::new(PendingTypedEvents {
                queue: VecDeque::new(),
                draining: false,
                retry_requested: false,
            }),
            #[cfg(target_family = "wasm")]
            guest_output_state: parking_lot::Mutex::new((
                Dec2026Parser::new(),
                AdaptiveCoalescer::new(),
                ReadLoopState::default(),
            )),
        });

        // §3.1 启动 PTY read loop — 后台线程持续读取 PTY 输出, 喂给 alacritty,
        // 计算 dirty diff, bump generation。线程持有弱引用, pane drop 时自动结束。
        #[cfg(all(
            not(target_family = "wasm"),
            any(feature = "desktop", feature = "guest")
        ))]
        pane.clone().start_pty_read_loop(reader, master_raw_fd);
        #[cfg(target_family = "wasm")]
        let _ = master_raw_fd;
        Ok(pane)
    }

    /// §3.1 启动 PTY read 后台线程。
    ///
    /// 该线程持续从 PTY 读取字节, 喂给 alacritty Term, 然后从 dirty_lines
    /// 提取变更行, 生成 GridDiff, push 到 ring 并 bump generation。
    /// Bump generation 后由 connection 层 fan-out PaneDirty 通知到所有 client。
    #[cfg(all(
        not(target_family = "wasm"),
        any(feature = "desktop", feature = "guest")
    ))]
    fn start_pty_read_loop(
        self: Arc<Self>,
        mut reader: Box<dyn Read + Send>,
        master_raw_fd: Option<i32>,
    ) {
        let pane_weak = Arc::downgrade(&self);
        // §16.3 The coalescer reads keystroke activity recorded on connection
        // tasks, so it must share this pane's handle rather than own its own.
        let keyboard_activity = self.keyboard_activity.clone();

        if let Err(error) = std::thread::Builder::new()
            .name(format!("pty-read-{}", self.id))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                let mut dec = Dec2026Parser::new();
                let mut coalescer = AdaptiveCoalescer::with_keyboard_activity(keyboard_activity);
                let mut state = ReadLoopState::default();
                loop {
                    let Some(pane) = pane_weak.upgrade() else {
                        return;
                    };

                    // §3.3: while BSU is open, poll the master fd so a quiet PTY
                    // still hits the 100ms force-flush without waiting for more bytes.
                    let poll_ms: i32 = if dec.is_in_sync() { 25 } else { 250 };
                    let readable = match master_raw_fd {
                        Some(fd) => poll_fd_readable(fd, poll_ms),
                        None => true,
                    };

                    if !readable {
                        if dec.check_timeout() {
                            pane.force_flush_after_bsu_timeout(&mut coalescer, &mut state);
                        } else {
                            pane.flush_pending_notify(&mut state, &mut coalescer);
                        }
                        continue;
                    }

                    match reader.read(&mut buf) {
                        Ok(0) => {
                            pane.set_alive(false);
                            pane.fire_exit_hook();
                            return;
                        }
                        Ok(count) => {
                            pane.process_pty_bytes(
                                &buf[..count],
                                &mut dec,
                                &mut coalescer,
                                &mut state,
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if dec.check_timeout() {
                                pane.force_flush_after_bsu_timeout(&mut coalescer, &mut state);
                            }
                        }
                        Err(error) => {
                            tracing::error!(pane_id = %pane.id, error = %error, "PTY read failed");
                            pane.set_alive(false);
                            pane.fire_exit_hook();
                            return;
                        }
                    }
                }
            })
        {
            tracing::error!(pane_id = %self.id, error = %error, "failed to spawn PTY reader");
            self.set_alive(false);
        }
    }

    /// Feed one PTY byte batch into the server-owned emulator and publish one
    /// coherent grid generation outside DEC-2026 synchronized-update windows.
    pub(crate) fn process_pty_bytes(
        self: &Arc<Self>,
        bytes: &[u8],
        dec: &mut Dec2026Parser,
        coalescer: &mut AdaptiveCoalescer,
        state: &mut ReadLoopState,
    ) {
        // §16.13 Strip Kitty APC and consumed OSC 9 actions before any
        // DEC-2026, OSC, history, or alacritty processor sees a byte.
        // `grid_bytes` is the only stream advanced below; media/actions travel
        // through the event ledger and are dispatched after the batch commits.
        let ScanOutput {
            grid_bytes,
            media,
            actions,
            events,
        } = state.terminal_scanner.feed(bytes);
        state.scan_events = events;
        state.media = media;
        state.actions = actions;

        let transitions = dec.parse(&grid_bytes);
        let in_sync = dec.is_in_sync();
        // §16.3 Re-classify on the grid-byte volume of this batch before any
        // notification decision uses the resulting window.
        coalescer.on_output(grid_bytes.len());
        self.flush_pending_notify(state, coalescer);

        // §3.3 OSC 7 / OSC 133 are scanned on the grid-safe stream so their
        // offsets stay grid-relative: a marker belongs to the cursor row at its
        // own byte offset, and one batch routinely spans a prompt, a command
        // line and its output. Kitty / OSC 9 bytes are absent from `grid_bytes`,
        // so offsets here address the same stream the emulator consumes.
        state.osc_events.clear();
        state.osc_scanner.scan(&grid_bytes, &mut state.osc_events);

        let commit = self.commit.lock();
        state.history_observer.reset();
        state
            .history_processor
            .advance(&mut state.history_observer, &grid_bytes);
        let (
            render_state_changed,
            history_size_before,
            history_size_after,
            cursor_row_unchanged,
            modes_after,
            alt_screen_changed,
            addressing,
            media_cursor,
        ) = {
            let mut term = self.term.lock();
            let before = (
                term.grid().cursor.point,
                term.cursor_style(),
                term.grid().display_offset(),
                modes_from_alacritty(*term.mode()),
            );
            let history_size_before = term.grid().history_size();
            let alt_screen_before = term.mode().contains(TermMode::ALT_SCREEN);
            let (addressing, media_cursor) = self.advance_recording_markers(
                &mut term,
                &grid_bytes,
                &state.osc_events,
                &state.scan_events,
                &mut state.terminal_processor,
                &mut state.pending_media_cursor,
            );
            let after = (
                term.grid().cursor.point,
                term.cursor_style(),
                term.grid().display_offset(),
                modes_from_alacritty(*term.mode()),
            );
            (
                before != after,
                history_size_before,
                term.grid().history_size(),
                before.0.line == after.0.line,
                after.3,
                alt_screen_before != term.mode().contains(TermMode::ALT_SCREEN),
                addressing,
                media_cursor,
            )
        };
        self.set_bracketed_paste_mode(
            modes_after & mux_protocol::terminal_mode::BRACKETED_PASTE != 0,
        );
        // §16.13 Apply cursor-cell placement captured at each Kitty event's
        // exact grid-byte offset. Only `a=T` places at the cursor; other
        // actions keep row/column at their protocol default (0).
        for (index, row, column) in media_cursor {
            if let Some(media) = state.media.get_mut(index) {
                media.row = row;
                media.column = column;
            }
        }
        let (dirty_rows, _fully_damaged) = self.collect_dirty_rows();
        // A VTE scroll can rotate a full history ring without changing its
        // length. Ordinary input and color changes do not invalidate the
        // history checkpoint; only a size change, a possible rotation at the
        // configured capacity, or an operation that readdresses rows does.
        let history_capacity = self.scrollback_capacity.load(Ordering::Acquire);
        let history_changed = history_size_before != history_size_after
            || (history_capacity > 0
                && history_size_after as u64 >= history_capacity
                && cursor_row_unchanged
                && state.history_observer.may_rotate)
            || (history_size_after != 0 && state.history_observer.may_break_addressing);
        if history_changed {
            self.history_version.fetch_add(1, Ordering::AcqRel);
        }
        self.commit_row_addressing(
            &addressing,
            history_size_before,
            history_size_after,
            alt_screen_changed,
            state.history_observer.may_break_addressing,
        );

        let grid_changed = !dirty_rows.is_empty() || render_state_changed || history_changed;
        // §16.13 A media add/delete is render-affecting even when it leaves
        // the grid untouched (Kitty bytes are stripped), so it advances the
        // pane generation under the same commit fence as PTY output.
        let has_media = state
            .scan_events
            .iter()
            .any(|event| matches!(event, ScanEvent::Media { .. }));
        let should_broadcast_dirty = if in_sync && !transitions.ended() {
            if grid_changed || has_media {
                state.pending_sync = true;
                state.pending_dirty_rows.extend(dirty_rows);
                state.pending_full_snapshot |= render_state_changed || history_changed;
            }
            false
        } else if grid_changed || has_media || state.pending_sync {
            let mut all_dirty_rows = std::mem::take(&mut state.pending_dirty_rows);
            all_dirty_rows.extend(dirty_rows);
            all_dirty_rows.sort_unstable();
            all_dirty_rows.dedup();
            let requires_full_snapshot = std::mem::take(&mut state.pending_full_snapshot)
                || render_state_changed
                || history_changed;
            let should_broadcast = self.emit_generation(
                all_dirty_rows,
                requires_full_snapshot,
                transitions.ended(),
                coalescer,
                state,
            );
            state.pending_sync = false;
            should_broadcast
        } else {
            false
        };
        // Advance the raw-byte fence only after the authoritative emulator and
        // generation ring include this entire PTY batch. fetch_grid_update takes
        // the same commit lock, so its fence is an atomic grid/stream checkpoint.
        let output_sequence = self.advance_output_sequence();
        drop(commit);

        // §16.13 Deliver completed media/actions in cross-type arrival order,
        // after the batch's emulator/generation state is committed and with
        // no lock held.
        self.dispatch_media_events(state);
        self.broadcast_pane_output(bytes, output_sequence);
        self.handle_pending_events();
        for event in &state.osc_events {
            if let OscEvent::Cwd(uri) = event {
                self.handle_osc7_cwd(uri);
            }
        }
        if should_broadcast_dirty {
            self.broadcast_pane_dirty();
        }
    }

    /// Feed one batch to the emulator, pausing at every OSC 133 marker so the
    /// marker's row is read at the byte offset it arrived on, and return the
    /// batch's row-addressing bookkeeping plus the cursor cell captured at each
    /// `a=T` Kitty event's grid-byte offset.
    ///
    /// Rows pushed above the viewport are counted as scrollback growth. The
    /// emulator would stop growing once scrollback reaches capacity, so the
    /// batch is parsed against a raised limit and trimmed back down here; see
    /// `ROW_ADDRESSING_HEADROOM`.
    fn advance_recording_markers(
        &self,
        term: &mut Term<PaneEventListener>,
        bytes: &[u8],
        events: &[OscEvent],
        media_events: &[ScanEvent],
        processor: &mut Processor<StdSyncHandler>,
        pending_media_cursor: &mut Option<PendingMediaCursor>,
    ) -> (RowAddressing, Vec<(usize, i32, u32)>) {
        let mut addressing = RowAddressing {
            viewport_top: self.viewport_top_absolute.load(Ordering::Acquire),
            history_size: term.grid().history_size(),
            capacity: self.scrollback_capacity.load(Ordering::Acquire) as usize,
            consumed: 0,
            primary_screen: !term.mode().contains(TermMode::ALT_SCREEN),
            headroom_granted: false,
            saturated: false,
        };
        let epoch = self.row_addressing_epoch.load(Ordering::Acquire);

        // §16.13 Merge OSC 133 marker boundaries with Kitty event offsets into
        // one sorted walk. A continuation's final media event is retained as
        // a boundary even though its cursor was captured by an earlier event;
        // this lets pending cursor state be consumed in exact event order.
        enum Boundary {
            Marker {
                kind: ShellMarkerKind,
                exit_code: Option<i32>,
            },
            Placement {
                image_id: u32,
                action: Option<u8>,
            },
            Media {
                index: usize,
                image_id: u32,
                action: Option<u8>,
                placement_from_pending: bool,
            },
        }
        let mut boundaries: Vec<(usize, Boundary)> = Vec::new();
        for event in events {
            if let OscEvent::ShellMarker {
                kind,
                exit_code,
                end_offset,
            } = event
            {
                boundaries.push((
                    *end_offset,
                    Boundary::Marker {
                        kind: *kind,
                        exit_code: *exit_code,
                    },
                ));
            }
        }
        for event in media_events {
            match event {
                ScanEvent::Placement {
                    image_id,
                    grid_offset,
                    action,
                } => boundaries.push((
                    *grid_offset,
                    Boundary::Placement {
                        image_id: *image_id,
                        action: *action,
                    },
                )),
                ScanEvent::Media {
                    index,
                    image_id,
                    grid_offset,
                    action,
                    placement_from_pending,
                } => boundaries.push((
                    *grid_offset,
                    Boundary::Media {
                        index: *index,
                        image_id: *image_id,
                        action: *action,
                        placement_from_pending: *placement_from_pending,
                    },
                )),
                ScanEvent::Action { .. } => {}
            }
        }
        boundaries.sort_by_key(|(offset, _)| *offset);

        let mut media_cursor: Vec<(usize, i32, u32)> = Vec::new();
        for (offset, boundary) in boundaries {
            addressing.advance_to(term, processor, bytes, offset);
            match boundary {
                Boundary::Marker { kind, exit_code } => {
                    self.record_shell_marker(term, kind, exit_code, addressing.viewport_top, epoch);
                }
                Boundary::Placement { image_id, action } => {
                    if action == Some(b'T') {
                        let cursor = term.grid().cursor.point;
                        *pending_media_cursor = Some(PendingMediaCursor {
                            image_id,
                            row: cursor.line.0,
                            column: u32::try_from(cursor.column.0).unwrap_or(u32::MAX),
                        });
                    }
                }
                Boundary::Media {
                    index,
                    image_id,
                    action,
                    placement_from_pending,
                } => {
                    if action != Some(b'T') {
                        continue;
                    }
                    if placement_from_pending {
                        if let Some(cursor) = pending_media_cursor.take() {
                            if cursor.image_id == image_id {
                                media_cursor.push((index, cursor.row, cursor.column));
                            } else {
                                tracing::warn!(
                                    "terminal media: pending cursor image {} did not match final image {}",
                                    cursor.image_id,
                                    image_id
                                );
                                *pending_media_cursor = Some(cursor);
                            }
                        } else {
                            tracing::warn!(
                                "terminal media: missing pending cursor for image {image_id}"
                            );
                        }
                    } else {
                        let cursor = term.grid().cursor.point;
                        media_cursor.push((
                            index,
                            cursor.line.0,
                            u32::try_from(cursor.column.0).unwrap_or(u32::MAX),
                        ));
                    }
                }
            }
        }
        addressing.advance_to(term, processor, bytes, bytes.len());
        addressing.finish(term);
        (addressing, media_cursor)
    }

    /// Publish the batch's absolute row base, retiring the numbering when the
    /// batch moved rows by an amount the emulator does not report.
    fn commit_row_addressing(
        &self,
        addressing: &RowAddressing,
        history_size_before: usize,
        history_size_after: usize,
        alt_screen_changed: bool,
        addressing_break: bool,
    ) {
        if alt_screen_changed
            || addressing_break
            || addressing.saturated
            || history_size_after < history_size_before
        {
            self.retire_row_addressing(addressing.viewport_top, history_size_after);
        } else {
            self.viewport_top_absolute
                .store(addressing.viewport_top, Ordering::Release);
        }
    }

    /// Retire every recorded row id. Re-anchoring the base past the current
    /// scrollback keeps ids monotonic and keeps `base - history_size` pointing
    /// at the oldest addressable row for markers recorded afterwards. Recorded
    /// markers are kept — their kind and exit status stay meaningful — but the
    /// epoch mismatch makes them resolve to `Unavailable`.
    fn retire_row_addressing(&self, viewport_top: u64, history_size: usize) {
        self.viewport_top_absolute.store(
            viewport_top.saturating_add(history_size as u64),
            Ordering::Release,
        );
        self.row_addressing_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn record_shell_marker(
        &self,
        term: &Term<PaneEventListener>,
        kind: ShellMarkerKind,
        exit_code: Option<i32>,
        viewport_top: u64,
        epoch: u64,
    ) {
        let cursor = term.grid().cursor.point;
        let line = u64::try_from(cursor.line.0).unwrap_or(0);
        let marker = ShellMarker {
            sequence: self
                .marker_sequence
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1),
            kind,
            absolute_row: viewport_top.saturating_add(line),
            column: u32::try_from(cursor.column.0).unwrap_or(u32::MAX),
            exit_code,
            epoch,
        };
        let mut markers = self.shell_markers.lock();
        while markers.len() >= MAX_RECORDED_SHELL_MARKERS {
            markers.pop_front();
        }
        markers.push_back(marker);
        drop(markers);
        self.prompt_marker.fetch_add(1, Ordering::SeqCst);
    }

    /// Publish one coherent grid generation after its structured state is in
    /// the diff ring. The state flag forces a full snapshot for clients whose
    /// checkpoint precedes a cursor/mode/offset change.
    fn emit_generation(
        self: &Arc<Self>,
        dirty_rows: Vec<usize>,
        requires_full_snapshot: bool,
        force_broadcast: bool,
        coalescer: &mut AdaptiveCoalescer,
        state: &mut ReadLoopState,
    ) -> bool {
        let (diff, viewport_is_scrolled) = {
            let term = self.term.lock();
            (
                diff_from_dirty(&*term, &dirty_rows),
                term.grid().display_offset() != 0,
            )
        };
        self.publish_generation(diff, requires_full_snapshot || viewport_is_scrolled);

        // §16.3 The generation is already durable in the ring; only the
        // PaneDirty wakeup is subject to the tier window.
        let admitted = coalescer.admit_frame(Instant::now(), force_broadcast);
        state.pending_notify = !admitted;
        admitted
    }

    /// §3.3 补发被 coalescing 推迟、且窗口已到期的 PaneDirty。
    fn flush_pending_notify(&self, state: &mut ReadLoopState, coalescer: &mut AdaptiveCoalescer) {
        if !state.pending_notify {
            return;
        }
        if coalescer.admit_deferred_frame(Instant::now()) {
            self.broadcast_pane_dirty();
            state.pending_notify = false;
        }
    }

    /// §3.3 Unpaired-BSU wall-clock timeout: publish any deferred sync window
    /// generation bump without waiting for further PTY bytes.
    fn force_flush_after_bsu_timeout(
        self: &Arc<Self>,
        coalescer: &mut AdaptiveCoalescer,
        state: &mut ReadLoopState,
    ) {
        if state.pending_sync {
            let commit = self.commit.lock();
            let mut dirty_rows = std::mem::take(&mut state.pending_dirty_rows);
            dirty_rows.sort_unstable();
            dirty_rows.dedup();
            let requires_full_snapshot = std::mem::take(&mut state.pending_full_snapshot);
            let should_broadcast =
                self.emit_generation(dirty_rows, requires_full_snapshot, true, coalescer, state);
            state.pending_sync = false;
            drop(commit);
            if should_broadcast {
                self.broadcast_pane_dirty();
            }
        }
        self.flush_pending_notify(state, coalescer);
    }

    fn broadcast_pane_output(&self, bytes: &[u8], output_sequence: u64) {
        self.broadcast_notification(MuxNotification {
            event: Some(mux_protocol::notification::Event::PaneOutput(
                mux_protocol::PaneOutputChunk {
                    pane_id: self.id.clone(),
                    data: bytes.to_vec(),
                    output_sequence,
                },
            )),
        });
    }

    fn broadcast_pane_dirty(&self) {
        self.broadcast_notification(MuxNotification {
            event: Some(mux_protocol::notification::Event::PaneDirty(
                mux_protocol::PaneDirty {
                    pane_id: self.id.clone(),
                },
            )),
        });
    }

    fn broadcast_pane_bell(&self) {
        self.broadcast_notification(MuxNotification {
            event: Some(mux_protocol::notification::Event::PaneBell(
                mux_protocol::PaneBell {
                    pane_id: self.id.clone(),
                },
            )),
        });
    }
    /// §16.13 Queue completed media/actions in cross-type arrival order, each
    /// stamped with the one per-pane monotonic sequence shared by both types.
    /// The queue drains only through matching hooks; a session-scoped pane
    /// retains every event across late registration.
    fn dispatch_media_events(&self, state: &ReadLoopState) {
        let mut events = Vec::with_capacity(state.scan_events.len());
        for event in &state.scan_events {
            match event {
                ScanEvent::Media { index, .. } => {
                    let Some(media) = state.media.get(*index) else {
                        continue;
                    };
                    let mut media = media.clone();
                    media.pane_id = self.id.clone();
                    media.sequence = self
                        .media_sequence
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    events.push(PendingTypedEvent::Media(media));
                }
                ScanEvent::Action { index, .. } => {
                    let Some(action) = state.actions.get(*index) else {
                        continue;
                    };
                    let mut action = action.clone();
                    action.pane_id = self.id.clone();
                    action.sequence = self
                        .media_sequence
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    events.push(PendingTypedEvent::Action(action));
                }
                ScanEvent::Placement { .. } => {}
            }
        }
        self.enqueue_typed_events(events);
    }

    fn enqueue_typed_events(&self, events: Vec<PendingTypedEvent>) {
        let has_session = self.session_id.lock().is_some();
        let media_hook_present = self.media_hook.lock().is_some();
        let action_hook_present = self.action_hook.lock().is_some();
        let mut pending = self.pending_typed_events.lock();
        for event in events {
            let retain = has_session
                || match &event {
                    PendingTypedEvent::Media(_) => media_hook_present,
                    PendingTypedEvent::Action(_) => action_hook_present,
                };
            if !retain {
                continue;
            }
            pending.queue.push_back(event);
        }
        drop(pending);
        self.drain_pending_typed_events();
    }

    /// Drain without holding either the queue or hook lock while invoking a
    /// callback. `draining` also makes re-entrant hook registration safe: the
    /// active drainer observes newly queued events after the callback returns.
    /// Drain without holding either the queue or hook lock while invoking a
    /// callback. `draining` serializes concurrent drains, while
    /// `retry_requested` hands off a hook registration that races a missing
    /// hook at the front of the queue.
    fn drain_pending_typed_events(&self) {
        {
            let mut pending = self.pending_typed_events.lock();
            if pending.draining {
                pending.retry_requested = true;
                return;
            }
            pending.draining = true;
        }
        loop {
            let event = {
                let mut pending = self.pending_typed_events.lock();
                let Some(event) = pending.queue.pop_front() else {
                    pending.draining = false;
                    pending.retry_requested = false;
                    return;
                };
                event
            };
            match event {
                PendingTypedEvent::Media(media) => {
                    let hook = self.media_hook.lock().clone();
                    if let Some(hook) = hook {
                        hook(vec![media]);
                    } else {
                        let retry = {
                            let mut pending = self.pending_typed_events.lock();
                            pending.queue.push_front(PendingTypedEvent::Media(media));
                            let retry = pending.retry_requested;
                            pending.retry_requested = false;
                            pending.draining = false;
                            retry
                        };
                        if retry {
                            self.drain_pending_typed_events();
                        }
                        return;
                    }
                }
                PendingTypedEvent::Action(action) => {
                    let hook = self.action_hook.lock().clone();
                    if let Some(hook) = hook {
                        hook(vec![action]);
                    } else {
                        let retry = {
                            let mut pending = self.pending_typed_events.lock();
                            pending.queue.push_front(PendingTypedEvent::Action(action));
                            let retry = pending.retry_requested;
                            pending.retry_requested = false;
                            pending.draining = false;
                            retry
                        };
                        if retry {
                            self.drain_pending_typed_events();
                        }
                        return;
                    }
                }
            }
        }
    }

    /// §3.3 从 alacritty Term 收集 dirty 行号和整屏损伤标志。
    fn collect_dirty_rows(&self) -> (Vec<usize>, bool) {
        let mut term = self.term.lock();
        let mut rows = Vec::new();
        let fully_damaged = match term.damage() {
            TermDamage::Full => {
                // 整屏 dirty — 所有行
                let n = term.screen_lines();
                rows.extend(0..n);
                true
            }
            TermDamage::Partial(iter) => {
                for line in iter {
                    rows.push(line.line);
                }
                false
            }
        };
        term.reset_damage();
        (rows, fully_damaged)
    }
    fn broadcast_pane_title(&self, title: String) {
        self.broadcast_notification(MuxNotification {
            event: Some(mux_protocol::notification::Event::PaneTitleChanged(
                mux_protocol::PaneTitleChanged {
                    pane_id: self.id.clone(),
                    title,
                },
            )),
        });
    }

    fn broadcast_notification(&self, notification: MuxNotification) {
        if let Some(hook) = self.notification_hook.lock().clone() {
            hook(notification.clone());
        }
        self.subscribers
            .write()
            .retain(|_client_id, subscriber| subscriber.send(notification.clone()).is_ok());
    }
    pub fn add_subscriber(
        &self,
        client_id: String,
        sender: mpsc::UnboundedSender<MuxNotification>,
    ) {
        self.subscribers.write().insert(client_id, sender);
    }

    pub fn remove_subscriber(&self, client_id: &str) {
        self.subscribers.write().remove(client_id);
    }

    /// Install or replace the daemon-side observer used by server extensions.
    pub fn set_notification_hook(&self, hook: Arc<dyn Fn(MuxNotification) + Send + Sync>) {
        *self.notification_hook.lock() = Some(hook);
    }

    /// Drain Alacritty side effects. Grid-affecting state is compared around
    /// `Processor::advance`; titles and bells travel through dedicated events.
    fn handle_pending_events(&self) {
        let events: Vec<AlacEvent> = self.events.lock().drain(..).collect();
        for event in events {
            match event {
                AlacEvent::Title(title) => {
                    let commit = self.commit.lock();
                    self.set_title_locked(title.clone());
                    drop(commit);
                    self.broadcast_pane_title(title);
                    self.broadcast_pane_dirty();
                }
                AlacEvent::ResetTitle => {
                    let commit = self.commit.lock();
                    self.set_title_locked(String::new());
                    drop(commit);
                    self.broadcast_pane_title(String::new());
                    self.broadcast_pane_dirty();
                }
                AlacEvent::Bell => self.broadcast_pane_bell(),
                AlacEvent::PtyWrite(text) => {
                    if let Err(error) = self.pty_writer.lock().write_all(text.as_bytes()) {
                        tracing::warn!(error = %error, "pty_writer write_all failed");
                    }
                }
                AlacEvent::ClipboardStore(_clipboard_type, data) => {
                    if let Some(hook) = self.clipboard_hook.lock().as_ref() {
                        hook(data);
                    }
                }
                AlacEvent::ClipboardLoad(_, _) => {}
                AlacEvent::Exit | AlacEvent::ChildExit(_) => {
                    self.set_alive(false);
                    self.fire_exit_hook();
                }
                _ => {}
            }
        }
    }

    /// Insert the ring entry before exposing its generation. Callers hold the
    /// commit lock, which serializes PTY, resize, and metadata publishers.
    fn publish_generation(&self, diff: GridDiff, requires_full_snapshot: bool) -> u64 {
        let mut ring = self.grid_diff_ring.write();
        let generation = self.generation.load(Ordering::Relaxed).saturating_add(1);
        if requires_full_snapshot {
            ring.push_requiring_full_snapshot(generation, diff);
        } else {
            ring.push(generation, diff);
        }
        self.generation.store(generation, Ordering::Release);
        generation
    }

    pub fn get_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Advance while holding `commit`; saturate instead of wrapping so a
    /// multi-year daemon can never make an old sequence appear new again.
    fn advance_output_sequence(&self) -> u64 {
        self.output_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(1))
            })
            .map(|previous| previous.saturating_add(1))
            .unwrap_or(u64::MAX)
    }

    /// §16.11 Apply hot-reloaded scrollback capacity to the authoritative grid.
    pub fn set_scrollback_capacity(&self, capacity: usize) {
        let capacity = capacity.min(100_000);
        let commit = self.commit.lock();
        if self.scrollback_capacity.load(Ordering::Acquire) == capacity as u64 {
            return;
        }

        let diff = {
            let mut term = self.term.lock();
            term.set_options(TermConfig {
                scrolling_history: capacity,
                ..TermConfig::default()
            });
            // Shrinking the limit evicts the oldest rows without reporting how
            // many, so recorded row ids cannot survive it.
            self.retire_row_addressing(
                self.viewport_top_absolute.load(Ordering::Acquire),
                term.grid().history_size(),
            );
            let all_rows = (0..term.screen_lines()).collect::<Vec<_>>();
            diff_from_dirty(&*term, &all_rows)
        };
        self.scrollback_capacity
            .store(capacity as u64, Ordering::Release);
        self.history_version.fetch_add(1, Ordering::AcqRel);
        self.publish_generation(diff, true);
        drop(commit);
        self.broadcast_pane_dirty();
    }

    /// Publish a metadata-triggered generation with a full-screen row diff.
    pub fn bump_generation(&self) {
        let commit = self.commit.lock();
        self.bump_generation_locked();
        drop(commit);
        self.broadcast_pane_dirty();
    }

    fn bump_generation_locked(&self) {
        let diff = {
            let term = self.term.lock();
            let all_rows = (0..term.screen_lines()).collect::<Vec<_>>();
            diff_from_dirty(&*term, &all_rows)
        };
        self.publish_generation(diff, false);
    }

    /// §3.10 SendInput — 向 PTY 写入原始字节。
    pub fn write_input(&self, data: &[u8]) -> anyhow::Result<()> {
        // §16.3 This is the only path user input reaches the PTY, so it is
        // where "keyboard active" is established for the coalescer. A large
        // paste also lands here, but its echo exceeds the Interactive tier's
        // 4KB/s ceiling, so it cannot hold the pane at a 0ms window.
        self.keyboard_activity.note_input();
        let mut writer = self.pty_writer.lock();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// §3.10 Paste — 向 PTY 写入文本 (可选 bracketed paste markers)。
    pub fn paste(&self, text: &str) -> anyhow::Result<()> {
        if self.is_bracketed_paste_active() {
            let bracketed = format!("\x1b[200~{}\x1b[201~", text);
            self.write_input(bracketed.as_bytes())
        } else {
            self.write_input(text.as_bytes())
        }
    }

    /// Fetch one generation checkpoint while excluding every publisher. The
    /// returned output sequence is an atomic fence: the grid state incorporates
    /// every PaneOutput batch through that sequence.
    pub fn fetch_grid_update(&self, since_generation: u64) -> (grid_sync::GridUpdate, u64) {
        let _commit = self.commit.lock();
        let ring = self.grid_diff_ring.read();
        let current = self.generation.load(Ordering::Acquire);
        let output_sequence = self.output_sequence.load(Ordering::Acquire);
        let update = ring.fetch_update(since_generation, current, || {
            let term = self.term.lock();
            let mut snapshot = snapshot_from_term(&*term);
            snapshot.history_version = self.history_version.load(Ordering::Acquire);
            snapshot
        });
        (update, output_sequence)
    }

    /// §3.3 get_full_snapshot — 当前 grid 完整快照。
    pub fn get_full_snapshot(&self) -> FullGridSnapshot {
        let _commit = self.commit.lock();
        let term = self.term.lock();
        let mut snapshot = snapshot_from_term(&*term);
        snapshot.history_version = self.history_version.load(Ordering::Acquire);
        snapshot
    }

    /// Reject sizes the protocol cannot carry before any state is recorded, so
    /// a malformed client viewport can never become part of the min-fit.
    fn checked_grid_dimensions(cols: u32, rows: u32) -> anyhow::Result<(usize, usize)> {
        let cols_usize = usize::try_from(cols).context("pane column count exceeds host limit")?;
        let rows_usize = usize::try_from(rows).context("pane row count exceeds host limit")?;
        mux_protocol::checked_grid_cell_count(cols_usize, rows_usize)
            .map_err(|message| anyhow::anyhow!("invalid pane size {cols}x{rows}: {message}"))?;
        Ok((cols_usize, rows_usize))
    }

    /// §16.2 Record `client_id`'s viewport for this pane and re-apply the
    /// min-fit size.
    ///
    /// Multi-client sessions share one authoritative grid, so the pane shrinks
    /// to the smallest attached viewport instead of letting whichever client
    /// resized last overwrite everyone else's size.
    pub fn set_client_viewport(
        &self,
        client_id: String,
        cols: u32,
        rows: u32,
    ) -> anyhow::Result<()> {
        Self::checked_grid_dimensions(cols, rows)?;
        let mut viewports = self.client_viewports.lock();
        viewports.insert(client_id, PaneViewport { cols, rows });
        self.apply_min_fit(&viewports)
    }

    /// §16.2 Drop a detached, kicked, or disconnected client's constraint and
    /// re-apply min-fit. Removing the smallest client lets the pane grow back.
    pub fn remove_client_viewport(&self, client_id: &str) -> anyhow::Result<()> {
        let mut viewports = self.client_viewports.lock();
        if viewports.remove(client_id).is_none() {
            return Ok(());
        }
        self.apply_min_fit(&viewports)
    }

    /// §16.2 Number of attached clients currently constraining this pane.
    pub fn client_viewport_count(&self) -> usize {
        self.client_viewports.lock().len()
    }

    /// §16.2 Current min-fit across attached clients, or `None` when no client
    /// has reported a viewport.
    pub fn min_fit_viewport(&self) -> Option<PaneViewport> {
        min_fit(&self.client_viewports.lock())
    }

    /// Apply the min-fit size while still holding the viewport map, so
    /// concurrent client reports cannot interleave into a size that disagrees
    /// with the recorded constraints. The last remaining client detaching
    /// leaves the map empty; the pane then keeps its current size rather than
    /// collapsing to a default.
    fn apply_min_fit(&self, viewports: &HashMap<String, PaneViewport>) -> anyhow::Result<()> {
        let Some(fit) = min_fit(viewports) else {
            return Ok(());
        };
        if self.get_cols() == fit.cols && self.get_rows() == fit.rows {
            return Ok(());
        }
        self.resize(fit.cols, fit.rows)
    }

    /// §3.10 Resize — 改 PTY winsize + resize alacritty Term + bump generation。
    ///
    /// §16.2 callers that represent one client should go through
    /// `set_client_viewport` so the min-fit constraint is honored; this entry
    /// point applies a size unconditionally.
    pub fn resize(&self, cols: u32, rows: u32) -> anyhow::Result<()> {
        let (cols_usize, rows_usize) = Self::checked_grid_dimensions(cols, rows)?;
        let commit = self.commit.lock();
        self.pty_master.lock().resize(PtySize {
            rows: rows
                .try_into()
                .context("pane row count exceeds PTY limit")?,
            cols: cols
                .try_into()
                .context("pane column count exceeds PTY limit")?,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let diff = {
            let mut term = self.term.lock();
            term.resize(TermSize::new(cols_usize, rows_usize));
            // Reflow moves recorded rows by an amount Alacritty does not
            // report; §15 accepts losing marker positions across a resize.
            self.retire_row_addressing(
                self.viewport_top_absolute.load(Ordering::Acquire),
                term.grid().history_size(),
            );
            let all_rows = (0..term.screen_lines()).collect::<Vec<_>>();
            diff_from_dirty(&*term, &all_rows)
        };
        self.cols.store(cols as u64, Ordering::SeqCst);
        self.rows.store(rows as u64, Ordering::SeqCst);
        self.history_version.fetch_add(1, Ordering::AcqRel);
        self.publish_generation(diff, true);
        drop(commit);

        self.broadcast_pane_dirty();
        Ok(())
    }

    /// §3.3 获取当前 cols。
    pub fn get_cols(&self) -> u32 {
        self.cols.load(Ordering::SeqCst) as u32
    }

    /// §3.3 获取当前 rows。
    pub fn get_rows(&self) -> u32 {
        self.rows.load(Ordering::SeqCst) as u32
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::SeqCst);
    }

    /// §3.4 关联该 pane 到所在 session。会话级 lifecycle 通知需要知道
    /// 目标 session 才能 fan-out; spawn_with_session 已设置, 此处覆盖用于
    /// "Pane::spawn 后由 connection 层延迟注入" 的回退路径。
    pub fn set_session_id(&self, session_id: String) {
        *self.session_id.lock() = Some(session_id);
    }

    /// §16.6 Install a hook invoked when the emulator stores clipboard content
    /// (OSC 52 / ClipboardStore). Replaces any previous hook.
    pub fn set_clipboard_hook(&self, hook: Box<dyn Fn(String) + Send>) {
        *self.clipboard_hook.lock() = Some(hook);
    }
    /// §16.13 Install an observer invoked for each completed `PaneMedia` in
    /// sequence order. Replaces any previous hook.
    pub fn set_media_hook(&self, hook: Box<dyn Fn(Vec<PaneMedia>) + Send + Sync>) {
        *self.media_hook.lock() = Some(Arc::from(hook));
        self.drain_pending_typed_events();
    }

    /// §16.13 Install an observer invoked for each `PaneAction` in sequence
    /// order. Replaces any previous hook.
    pub fn set_action_hook(&self, hook: Box<dyn Fn(Vec<PaneAction>) + Send + Sync>) {
        *self.action_hook.lock() = Some(Arc::from(hook));
        self.drain_pending_typed_events();
    }

    /// §3.4 获取 pane 所属 session id (可能为 None 表示未关联会话)。
    pub fn get_session_id(&self) -> Option<String> {
        self.session_id.lock().clone()
    }

    /// §3.4 注册 PTY 自然退出钩子。由 connection 层在把 pane 加入 session
    /// 之后调用; 闭包在 PTY EOF 或 alacritty Exit/ChildExit 时被触发,
    /// 负责 session 级清理 (从 layout / panes 移除) 以及 PaneRemoved fan-out。
    ///
    /// Hook installation races the read loop: a command can exit before the
    /// connection layer has published the pane and installed its cleanup.
    /// Re-check `alive` after storing the hook so that late installation
    /// deterministically replays the one-shot cleanup.
    pub fn set_exit_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.exit_hook.lock() = Some(hook);
        if !self.is_alive() {
            self.fire_exit_hook();
        }
    }

    /// §3.4 触发并清空 PTY 退出钩子 (一次性)。
    ///
    /// 由 PTY read-loop Ok(0) / Err 路径与 alacritty Exit / ChildExit 路径共享;
    /// take 保证只执行一次, 防止两份清理代码同时跑导致重复 PaneRemoved 广播。
    pub fn fire_exit_hook(&self) {
        let hook = self.exit_hook.lock().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn set_title(&self, title: String) {
        let commit = self.commit.lock();
        self.set_title_locked(title);
        drop(commit);
        self.broadcast_pane_dirty();
    }

    fn set_title_locked(&self, title: String) {
        *self.title.write() = title;
        self.bump_generation_locked();
    }

    pub fn get_title(&self) -> String {
        self.title.read().clone()
    }

    pub(crate) fn metadata_snapshot(&self) -> PaneMetadataSnapshot {
        let _commit = self.commit.lock();
        PaneMetadataSnapshot {
            title: self.title.read().clone(),
            generation: self.generation.load(Ordering::Acquire),
            cols: self.cols.load(Ordering::SeqCst) as u32,
            rows: self.rows.load(Ordering::SeqCst) as u32,
            is_alive: self.alive.load(Ordering::SeqCst),
            zoomed: self.zoomed.load(Ordering::SeqCst),
        }
    }

    pub fn is_bracketed_paste_active(&self) -> bool {
        self.bracketed_paste_mode.load(Ordering::SeqCst)
    }

    pub fn set_bracketed_paste_mode(&self, active: bool) {
        self.bracketed_paste_mode.store(active, Ordering::SeqCst);
    }

    /// §3.3 同步 bracketed paste 状态 (从 alacritty term mode 读取)。
    pub fn sync_bracketed_paste_mode(&self) {
        let term = self.term.lock();
        let active = term.mode().contains(TermMode::BRACKETED_PASTE);
        drop(term);
        self.set_bracketed_paste_mode(active);
    }

    /// §16.9 扩展导出路径 (无 typed error 通道): 越界参数按空结果 fail-closed,
    /// 避免在扩展可控的 count 下做无界分配。RPC 路径请用 `fetch_scrollback_checked`。
    pub fn fetch_scrollback(
        &self,
        from_line: u32,
        direction: u32,
        count: u32,
    ) -> (Vec<grid_sync::RowChange>, u32, u64) {
        match self.fetch_scrollback_checked(from_line, direction, count) {
            Ok(result) => result,
            Err(_) => (Vec::new(), 0, self.history_version.load(Ordering::Acquire)),
        }
    }

    /// §16.9 RPC 路径: 在分配/序列化前严格校验 direction 与 count, 失败返回
    /// typed 错误, 由连接层映射为 RPC error (socket 保持可用)。
    pub fn fetch_scrollback_checked(
        &self,
        from_line: u32,
        direction: u32,
        count: u32,
    ) -> Result<(Vec<grid_sync::RowChange>, u32, u64), grid_sync::ScrollbackError> {
        let _commit = self.commit.lock();
        let term = self.term.lock();
        let (lines, total) =
            grid_sync::fetch_scrollback_from_term(&*term, from_line, direction, count)?;
        let version = self.history_version.load(Ordering::Acquire);
        Ok((lines, total, version))
    }

    pub fn search_scrollback(
        &self,
        regex: &str,
        from_line: u32,
        direction: u32,
        max_results: u32,
    ) -> (Vec<(u32, grid_sync::RowChange)>, u64) {
        let _commit = self.commit.lock();
        let term = self.term.lock();
        let matches = grid_sync::search_scrollback_from_term(
            &*term,
            regex,
            from_line,
            direction,
            max_results,
        );
        let version = self.history_version.load(Ordering::Acquire);
        (matches, version)
    }

    pub fn get_scrollback_version(&self) -> u64 {
        self.history_version.load(Ordering::Acquire)
    }

    /// §3.3 Atomically set pane zoom state and publish its generation.
    ///
    /// Returns whether the state actually changed. Zoom moves the layout, so a
    /// zoom that changes nothing must not bump the generation or wake clients
    /// into reprojecting a layout that is already what they render.
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        let commit = self.commit.lock();
        if self.zoomed.swap(zoomed, Ordering::SeqCst) == zoomed {
            return false;
        }
        self.bump_generation_locked();
        drop(commit);
        self.broadcast_pane_dirty();
        true
    }

    /// §3.3 获取 pane zoom 状态。
    pub fn is_zoomed(&self) -> bool {
        self.zoomed.load(Ordering::SeqCst)
    }

    /// §3.3 获取当前 cwd (可能已被 OSC 7 更新)。
    pub fn get_cwd(&self) -> String {
        self.cwd.read().clone()
    }

    /// §3.3 获取 prompt marker 计数。
    pub fn get_prompt_marker(&self) -> u32 {
        self.prompt_marker.load(Ordering::SeqCst) as u32
    }

    /// §3.3 Recorded OSC 133 markers, oldest first.
    pub fn shell_markers(&self) -> Vec<ShellMarker> {
        self.shell_markers.lock().iter().copied().collect()
    }

    /// §3.3 Current row-numbering epoch. A marker recorded under a different
    /// epoch can no longer be resolved to a row.
    pub fn row_addressing_epoch(&self) -> u64 {
        self.row_addressing_epoch.load(Ordering::Acquire)
    }

    /// §3.3 Resolve one recorded marker to a row addressable right now.
    pub fn locate_shell_marker(&self, marker: &ShellMarker) -> ShellMarkerPosition {
        let _commit = self.commit.lock();
        let (history_size, screen_lines) = self.row_addressing_extent();
        self.resolve_shell_marker(marker, history_size, screen_lines)
    }

    /// §3.3 Every recorded marker paired with where it resolves right now.
    pub fn shell_marker_positions(&self) -> Vec<(ShellMarker, ShellMarkerPosition)> {
        let _commit = self.commit.lock();
        let (history_size, screen_lines) = self.row_addressing_extent();
        self.shell_markers
            .lock()
            .iter()
            .map(|marker| {
                (
                    *marker,
                    self.resolve_shell_marker(marker, history_size, screen_lines),
                )
            })
            .collect()
    }

    fn row_addressing_extent(&self) -> (usize, usize) {
        let term = self.term.lock();
        (term.grid().history_size(), term.screen_lines())
    }

    fn resolve_shell_marker(
        &self,
        marker: &ShellMarker,
        history_size: usize,
        screen_lines: usize,
    ) -> ShellMarkerPosition {
        if marker.epoch != self.row_addressing_epoch.load(Ordering::Acquire) {
            return ShellMarkerPosition::Unavailable;
        }
        let viewport_top = self.viewport_top_absolute.load(Ordering::Acquire);
        let oldest_row = viewport_top.saturating_sub(history_size as u64);
        if marker.absolute_row < oldest_row {
            return ShellMarkerPosition::Unavailable;
        }
        if marker.absolute_row < viewport_top {
            return match u32::try_from(marker.absolute_row - oldest_row) {
                Ok(index) => ShellMarkerPosition::History { index },
                Err(_) => ShellMarkerPosition::Unavailable,
            };
        }
        let line = marker.absolute_row - viewport_top;
        if line >= screen_lines as u64 {
            return ShellMarkerPosition::Unavailable;
        }
        match u32::try_from(line) {
            Ok(line) => ShellMarkerPosition::Viewport { line },
            Err(_) => ShellMarkerPosition::Unavailable,
        }
    }

    /// §3.3 处理 OSC 7 URI: 提取 file:// 路径, 更新 pane cwd。
    fn handle_osc7_cwd(&self, uri: &str) {
        // file://hostname/path → /path
        let path = if let Some(rest) = uri.strip_prefix("file://") {
            // 跳过 hostname (到第一个 '/')
            match rest.find('/') {
                Some(slash) => &rest[slash..],
                None => rest,
            }
        } else {
            uri
        };

        if path.is_empty() {
            return;
        }

        // 百分号解码 (e.g. %20 → space)
        let decoded = percent_decode(path);
        let old = self.cwd.read().clone();
        if decoded != old {
            *self.cwd.write() = decoded;
            // 广播 ShellIntegrationChanged 到所有订阅者
            self.broadcast_shell_integration_changed();
        }
    }

    fn broadcast_shell_integration_changed(&self) {
        self.broadcast_notification(MuxNotification {
            event: Some(mux_protocol::notification::Event::ShellIntegrationChanged(
                mux_protocol::ShellIntegrationChanged {
                    cwd: self.get_cwd(),
                },
            )),
        });
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // §3.5 pane drop: 标记 dead + 尝试 kill child (避免僵尸进程)
        self.alive.store(false, Ordering::SeqCst);
        if let Some(child) = self.child.lock().take() {
            let mut killer = child.clone_killer();
            if let Err(error) = killer.kill() {
                tracing::warn!(%error, "failed to kill child process during pane drop");
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn emulator_title_events_reach_pane_subscribers() {
        let pane = match Pane::spawn(
            "title-test-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn title test pane: {error}"),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        pane.add_subscriber("title-client".to_string(), tx);

        pane.events
            .lock()
            .push(AlacEvent::Title("server title".to_string()));
        pane.handle_pending_events();
        let notification = match rx.try_recv() {
            Ok(notification) => notification,
            Err(error) => panic!("receive title notification: {error}"),
        };
        match notification.event {
            Some(mux_protocol::notification::Event::PaneTitleChanged(changed)) => {
                assert_eq!(changed.pane_id, pane.id);
                assert_eq!(changed.title, "server title");
            }
            event => panic!("expected PaneTitleChanged, got {event:?}"),
        }
        match rx.try_recv() {
            Ok(MuxNotification {
                event: Some(mux_protocol::notification::Event::PaneDirty(dirty)),
            }) => assert_eq!(dirty.pane_id, pane.id),
            Ok(notification) => panic!("expected PaneDirty, got {:?}", notification.event),
            Err(error) => panic!("receive title PaneDirty notification: {error}"),
        }

        pane.events.lock().push(AlacEvent::ResetTitle);
        pane.handle_pending_events();
        let notification = match rx.try_recv() {
            Ok(notification) => notification,
            Err(error) => panic!("receive reset-title notification: {error}"),
        };
        match notification.event {
            Some(mux_protocol::notification::Event::PaneTitleChanged(changed)) => {
                assert_eq!(changed.pane_id, pane.id);
                assert!(changed.title.is_empty());
            }
            event => panic!("expected reset PaneTitleChanged, got {event:?}"),
        }
        match rx.try_recv() {
            Ok(MuxNotification {
                event: Some(mux_protocol::notification::Event::PaneDirty(dirty)),
            }) => assert_eq!(dirty.pane_id, pane.id),
            Ok(notification) => panic!("expected PaneDirty, got {:?}", notification.event),
            Err(error) => panic!("receive reset-title PaneDirty notification: {error}"),
        }
    }

    #[test]
    fn daemon_notification_hook_receives_emulator_events() {
        let pane = match Pane::spawn(
            "notification-hook-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn notification hook pane: {error}"),
        };
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let captured = notifications.clone();
        pane.set_notification_hook(Arc::new(move |notification| {
            captured.lock().push(notification);
        }));

        pane.events
            .lock()
            .push(AlacEvent::Title("extension title".to_string()));
        pane.handle_pending_events();

        let notifications = notifications.lock();
        assert!(matches!(
            notifications.first().and_then(|notification| notification.event.as_ref()),
            Some(mux_protocol::notification::Event::PaneTitleChanged(changed))
                if changed.title == "extension title"
        ));
        assert!(matches!(
            notifications
                .get(1)
                .and_then(|notification| notification.event.as_ref()),
            Some(mux_protocol::notification::Event::PaneDirty(_))
        ));
    }

    #[test]
    fn subscriber_registration_replaces_and_removes_by_client_id() {
        let pane = match Pane::spawn(
            "subscriber-lifecycle-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn subscriber lifecycle pane: {error}"),
        };
        let (old_sender, mut old_receiver) = mpsc::unbounded_channel();
        let (replacement_sender, mut replacement_receiver) = mpsc::unbounded_channel();

        pane.add_subscriber("client-1".to_string(), old_sender);
        pane.add_subscriber("client-1".to_string(), replacement_sender);
        assert!(matches!(
            old_receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));

        pane.events
            .lock()
            .push(AlacEvent::Title("replacement title".to_string()));
        pane.handle_pending_events();
        assert!(matches!(
            replacement_receiver.try_recv(),
            Ok(MuxNotification {
                event: Some(mux_protocol::notification::Event::PaneTitleChanged(_)),
            })
        ));
        assert!(matches!(
            replacement_receiver.try_recv(),
            Ok(MuxNotification {
                event: Some(mux_protocol::notification::Event::PaneDirty(_)),
            })
        ));

        pane.remove_subscriber("client-1");
        assert!(matches!(
            replacement_receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    fn spawn_media_pane(id: &str) -> Arc<Pane> {
        spawn_marker_pane(id, 20, 6, 100)
    }

    fn typed_notifications(pane: &Arc<Pane>) -> Arc<Mutex<Vec<MuxNotification>>> {
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let captured_media = notifications.clone();
        pane.set_media_hook(Box::new(move |media| {
            let mut notifications = captured_media.lock();
            notifications.extend(media.into_iter().map(|media| MuxNotification {
                event: Some(mux_protocol::notification::Event::PaneMedia(media)),
            }));
        }));
        let captured_actions = notifications.clone();
        pane.set_action_hook(Box::new(move |actions| {
            let mut notifications = captured_actions.lock();
            notifications.extend(actions.into_iter().map(|action| MuxNotification {
                event: Some(mux_protocol::notification::Event::PaneAction(action)),
            }));
        }));
        notifications
    }

    #[test]
    fn kitty_media_is_stripped_but_surrounding_text_reaches_grid() {
        let pane = spawn_media_pane("media-grid");
        let mut feed = PtyFeed::new();
        let notifications = typed_notifications(&pane);

        feed.feed(
            &pane,
            b"before\x1b_Ga=T,f=100,i=7,c=2,r=1;SGVsbG8=\x1b\\after",
        );

        let snapshot = pane.get_full_snapshot();
        let text: String = snapshot
            .cells
            .iter()
            .take("beforeafter".len())
            .map(|cell| cell.character.as_str())
            .collect();
        assert_eq!(text, "beforeafter");
        assert_eq!(notifications.lock().len(), 1);
    }

    #[test]
    fn kitty_display_captures_cursor_at_control_offset() {
        let pane = spawn_media_pane("media-cursor");
        let mut feed = PtyFeed::new();
        let notifications = typed_notifications(&pane);

        feed.feed(
            &pane,
            b"\x1b[3;5H\x1b_Ga=T,f=100,i=8,c=2,r=1;SGVsbG8=\x1b\\tail",
        );

        let typed = notifications.lock();
        assert_eq!(typed.len(), 1);
        match typed.first().and_then(|notification| notification.event.as_ref()) {
            Some(mux_protocol::notification::Event::PaneMedia(media)) => {
                assert_eq!((media.row, media.column), (2, 4));
            }
            event => panic!("expected PaneMedia, got {event:?}"),
        }
    }

    #[test]
    fn media_and_actions_share_ordered_pane_sequence_across_batches() {
        let pane = spawn_media_pane("media-action-order");
        let mut feed = PtyFeed::new();
        let notifications = typed_notifications(&pane);

        feed.feed(
            &pane,
            b"left\x1b_Ga=T,f=100,i=9;SGVsbG8=\x1b\\right",
        );
        feed.feed(&pane, b"\x1b]9;z3rm-download;https://example.test/a\x07after");

        let typed = notifications.lock();
        assert_eq!(typed.len(), 2);
        let events: Vec<(u64, &'static str)> = typed
            .iter()
            .filter_map(|notification| match notification.event.as_ref() {
                Some(mux_protocol::notification::Event::PaneMedia(media)) => {
                    Some((media.sequence, "media"))
                }
                Some(mux_protocol::notification::Event::PaneAction(action)) => {
                    Some((action.sequence, "action"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(events, vec![(1, "media"), (2, "action")]);
    }
    #[test]
    fn media_advances_generation_and_osc52_remains_one_clipboard_effect() {
        let pane = spawn_media_pane("media-generation-clipboard");
        let mut feed = PtyFeed::new();
        let clipboard = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = clipboard.clone();
        pane.set_clipboard_hook(Box::new(move |value| captured.lock().push(value)));
        let baseline = pane.get_generation();

        feed.feed(
            &pane,
            b"\x1b_Ga=T,f=100,i=10;SGVsbG8=\x1b\\\x1b]52;c;SGVsbG8=\x07",
        );

        assert!(pane.get_generation() > baseline);
        assert_eq!(&*clipboard.lock(), &["Hello".to_string()]);
    }

    #[test]
    fn pane_drop_releases_media_hook_without_arc_cycle() {
        let pane = Pane::spawn(
            "media-hook-drop".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            6,
            // A process that exits at once, spelled the way that works on every
            // platform: macOS has no /bin/false, so naming it directly made this
            // test unrunnable there.
            Some(ShellCommand {
                program: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "exit 0".to_string()],
                ..Default::default()
            }),
        )
        .expect("spawn media hook drop pane");
        let weak = Arc::downgrade(&pane);
        drop(pane);
        for _ in 0..100 {
            if weak.upgrade().is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("pane reader retained a strong reference after drop");
    }

    #[test]
    fn hook_registration_during_drain_releases_pending_action() {
        let pane = Pane::spawn_with_session(
            "media-hook-drain-race".to_string(),
            "session".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            6,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            100,
        )
        .expect("spawn media hook drain pane");
        let received = Arc::new(Mutex::new(Vec::<PaneAction>::new()));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut pending = pane.pending_typed_events.lock();
            pending
                .queue
                .push_back(PendingTypedEvent::Media(PaneMedia::default()));
            pending
                .queue
                .push_back(PendingTypedEvent::Action(PaneAction::default()));
        }

        let pane_for_drain = pane.clone();
        let started_for_hook = started.clone();
        let release_for_hook = release.clone();
        let drain_thread = std::thread::spawn(move || {
            pane_for_drain.set_media_hook(Box::new(move |_media| {
                started_for_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                while !release_for_hook.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::yield_now();
                }
            }));
        });
        for _ in 0..1000 {
            if started.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));

        let captured = received.clone();
        pane.set_action_hook(Box::new(move |actions| {
            captured.lock().extend(actions);
        }));
        release.store(true, std::sync::atomic::Ordering::SeqCst);
        drain_thread.join().expect("media hook drain thread");
        assert_eq!(received.lock().len(), 1);
    }
    #[test]
    fn media_event_waits_for_late_hook_registration() {
        let pane = match Pane::spawn_with_session(
            "media-late-hook".to_string(),
            "session".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            6,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            100,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn media late-hook pane: {error}"),
        };
        let mut feed = PtyFeed::new();
        feed.feed(&pane, b"\x1b_Ga=T,f=100,i=12;SGVsbG8=\x1b\\");

        let received = Arc::new(Mutex::new(Vec::<PaneMedia>::new()));
        let captured = received.clone();
        pane.set_media_hook(Box::new(move |media| captured.lock().extend(media)));

        assert_eq!(received.lock().len(), 1);
    }

    #[test]
    fn kitty_continuation_preserves_initial_display_cursor_across_feeds() {
        let pane = spawn_media_pane("media-cross-feed-cursor");
        let mut feed = PtyFeed::new();
        let notifications = typed_notifications(&pane);

        feed.feed(
            &pane,
            b"abcdefghijkl\x1b[5;7H\x1b_Ga=T,m=1,f=100,i=13;SGVsbG8=\x1b\\after",
        );
        feed.feed(&pane, b"\x1b_Gm=0,i=13;V29ybGQ=\x1b\\");

        let typed = notifications.lock();
        match typed.first().and_then(|notification| notification.event.as_ref()) {
            Some(mux_protocol::notification::Event::PaneMedia(media)) => {
                assert_eq!((media.row, media.column), (4, 6));
            }
            event => panic!("expected PaneMedia, got {event:?}"),
        }
    }


    #[test]
    fn mode_only_output_publishes_full_generation() {
        let pane = match Pane::spawn(
            "mode-test-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn mode test pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"baseline", &mut dec, &mut coalescer, &mut state);
        let (_, baseline_output_sequence) = pane.fetch_grid_update(0);
        assert_eq!(baseline_output_sequence, 1);
        assert_eq!(pane.get_generation(), 1);

        pane.process_pty_bytes(b"\x1b[?1h\x1b[?2004h", &mut dec, &mut coalescer, &mut state);

        assert_eq!(pane.get_generation(), 2);
        let (update, output_sequence) = pane.fetch_grid_update(1);
        assert_eq!(output_sequence, 2);
        match update {
            grid_sync::GridUpdate::FullSnapshot { snapshot, .. } => {
                assert_ne!(snapshot.modes & mux_protocol::terminal_mode::APP_CURSOR, 0);
                assert_ne!(
                    snapshot.modes & mux_protocol::terminal_mode::BRACKETED_PASTE,
                    0
                );
            }
            update => panic!("expected mode-only full snapshot, got {update:?}"),
        }
    }

    #[test]
    fn split_escape_sequence_is_parsed_across_pty_batches() {
        let pane = match Pane::spawn(
            "split-sequence-pane".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn split sequence pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"\x1b[?", &mut dec, &mut coalescer, &mut state);
        assert_eq!(
            pane.get_full_snapshot().modes & mux_protocol::terminal_mode::APP_CURSOR,
            0
        );
        pane.process_pty_bytes(b"1h", &mut dec, &mut coalescer, &mut state);

        assert_ne!(
            pane.get_full_snapshot().modes & mux_protocol::terminal_mode::APP_CURSOR,
            0
        );
    }

    #[test]
    fn mode_only_output_preserves_existing_history_version() {
        let pane = match Pane::spawn_with_session(
            "mode-history-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            10,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn mode history test pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"A\r\nB\r\n", &mut dec, &mut coalescer, &mut state);
        let history_version = pane.get_scrollback_version();
        let generation = pane.get_generation();

        pane.process_pty_bytes(b"\x1b[?1h", &mut dec, &mut coalescer, &mut state);

        assert_eq!(pane.get_scrollback_version(), history_version);
        match pane.fetch_grid_update(generation).0 {
            grid_sync::GridUpdate::FullSnapshot { snapshot, .. } => {
                assert_eq!(snapshot.history_version, history_version);
                assert_ne!(snapshot.modes & mux_protocol::terminal_mode::APP_CURSOR, 0);
            }
            update => panic!("expected mode-only full snapshot, got {update:?}"),
        }
    }

    #[test]
    fn visible_input_does_not_advance_history_version() {
        let pane = match Pane::spawn_with_session(
            "visible-input-history-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            10,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn visible input history test pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"A\r\nB\r\n", &mut dec, &mut coalescer, &mut state);
        let history_version = pane.get_scrollback_version();
        let generation = pane.get_generation();

        pane.process_pty_bytes(b"xy", &mut dec, &mut coalescer, &mut state);

        assert_eq!(pane.get_scrollback_version(), history_version);
        assert!(pane.get_generation() > generation);
    }

    #[test]
    fn full_history_rotation_advances_history_version() {
        let pane = match Pane::spawn_with_session(
            "history-version-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            2,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn history version test pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"A\r\nB\r\nC\r\n", &mut dec, &mut coalescer, &mut state);
        let (_, full_total, full_version) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(full_total, 2);

        pane.process_pty_bytes(b"D\r\n", &mut dec, &mut coalescer, &mut state);
        let (lines, rotated_total, rotated_version) = pane.fetch_scrollback(0, 1, 10);

        assert_eq!(rotated_total, full_total);
        assert_ne!(rotated_version, full_version);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].cells[0].character, "B");
        assert_eq!(lines[1].cells[0].character, "C");
    }
    #[test]
    fn repeated_content_rotation_advances_history_version() {
        let pane = match Pane::spawn_with_session(
            "repeated-history-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            1,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            2,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn repeated history pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"X\r\nX\r\nX\r\n", &mut dec, &mut coalescer, &mut state);
        let (before_rows, before_total, before_version) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(before_total, 2);
        assert!(before_rows.iter().all(|row| row.cells[0].character == "X"));

        pane.process_pty_bytes(b"X\r\n", &mut dec, &mut coalescer, &mut state);
        let (after_rows, after_total, after_version) = pane.fetch_scrollback(0, 1, 10);

        assert_eq!(after_total, before_total);
        assert!(after_rows.iter().all(|row| row.cells[0].character == "X"));
        assert_ne!(after_version, before_version);
    }

    #[test]
    fn invalid_scrollback_parameters_are_rejected() {
        let pane = match Pane::spawn_with_session(
            "invalid-scrollback-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            2,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn invalid scrollback pane: {error}"),
        };

        // Only 0 (up) and 1 (down) are valid directions.
        for direction in [2u32, 3, 7, u32::MAX] {
            assert!(
                matches!(
                    pane.fetch_scrollback_checked(0, direction, 10),
                    Err(grid_sync::ScrollbackError::InvalidDirection)
                ),
                "direction {direction} must be rejected"
            );
        }
        // cols=4, so any count above MAX_GRID_CELLS / 4 is oversized.
        let cap = (mux_protocol::MAX_GRID_CELLS / 4) as u32;
        assert!(matches!(
            pane.fetch_scrollback_checked(0, 1, cap + 1),
            Err(grid_sync::ScrollbackError::CountTooLarge)
        ));
        assert!(matches!(
            pane.fetch_scrollback_checked(0, 1, u32::MAX),
            Err(grid_sync::ScrollbackError::CountTooLarge)
        ));
        // Valid parameters pass, including the count=0 metadata probe.
        assert!(pane.fetch_scrollback_checked(0, 0, 10).is_ok());
        assert!(pane.fetch_scrollback_checked(0, 1, 10).is_ok());
        assert!(pane.fetch_scrollback_checked(0, 1, 0).is_ok());
        assert!(pane.fetch_scrollback_checked(0, 1, cap).is_ok());
    }

    #[test]
    fn clear_screen_all_rotates_full_history_and_advances_version() {
        let pane = match Pane::spawn_with_session(
            "clear-screen-all-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            2,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn clear screen all pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        // Fill the 2-row history ring to full capacity: history = [A, B].
        pane.process_pty_bytes(b"A\r\nB\r\nC\r\n", &mut dec, &mut coalescer, &mut state);
        let (before_rows, before_total, before_version) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(before_total, 2);
        assert_eq!(before_rows[0].cells[0].character, "A");

        // CSI 2J scrolls the whole viewport into the full history ring:
        // contents rotate in place (A dropped, C added) without changing size.
        pane.process_pty_bytes(b"\x1b[2J", &mut dec, &mut coalescer, &mut state);
        let (after_rows, after_total, after_version) = pane.fetch_scrollback(0, 1, 10);

        assert_eq!(after_total, before_total, "history size must not change");
        assert_ne!(
            after_version, before_version,
            "rotated contents must bump version"
        );
        assert_eq!(after_rows.len(), 2);
        assert_eq!(after_rows[0].cells[0].character, "B");
        assert_eq!(after_rows[1].cells[0].character, "C");
    }

    #[test]
    fn clear_screen_saved_clears_history_and_advances_version() {
        let pane = match Pane::spawn_with_session(
            "clear-screen-saved-pane".to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            4,
            2,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            2,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn clear screen saved pane: {error}"),
        };
        let _ = pane.collect_dirty_rows();
        let mut dec = Dec2026Parser::new();
        let mut coalescer = AdaptiveCoalescer::new();
        let mut state = ReadLoopState::default();

        pane.process_pty_bytes(b"A\r\nB\r\nC\r\n", &mut dec, &mut coalescer, &mut state);
        let (_, before_total, before_version) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(before_total, 2);

        // CSI 3J erases the saved history entirely.
        pane.process_pty_bytes(b"\x1b[3J", &mut dec, &mut coalescer, &mut state);
        let (after_rows, after_total, after_version) = pane.fetch_scrollback(0, 1, 10);

        assert_eq!(after_total, 0, "saved history must be cleared");
        assert!(after_rows.is_empty());
        assert_ne!(after_version, before_version);
    }

    #[test]
    fn exit_hook_installed_after_exit_fires_exactly_once() {
        let pane = match Pane::spawn(
            "late-exit-hook".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            20,
            5,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn late-exit-hook pane: {error}"),
        };
        pane.set_alive(false);
        let calls = Arc::new(AtomicU64::new(0));
        let hook_calls = calls.clone();

        pane.set_exit_hook(Arc::new(move || {
            hook_calls.fetch_add(1, Ordering::SeqCst);
        }));
        pane.fire_exit_hook();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn oversized_grid_is_rejected_before_spawn_or_resize_mutation() {
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        assert!(Pane::spawn("oversized-spawn".to_string(), cwd.clone(), 4_097, 1, None).is_err());

        let pane = match Pane::spawn(
            "oversized-resize".to_string(),
            cwd,
            20,
            5,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn resize limit pane: {error}"),
        };
        let generation = pane.get_generation();
        assert!(pane.resize(4_097, 1).is_err());
        assert_eq!((pane.get_cols(), pane.get_rows()), (20, 5));
        assert_eq!(pane.get_generation(), generation);
    }

    fn spawn_viewport_pane(id: &str, cols: u32, rows: u32) -> Arc<Pane> {
        match Pane::spawn(
            id.to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            cols,
            rows,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn {id}: {error}"),
        }
    }

    fn record_viewport(pane: &Arc<Pane>, client_id: &str, cols: u32, rows: u32) {
        if let Err(error) = pane.set_client_viewport(client_id.to_string(), cols, rows) {
            panic!("record {client_id} viewport {cols}x{rows}: {error}");
        }
    }

    /// §16.2 Two clients of different sizes constrain the pane to the smallest
    /// dimensions, each axis minimized independently.
    #[test]
    fn min_fit_takes_the_smallest_viewport_per_axis() {
        let pane = spawn_viewport_pane("min-fit-per-axis", 120, 50);

        record_viewport(&pane, "wide-client", 100, 20);
        assert_eq!((pane.get_cols(), pane.get_rows()), (100, 20));

        record_viewport(&pane, "tall-client", 80, 40);
        assert_eq!(
            (pane.get_cols(), pane.get_rows()),
            (80, 20),
            "min-fit takes 80 cols from the narrow client and 20 rows from the short one"
        );
        assert_eq!(pane.client_viewport_count(), 2);
        assert_eq!(
            pane.min_fit_viewport(),
            Some(PaneViewport { cols: 80, rows: 20 })
        );
    }

    /// §16.2 A later, larger client must not overwrite an earlier smaller one —
    /// that is the multi-client size stomp this replaces.
    #[test]
    fn larger_client_attaching_later_does_not_grow_the_pane() {
        let pane = spawn_viewport_pane("min-fit-no-stomp", 120, 50);

        record_viewport(&pane, "small-client", 80, 24);
        record_viewport(&pane, "large-client", 200, 60);

        assert_eq!((pane.get_cols(), pane.get_rows()), (80, 24));
    }

    /// §16.2 Detaching the smallest client drops its constraint, so the pane
    /// grows back to what the remaining clients can display.
    #[test]
    fn removing_the_smallest_client_grows_the_pane_back() {
        let pane = spawn_viewport_pane("min-fit-detach-grow", 120, 50);

        record_viewport(&pane, "large-client", 120, 50);
        record_viewport(&pane, "small-client", 80, 24);
        assert_eq!((pane.get_cols(), pane.get_rows()), (80, 24));

        if let Err(error) = pane.remove_client_viewport("small-client") {
            panic!("remove small client viewport: {error}");
        }

        assert_eq!((pane.get_cols(), pane.get_rows()), (120, 50));
        assert_eq!(pane.client_viewport_count(), 1);
    }

    /// §3.3 / §16.3 A size change is a render-affecting change that row diffs
    /// cannot express, so it must bump the generation and force a full snapshot.
    #[test]
    fn min_fit_resize_bumps_generation_and_forces_full_snapshot() {
        let pane = spawn_viewport_pane("min-fit-generation", 120, 50);

        record_viewport(&pane, "first-client", 100, 30);
        let baseline = pane.get_generation();
        assert!(baseline > 0, "the first viewport report resizes the pane");

        record_viewport(&pane, "second-client", 60, 20);
        assert!(pane.get_generation() > baseline);

        match pane.fetch_grid_update(baseline).0 {
            grid_sync::GridUpdate::FullSnapshot { snapshot, .. } => {
                assert_eq!((snapshot.cols, snapshot.rows), (60, 20));
            }
            other => panic!("size change must force a full snapshot, got {other:?}"),
        }
    }

    /// A client re-reporting the size it already has must not publish a
    /// generation: every attached client reports on each of its own repaints.
    #[test]
    fn repeated_identical_viewport_report_does_not_publish_a_generation() {
        let pane = spawn_viewport_pane("min-fit-idempotent", 120, 50);

        record_viewport(&pane, "client", 80, 24);
        let generation = pane.get_generation();

        record_viewport(&pane, "client", 80, 24);
        record_viewport(&pane, "other-client", 100, 40);

        assert_eq!(pane.get_generation(), generation);
        assert_eq!((pane.get_cols(), pane.get_rows()), (80, 24));
    }

    /// An unusable viewport must be rejected before it can clamp the pane.
    #[test]
    fn invalid_client_viewport_is_rejected_without_being_recorded() {
        let pane = spawn_viewport_pane("min-fit-invalid", 120, 50);
        record_viewport(&pane, "good-client", 80, 24);
        let generation = pane.get_generation();

        assert!(
            pane.set_client_viewport("zero-client".to_string(), 0, 24)
                .is_err()
        );
        assert!(
            pane.set_client_viewport("huge-client".to_string(), 4_097, 24)
                .is_err()
        );

        assert_eq!(pane.client_viewport_count(), 1);
        assert_eq!((pane.get_cols(), pane.get_rows()), (80, 24));
        assert_eq!(pane.get_generation(), generation);
    }

    /// With no client left there is no constraint to fit, so the pane keeps its
    /// last size instead of collapsing.
    #[test]
    fn last_client_detaching_keeps_the_current_size() {
        let pane = spawn_viewport_pane("min-fit-last-detach", 120, 50);
        record_viewport(&pane, "only-client", 80, 24);

        if let Err(error) = pane.remove_client_viewport("only-client") {
            panic!("remove only client viewport: {error}");
        }

        assert_eq!(pane.client_viewport_count(), 0);
        assert_eq!(pane.min_fit_viewport(), None);
        assert_eq!((pane.get_cols(), pane.get_rows()), (80, 24));
    }

    /// Removing a client that never reported a viewport must be a no-op.
    #[test]
    fn removing_an_unknown_client_viewport_is_a_no_op() {
        let pane = spawn_viewport_pane("min-fit-unknown-client", 120, 50);
        record_viewport(&pane, "client", 80, 24);
        let generation = pane.get_generation();

        if let Err(error) = pane.remove_client_viewport("never-attached") {
            panic!("remove unknown client viewport: {error}");
        }

        assert_eq!(pane.client_viewport_count(), 1);
        assert_eq!(pane.get_generation(), generation);
    }

    fn spawn_marker_pane(id: &str, cols: u32, rows: u32, scrollback: usize) -> Arc<Pane> {
        match Pane::spawn_with_session(
            id.to_string(),
            String::new(),
            std::env::temp_dir().to_string_lossy().to_string(),
            cols,
            rows,
            Some(ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
            scrollback,
        ) {
            Ok(pane) => pane,
            Err(error) => panic!("spawn {id}: {error}"),
        }
    }

    /// Drives `process_pty_bytes` with the same persistent read-loop state a
    /// real PTY reader thread owns, so batch splitting behaves as in production.
    struct PtyFeed {
        dec: Dec2026Parser,
        coalescer: AdaptiveCoalescer,
        state: ReadLoopState,
    }

    impl PtyFeed {
        fn new() -> Self {
            Self {
                dec: Dec2026Parser::new(),
                coalescer: AdaptiveCoalescer::new(),
                state: ReadLoopState::default(),
            }
        }

        fn feed(&mut self, pane: &Arc<Pane>, bytes: &[u8]) {
            pane.process_pty_bytes(bytes, &mut self.dec, &mut self.coalescer, &mut self.state);
        }
    }

    fn marker_of(pane: &Arc<Pane>, kind: ShellMarkerKind) -> ShellMarker {
        match pane
            .shell_markers()
            .into_iter()
            .find(|marker| marker.kind == kind)
        {
            Some(marker) => marker,
            None => panic!(
                "no {kind:?} marker recorded, got {:?}",
                pane.shell_markers()
            ),
        }
    }

    /// §3.3 One shell command emits A/B/C/D at four different rows within a
    /// single PTY batch. Collapsing the batch into one cursor read would put all
    /// four on the row the batch ended on, which is what this pins down.
    #[test]
    fn one_command_records_each_marker_at_its_own_row_and_column() {
        let pane = spawn_marker_pane("osc133-rows", 20, 6, 100);
        let mut feed = PtyFeed::new();

        feed.feed(
            &pane,
            b"\x1b]133;A\x07user@host$ \x1b]133;B\x07echo hi\r\n\
              \x1b]133;C\x07hi\r\n\x1b]133;D;0\x07",
        );

        let markers = pane.shell_markers();
        assert_eq!(markers.len(), 4, "recorded {markers:?}");
        assert_eq!(
            markers.iter().map(|m| m.kind).collect::<Vec<_>>(),
            vec![
                ShellMarkerKind::PromptStart,
                ShellMarkerKind::CommandStart,
                ShellMarkerKind::OutputStart,
                ShellMarkerKind::CommandEnd,
            ]
        );
        assert_eq!(
            markers.iter().map(|m| m.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        let positions = markers
            .iter()
            .map(|marker| (pane.locate_shell_marker(marker), marker.column))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                (ShellMarkerPosition::Viewport { line: 0 }, 0),
                (ShellMarkerPosition::Viewport { line: 0 }, 11),
                (ShellMarkerPosition::Viewport { line: 1 }, 0),
                (ShellMarkerPosition::Viewport { line: 2 }, 0),
            ]
        );
        assert_eq!(
            pane.get_prompt_marker(),
            4,
            "the existing prompt marker counter must keep counting every marker"
        );
    }

    /// §3.3 `OSC 133 ; D ; <status>` carries the command's exit status; the
    /// previous scanner stopped at the marker letter and never read it.
    #[test]
    fn command_end_marker_records_its_exit_status_when_present() {
        let pane = spawn_marker_pane("osc133-exit", 20, 6, 100);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;D;1\x07\x1b]133;D\x07\x1b]133;D;0\x07");
        feed.feed(&pane, b"\x1b]133;D;not-a-number\x07\x1b]133;A\x07");

        assert_eq!(
            pane.shell_markers()
                .iter()
                .map(|marker| (marker.kind, marker.exit_code))
                .collect::<Vec<_>>(),
            vec![
                (ShellMarkerKind::CommandEnd, Some(1)),
                (ShellMarkerKind::CommandEnd, None),
                (ShellMarkerKind::CommandEnd, Some(0)),
                (ShellMarkerKind::CommandEnd, None),
                (ShellMarkerKind::PromptStart, None),
            ]
        );
    }

    /// §3.3 A real PTY splits escape sequences at arbitrary byte boundaries. A
    /// scanner restarted per batch drops those markers entirely.
    #[test]
    fn marker_split_across_pty_batches_is_still_recorded_at_its_row() {
        let pane = spawn_marker_pane("osc133-split", 20, 6, 100);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"ab\x1b]13");
        assert!(
            pane.shell_markers().is_empty(),
            "an unterminated sequence must not record anything yet"
        );
        feed.feed(&pane, b"3;D;7\x1b");
        assert!(pane.shell_markers().is_empty(), "ST is still incomplete");
        feed.feed(&pane, b"\\cd");

        let marker = marker_of(&pane, ShellMarkerKind::CommandEnd);
        assert_eq!(marker.exit_code, Some(7));
        assert_eq!(
            (pane.locate_shell_marker(&marker), marker.column),
            (ShellMarkerPosition::Viewport { line: 0 }, 2),
            "the marker belongs to the cursor position after \"ab\", not after \"abcd\""
        );
        assert_eq!(pane.get_full_snapshot().cells[0].character, "a");
        assert_eq!(pane.get_full_snapshot().cells[2].character, "c");
    }

    /// §3.3 Scrollback is addressed `0..history_size` with 0 = oldest, so every
    /// row that scrolls off renumbers every row. The absolute id must survive
    /// that and still name the same text.
    #[test]
    fn absolute_rows_convert_to_scrollback_indices_after_scrolling() {
        let pane = spawn_marker_pane("osc133-scroll", 20, 3, 50);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07one\r\n");
        feed.feed(&pane, b"\x1b]133;B\x07two\r\n");
        feed.feed(&pane, b"three\r\nfour\r\nfive\r\n");

        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        let command_start = marker_of(&pane, ShellMarkerKind::CommandStart);
        assert_eq!(
            (
                pane.locate_shell_marker(&prompt_start),
                pane.locate_shell_marker(&command_start)
            ),
            (
                ShellMarkerPosition::History { index: 0 },
                ShellMarkerPosition::History { index: 1 }
            )
        );

        let (lines, total, _) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(total, 3);
        assert_eq!(lines[0].cells[0].character, "o", "index 0 is the A row");
        assert_eq!(lines[1].cells[0].character, "t", "index 1 is the B row");
    }

    /// §3.3 A row that scrolled past the oldest scrollback row is gone, so it
    /// has no index. A wrong row is worse than no row. The numbering itself
    /// stays live: rotation at capacity is performed here, so it is counted.
    #[test]
    fn markers_become_unavailable_once_their_row_is_evicted() {
        let pane = spawn_marker_pane("osc133-evict", 20, 2, 2);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07one\r\n");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Viewport { line: 0 }
        );
        let epoch = pane.row_addressing_epoch();

        feed.feed(&pane, b"two\r\nthree\r\nfour\r\nfive\r\n");

        assert_eq!(
            pane.row_addressing_epoch(),
            epoch,
            "rotation at capacity is counted, so it must not retire the numbering"
        );
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Unavailable,
            "\"one\" fell past the oldest surviving scrollback row"
        );
        assert_eq!(
            pane.shell_markers().len(),
            1,
            "the record itself survives so its kind and exit status stay readable"
        );

        // The surviving numbering must still address rows, not merely report
        // Unavailable for everything. B names the row "six" is typed on, and
        // that row must keep being named as the rotation moves it.
        feed.feed(&pane, b"\x1b]133;B\x07six\r\n");
        let command_start = marker_of(&pane, ShellMarkerKind::CommandStart);
        assert_eq!(
            pane.locate_shell_marker(&command_start),
            ShellMarkerPosition::Viewport { line: 0 }
        );
        assert_eq!(pane.get_full_snapshot().cells[0].character, "s");

        feed.feed(&pane, b"seven\r\n");
        assert_eq!(
            pane.locate_shell_marker(&command_start),
            ShellMarkerPosition::History { index: 1 }
        );
        let (lines, total, _) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(total, 2, "capacity is still honored");
        assert_eq!(
            lines[1].cells[0].character, "s",
            "index 1 is the row the B marker named"
        );
    }

    /// §3.3 The eviction floor is the second guard, independent of the epoch: a
    /// row older than the oldest surviving scrollback row has no index.
    #[test]
    fn rows_below_the_eviction_floor_resolve_to_unavailable() {
        let pane = spawn_marker_pane("osc133-floor", 20, 4, 100);
        let epoch = pane.row_addressing_epoch();
        pane.viewport_top_absolute.store(1_000, Ordering::Release);
        let marker = |absolute_row| ShellMarker {
            sequence: 1,
            kind: ShellMarkerKind::PromptStart,
            absolute_row,
            column: 0,
            exit_code: None,
            epoch,
        };

        assert_eq!(
            pane.resolve_shell_marker(&marker(989), 10, 4),
            ShellMarkerPosition::Unavailable,
            "990 is the oldest surviving row"
        );
        assert_eq!(
            pane.resolve_shell_marker(&marker(990), 10, 4),
            ShellMarkerPosition::History { index: 0 }
        );
        assert_eq!(
            pane.resolve_shell_marker(&marker(999), 10, 4),
            ShellMarkerPosition::History { index: 9 }
        );
        assert_eq!(
            pane.resolve_shell_marker(&marker(1_003), 10, 4),
            ShellMarkerPosition::Viewport { line: 3 }
        );
        assert_eq!(
            pane.resolve_shell_marker(&marker(1_004), 10, 4),
            ShellMarkerPosition::Unavailable,
            "past the last viewport row"
        );
    }

    /// §15 Losing marker positions across a resize is accepted; silently
    /// reporting the pre-reflow row is not.
    #[test]
    fn resize_retires_recorded_rows_instead_of_reflowing_them() {
        let pane = spawn_marker_pane("osc133-resize", 20, 4, 100);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07prompt\r\nsecond\r\n");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Viewport { line: 0 }
        );

        if let Err(error) = pane.resize(10, 4) {
            panic!("resize marker pane: {error}");
        }

        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Unavailable
        );

        feed.feed(&pane, b"\x1b]133;B\x07after\r\n");
        let command_start = marker_of(&pane, ShellMarkerKind::CommandStart);
        assert_eq!(
            pane.locate_shell_marker(&command_start),
            ShellMarkerPosition::Viewport { line: 2 },
            "markers recorded after the resize address rows again"
        );
    }

    /// §3.3 The alternate grid has no scrollback of its own, so its size changes
    /// must not be read as rows leaving the primary viewport.
    #[test]
    fn alternate_screen_switch_retires_recorded_rows() {
        let pane = spawn_marker_pane("osc133-alt", 20, 4, 100);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07prompt\r\n");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        let epoch = pane.row_addressing_epoch();

        feed.feed(&pane, b"\x1b[?1049h");

        assert_ne!(pane.row_addressing_epoch(), epoch);
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Unavailable
        );
    }

    /// §3.3 OSC 7 shares the scanner with OSC 133, including the split-batch and
    /// unrelated-OSC paths.
    #[test]
    fn osc7_cwd_survives_the_shared_scanner() {
        let pane = spawn_marker_pane("osc7-cwd", 20, 4, 100);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]7;file://localhost/tmp/z3rm%20osc7\x07");
        assert_eq!(pane.get_cwd(), "/tmp/z3rm osc7");

        feed.feed(&pane, b"\x1b]7;file://localhost/tmp/split");
        assert_eq!(pane.get_cwd(), "/tmp/z3rm osc7", "no ST yet");
        feed.feed(&pane, b"-path\x1b\\");
        assert_eq!(pane.get_cwd(), "/tmp/split-path");

        // An unrelated OSC whose payload looks like a marker must not record one,
        // and must not desynchronize the scanner for the next real sequence.
        feed.feed(&pane, b"\x1b]0;133;A window title\x07\x1b]133;A\x07");
        assert_eq!(
            pane.shell_markers()
                .iter()
                .map(|marker| marker.kind)
                .collect::<Vec<_>>(),
            vec![ShellMarkerKind::PromptStart]
        );
        assert_eq!(pane.get_cwd(), "/tmp/split-path");
    }

    /// §3.3 A long-running pane must not accumulate markers without bound.
    #[test]
    fn recorded_markers_are_capped() {
        let pane = spawn_marker_pane("osc133-cap", 20, 4, 100_000);
        let mut feed = PtyFeed::new();

        let mut batch = Vec::new();
        for _ in 0..MAX_RECORDED_SHELL_MARKERS + 32 {
            batch.extend_from_slice(b"\x1b]133;A\x07");
        }
        feed.feed(&pane, &batch);

        let markers = pane.shell_markers();
        assert_eq!(markers.len(), MAX_RECORDED_SHELL_MARKERS);
        assert_eq!(
            markers.iter().map(|marker| marker.sequence).min(),
            Some(33),
            "the oldest entries are the ones dropped"
        );
        assert_eq!(
            pane.get_prompt_marker() as usize,
            MAX_RECORDED_SHELL_MARKERS + 32,
            "the counter still sees every marker"
        );
    }

    /// §3.3 A long-lived pane spends nearly all its life with scrollback full,
    /// so this is the case that decides whether recorded rows are worth
    /// anything. Alacritty stops growing `history_size` there, which used to
    /// make a marker unresolvable in the very batch that recorded it.
    #[test]
    fn markers_stay_addressable_after_scrollback_fills() {
        let pane = spawn_marker_pane("osc133-full", 20, 4, 10);
        let mut feed = PtyFeed::new();
        for index in 0..40 {
            feed.feed(&pane, format!("fill{index}\r\n").as_bytes());
        }
        let (_, total, _) = pane.fetch_scrollback(0, 1, 1);
        assert_eq!(total, 10, "scrollback is at capacity");
        let epoch = pane.row_addressing_epoch();

        feed.feed(&pane, b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        let output_start = marker_of(&pane, ShellMarkerKind::OutputStart);
        assert_eq!(
            pane.locate_shell_marker(&output_start),
            ShellMarkerPosition::Viewport { line: 3 },
            "the batch that scrolls must not invalidate the marker it just recorded"
        );

        feed.feed(&pane, b"a\r\nb\r\nc\r\n");
        feed.feed(&pane, b"\x1b]133;D;0\x07");

        assert_eq!(
            pane.row_addressing_epoch(),
            epoch,
            "a plain command at full scrollback must not retire the numbering"
        );
        assert_eq!(
            pane.locate_shell_marker(&output_start),
            ShellMarkerPosition::Viewport { line: 0 },
            "C names the first output row, which the three output lines pushed to the top"
        );
        assert_eq!(pane.get_full_snapshot().cells[0].character, "a");

        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::History { index: 9 },
            "the prompt row rotated into scrollback rather than becoming unaddressable"
        );
        let (lines, _, _) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(
            lines[9].cells[0].character, "$",
            "index 9 is the row the A marker named"
        );
        assert_eq!(
            marker_of(&pane, ShellMarkerKind::CommandEnd).exit_code,
            Some(0)
        );
    }

    /// §3.3 With scrollback disabled every row that leaves the viewport is gone
    /// at once. Reporting the viewport line it used to occupy would name
    /// whatever text scrolled into that line since.
    #[test]
    fn disabled_scrollback_reports_no_row_rather_than_a_stale_one() {
        let pane = spawn_marker_pane("osc133-nohistory", 20, 4, 0);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07zero\r\n");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Viewport { line: 0 }
        );

        feed.feed(&pane, b"x\r\ny\r\nz\r\nw\r\n");

        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Unavailable,
            "the \"zero\" row is gone; viewport line 0 now holds \"y\""
        );
        assert_eq!(pane.get_full_snapshot().cells[0].character, "y");
        let (_, total, _) = pane.fetch_scrollback(0, 1, 10);
        assert_eq!(total, 0, "no scrollback is retained");
    }

    /// A batch far larger than `ROW_ADDRESSING_HEADROOM` must stay counted:
    /// the batch is fed in steps small enough that no step can append more rows
    /// than the headroom holds.
    #[test]
    fn a_batch_longer_than_the_headroom_stays_counted() {
        let pane = spawn_marker_pane("osc133-longbatch", 20, 4, 64);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07anchor\r\n");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        let epoch = pane.row_addressing_epoch();

        let mut burst = Vec::new();
        while burst.len() < 8192 {
            burst.extend_from_slice(b"x\r\n");
        }
        let appended = burst.len() / 3;
        assert!(appended > ROW_ADDRESSING_HEADROOM);
        feed.feed(&pane, &burst);

        assert_eq!(
            pane.row_addressing_epoch(),
            epoch,
            "step splitting must keep a burst from saturating the headroom"
        );
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Unavailable,
            "the anchor row scrolled past the oldest surviving row"
        );

        feed.feed(&pane, b"\x1b]133;B\x07tail\r\n");
        let command_start = marker_of(&pane, ShellMarkerKind::CommandStart);
        assert_eq!(
            pane.locate_shell_marker(&command_start),
            ShellMarkerPosition::Viewport { line: 2 },
            "B names the row \"tail\" is typed on, which one scroll moved up"
        );
        assert_eq!(pane.get_full_snapshot().cells[2 * 20].character, "t");

        feed.feed(&pane, b"p\r\nq\r\nr\r\n");
        assert_eq!(
            pane.locate_shell_marker(&command_start),
            ShellMarkerPosition::History { index: 63 }
        );
        let (lines, total, _) = pane.fetch_scrollback(0, 1, 64);
        assert_eq!(total, 64, "capacity is honored after the burst");
        assert_eq!(lines[63].cells[0].character, "t");
    }

    /// `SU` scrolls by up to a screen height per sequence, so enough of them in
    /// one parse step can still outrun the headroom. That is the one remaining
    /// case where the emulator evicts rows it does not report, and it must
    /// retire the numbering rather than drift.
    #[test]
    fn a_scroll_burst_past_the_headroom_retires_the_numbering() {
        let pane = spawn_marker_pane("osc133-saturate", 20, 60, 10);
        let mut feed = PtyFeed::new();

        feed.feed(&pane, b"\x1b]133;A\x07anchor\r\n");
        let prompt_start = marker_of(&pane, ShellMarkerKind::PromptStart);
        let epoch = pane.row_addressing_epoch();

        let mut burst = String::new();
        while burst.len() < ROW_ADDRESSING_HEADROOM {
            burst.push_str("\x1b[60S");
        }
        feed.feed(&pane, burst.as_bytes());

        assert_ne!(
            pane.row_addressing_epoch(),
            epoch,
            "growth the emulator clamped away cannot be counted"
        );
        assert_eq!(
            pane.locate_shell_marker(&prompt_start),
            ShellMarkerPosition::Unavailable
        );
        let (_, total, _) = pane.fetch_scrollback(0, 1, 64);
        assert_eq!(
            total, 10,
            "capacity is honored even when the numbering retires"
        );
    }

    /// §16.3 The Interactive tier depends on `write_input` publishing keyboard
    /// activity to the PTY reader thread's coalescer.
    #[test]
    fn write_input_marks_the_pane_keyboard_active() {
        let pane = spawn_viewport_pane("keyboard-activity", 20, 5);
        let before = Instant::now();
        assert!(!pane.keyboard_activity.is_active_at(before));

        if let Err(error) = pane.write_input(b"a") {
            panic!("write input: {error}");
        }

        assert!(pane.keyboard_activity.is_active_at(Instant::now()));
    }
}

/// §3.3 简单百分号解码 (OSC 7 URI 路径)。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// §3.3 poll(2) the PTY master fd so the read loop can wake for BSU timeout
/// without consuming bytes. Returns true if readable/error (caller should read),
/// false on timeout. On non-unix, always true (blocking read path).
#[cfg(unix)]
fn poll_fd_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single pollfd, valid fd from portable-pty master.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    rc > 0
}

#[cfg(not(unix))]
fn poll_fd_readable(_fd: i32, _timeout_ms: i32) -> bool {
    true
}

#[cfg(target_family = "wasm")]
impl Pane {
    /// §3.1 Feed bytes the guest produced into the emulator.
    ///
    /// This is the browser's equivalent of one `read()` returning in the
    /// native reader thread, and it runs the identical path: same parser, same
    /// coalescer, same dirty-row accounting, so a pane's generation advances
    /// and `PaneDirty` fires exactly as it does on a real pty.
    pub fn push_guest_output(self: &Arc<Self>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // The lock is per pane and only this entry point takes it, so a guest
        // callback re-entering while a batch is in flight would be a bug in the
        // bridge rather than contention to wait on.
        let Some(mut state) = self.guest_output_state.try_lock() else {
            tracing::warn!(pane_id = %self.id, "guest output arrived while a batch was still being processed; dropped");
            return;
        };
        let (dec, coalescer, read_loop) = &mut *state;
        self.process_pty_bytes(bytes, dec, coalescer, read_loop);
    }

    /// §3.1 The sink carrying this pane's writes toward the guest.
    ///
    /// Installed by the JS bridge once the emulator's serial input is ready.
    pub fn set_guest_input_handler(&self, handler: Box<dyn Fn(&[u8])>) {
        self.pty_master.lock().set_input_handler(handler);
    }

    /// §3.1 The size this pane last resized its pty to.
    ///
    /// A serial line carries no window size, so the bridge reads this and tells
    /// the guest itself.
    pub fn guest_pty_size(&self) -> crate::pty::PtySize {
        self.pty_master.lock().size()
    }
}
