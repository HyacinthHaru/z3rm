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
use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    ClearMode, Handler, PrivateMode, Processor, Rgb, StdSyncHandler,
};
use anyhow::Context as _;
use mux_protocol::Notification as MuxNotification;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtyPair, PtySize, PtySystem};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;

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
    scrollback_capacity: AtomicU64,
    history_version: AtomicU64,
    /// §3.1 PTY master (用于 resize / reader clone)。
    pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// §3.1 PTY writer (单一 writer)。
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// §3.5 child 进程 handle (用于 kill/wait)。
    child: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>,
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
    notification_hook:
        parking_lot::Mutex<Option<Arc<dyn Fn(MuxNotification) + Send + Sync>>>,
    /// §16.6 Optional hook for ClipboardStore events from the emulator.
    clipboard_hook: parking_lot::Mutex<Option<Box<dyn Fn(String) + Send>>>,
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
}

impl HistoryMutationObserver {
    fn reset(&mut self) {
        self.may_rotate = false;
    }

    fn mark_rotation(&mut self) {
        self.may_rotate = true;
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

    fn clear_screen(&mut self, _: ClearMode) {}

    fn reset_state(&mut self) {}

    fn reverse_index(&mut self) {
        self.mark_rotation();
    }

    fn set_color(&mut self, _: usize, _: Rgb) {}

    fn reset_color(&mut self, _: usize) {}

    fn decaln(&mut self) {}

    fn set_private_mode(&mut self, _: PrivateMode) {}

    fn unset_private_mode(&mut self, _: PrivateMode) {}
}

/// §3.3 PTY read-loop 本地状态: DEC-2026 同步延迟 + coalescing 通知节流。
/// 仅在单一 PTY read 线程内顺序访问, 无需同步原语。
struct ReadLoopState {
    /// Persistent parsers preserve escape sequences split across PTY reads.
    terminal_processor: Processor<StdSyncHandler>,
    history_processor: Processor<StdSyncHandler>,
    history_observer: HistoryMutationObserver,
    /// BSU..ESU 同步窗口内累积了尚未发布的变更
    pending_sync: bool,
    /// Dirty rows accumulated across a DEC-2026 synchronized update window.
    pending_dirty_rows: Vec<usize>,
    /// Whether the window changed state absent from row diffs.
    pending_full_snapshot: bool,
    /// 有被 coalescing 推迟、待窗口到期补发的 PaneDirty
    pending_notify: bool,
}

impl Default for ReadLoopState {
    fn default() -> Self {
        Self {
            terminal_processor: Processor::new(),
            history_processor: Processor::new(),
            history_observer: HistoryMutationObserver::default(),
            pending_sync: false,
            pending_dirty_rows: Vec::new(),
            pending_full_snapshot: false,
            pending_notify: false,
        }
    }
}

fn initial_history_version() -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    nanoid::nanoid!().hash(&mut hasher);
    hasher.finish().max(1)
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
            CommandBuilder::new(shell)
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
            scrollback_capacity: AtomicU64::new(scrollback_lines as u64),
            // A random non-zero authority epoch prevents a client from reusing
            // cached history after the daemon reconstructs this pane.
            history_version: AtomicU64::new(initial_history_version()),
            pty_master: Arc::new(Mutex::new(pair.master)),
            pty_writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(Some(child))),
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
        });

        // §3.1 启动 PTY read loop — 后台线程持续读取 PTY 输出, 喂给 alacritty,
        // 计算 dirty diff, bump generation。线程持有弱引用, pane drop 时自动结束。
        pane.clone().start_pty_read_loop(reader, master_raw_fd);
        Ok(pane)
    }

    /// §3.1 启动 PTY read 后台线程。
    ///
    /// 该线程持续从 PTY 读取字节, 喂给 alacritty Term, 然后从 dirty_lines
    /// 提取变更行, 生成 GridDiff, push 到 ring 并 bump generation。
    /// Bump generation 后由 connection 层 fan-out PaneDirty 通知到所有 client。
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
    fn process_pty_bytes(
        self: &Arc<Self>,
        bytes: &[u8],
        dec: &mut Dec2026Parser,
        coalescer: &mut AdaptiveCoalescer,
        state: &mut ReadLoopState,
    ) {
        let transitions = dec.parse(bytes);
        let in_sync = dec.is_in_sync();
        // §16.3 Re-classify on the byte volume of this batch before any
        // notification decision uses the resulting window.
        coalescer.on_output(bytes.len());
        self.flush_pending_notify(state, coalescer);

        let commit = self.commit.lock();
        state.history_observer.reset();
        state
            .history_processor
            .advance(&mut state.history_observer, bytes);
        let (
            render_state_changed,
            history_size_before,
            history_size_after,
            cursor_row_unchanged,
            modes_after,
        ) = {
            let mut term = self.term.lock();
            let before = (
                term.grid().cursor.point,
                term.cursor_style(),
                term.grid().display_offset(),
                modes_from_alacritty(*term.mode()),
            );
            let history_size_before = term.grid().history_size();
            state.terminal_processor.advance(&mut *term, bytes);
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
            )
        };
        self.set_bracketed_paste_mode(
            modes_after & mux_protocol::terminal_mode::BRACKETED_PASTE != 0,
        );
        let (dirty_rows, _fully_damaged) = self.collect_dirty_rows();
        // A VTE scroll can rotate a full history ring without changing its
        // length. Ordinary input and color changes do not invalidate the
        // history checkpoint; only a size change or a possible rotation at
        // the configured capacity does.
        let history_capacity = self.scrollback_capacity.load(Ordering::Acquire);
        let history_changed = history_size_before != history_size_after
            || (history_capacity > 0
                && history_size_after as u64 >= history_capacity
                && cursor_row_unchanged
                && state.history_observer.may_rotate);
        if history_changed {
            self.history_version.fetch_add(1, Ordering::AcqRel);
        }

        let grid_changed = !dirty_rows.is_empty() || render_state_changed || history_changed;
        let should_broadcast_dirty = if in_sync && !transitions.ended() {
            if grid_changed {
                state.pending_sync = true;
                state.pending_dirty_rows.extend(dirty_rows);
                state.pending_full_snapshot |= render_state_changed || history_changed;
            }
            false
        } else if grid_changed || state.pending_sync {
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

        self.broadcast_pane_output(bytes, output_sequence);
        self.handle_pending_events();
        self.parse_osc_sequences(bytes);
        if should_broadcast_dirty {
            self.broadcast_pane_dirty();
        }
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
    pub fn set_notification_hook(
        &self,
        hook: Arc<dyn Fn(MuxNotification) + Send + Sync>,
    ) {
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

    pub fn fetch_scrollback(
        &self,
        from_line: u32,
        direction: u32,
        count: u32,
    ) -> (Vec<grid_sync::RowChange>, u32, u64) {
        let _commit = self.commit.lock();
        let term = self.term.lock();
        let (lines, total) =
            grid_sync::fetch_scrollback_from_term(&*term, from_line, direction, count);
        let version = self.history_version.load(Ordering::Acquire);
        (lines, total, version)
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
    pub fn set_zoomed(&self, zoomed: bool) {
        let commit = self.commit.lock();
        self.zoomed.store(zoomed, Ordering::SeqCst);
        self.bump_generation_locked();
        drop(commit);
        self.broadcast_pane_dirty();
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

    /// §3.3 扫描 PTY 输出中的 OSC 7 / OSC 133 序列。
    ///
    /// OSC 7: `ESC ] 7 ; file://HOST/PATH ST` — shell 报告当前工作目录。
    /// OSC 133: `ESC ] 133 ; MARKER ST` — 语义 prompt 标记 (A=prompt start,
    ///   B=command start, C=output start, D=command end)。
    ///
    /// ST (String Terminator) 可以是 BEL (0x07) 或 ESC \ (0x1b 0x5c)。
    fn parse_osc_sequences(&self, bytes: &[u8]) {
        let mut i = 0;
        while i + 3 < bytes.len() {
            // 寻找 OSC 引入: ESC ]
            if bytes[i] != 0x1b || bytes[i + 1] != b']' {
                i += 1;
                continue;
            }
            i += 2;

            // 解析 OSC 编号
            let num_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == num_start || i >= bytes.len() {
                continue;
            }
            let osc_num: u32 = match std::str::from_utf8(&bytes[num_start..i]) {
                Ok(s) => s.parse().unwrap_or(u32::MAX),
                Err(_) => u32::MAX,
            };

            // OSC 7: 期望 ';' 后跟 URI
            if osc_num == 7 {
                if i < bytes.len() && bytes[i] == b';' {
                    i += 1;
                    let payload_start = i;
                    let payload_end = self.find_osc_terminator(bytes, i);
                    if let Some(end) = payload_end {
                        if let Ok(uri) = std::str::from_utf8(&bytes[payload_start..end]) {
                            self.handle_osc7_cwd(uri);
                        }
                        i = end;
                    }
                }
                continue;
            }

            // OSC 133: 期望 ';' 后跟 marker 字符
            if osc_num == 133 {
                if i < bytes.len() && bytes[i] == b';' {
                    i += 1;
                    if i < bytes.len() {
                        self.handle_osc133_marker(bytes[i]);
                    }
                }
                continue;
            }
        }
    }

    /// 从 `start` 位置寻找 OSC 终止符 (BEL 或 ESC \), 返回 payload 结束位置。
    fn find_osc_terminator(&self, bytes: &[u8], start: usize) -> Option<usize> {
        let mut i = start;
        while i < bytes.len() {
            if bytes[i] == 0x07 {
                return Some(i);
            }
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == 0x5c {
                return Some(i);
            }
            i += 1;
        }
        None
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

    /// §3.3 处理 OSC 133 marker: 递增 prompt marker 计数。
    fn handle_osc133_marker(&self, marker: u8) {
        // A = prompt start, B = command start, C = output start, D = command end
        if matches!(marker, b'A' | b'B' | b'C' | b'D') {
            self.prompt_marker.fetch_add(1, Ordering::SeqCst);
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
            notifications.get(1).and_then(|notification| notification.event.as_ref()),
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
