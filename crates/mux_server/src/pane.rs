// §3.1 Pane — PTY + alacritty terminal emulator + grid diff ring.
//
// mux_server 是 server-canonical 模型中终端状态的唯一拥有者 (spec §3.1):
// PTY fd、alacritty Term、scrollback、generation counter 全部在此进程内。
// 客户端只渲染我们 push 过来的 grid diff / snapshot。

use crate::grid_sync::{
    self, diff_from_dirty, snapshot_from_term, GridDiff, GridDiffRing, FullGridSnapshot,
    ScrollbackBuffer, ScrollbackVersion,
};
use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use mux_protocol::Notification as MuxNotification;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtyPair, PtySize, PtySystem};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// §3.1 真正拥有 alacritty Term + PTY pair 的 Pane (server-canonical)。
pub struct Pane {
    pub id: String,
    pub cwd: Arc<parking_lot::RwLock<String>>,
    pub title: Arc<parking_lot::RwLock<String>>,
    pub command: Option<String>,
    /// §3.1 alacritty 终端实例 (server-canonical, 真实 VT 解析)。
    pub term: Arc<parking_lot::Mutex<Term<PaneEventListener>>>,
    /// §3.3 generation counter (每次 grid-affecting 变化递增)。
    pub generation: AtomicU64,
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
    pub scrollback_buffer: Arc<parking_lot::RwLock<ScrollbackBuffer>>,
    pub scrollback_version: Arc<parking_lot::RwLock<ScrollbackVersion>>,
    /// §3.1 PTY master (用于 resize / reader clone)。
    pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// §3.1 PTY writer (单一 writer)。
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// §3.5 child 进程 handle (用于 kill/wait)。
    child: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>,
    /// §3.3 event 收集: alacritty 事件 → main loop。
    pub events: Arc<parking_lot::Mutex<Vec<AlacEvent>>>,
    /// §3.3 PaneDirty 订阅者: 每个连接的 notification_tx。
    /// PTY read loop bump generation 后 fan-out PaneDirty 到所有订阅者。
    subscribers: Arc<parking_lot::RwLock<Vec<mpsc::UnboundedSender<MuxNotification>>>>,
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

/// §3.10 Shell command (从 proto ShellCommand 转换)
#[derive(Clone, Debug, Default)]
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

impl Pane {
    /// §3.10 创建新 pane: spawn PTY + alacritty Term + 启动 read loop。
    ///
    /// 返回 Arc 因为 PTY read 线程持有弱引用, pane drop 时自动结束。
    pub fn spawn(
        id: String,
        cwd: String,
        cols: u32,
        rows: u32,
        command: Option<ShellCommand>,
    ) -> anyhow::Result<Arc<Self>> {
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let listener = PaneEventListener { events: events.clone() };

        let term_config = TermConfig::default();
        let size = TermSize::new(cols as usize, rows as usize);
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

        // §3.1 spawn 子进程
        let child = pair.slave.spawn_command(cmd)?;

        // §3.1 获取 reader / writer
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        // slave 端已经不需要了 (drop 让 child 持有)
        drop(pair.slave);

        let command_str = command.as_ref().map(|c| format!("{} {}", c.program, c.args.join(" ")));

        let pane = Arc::new(Pane {
            id: id.clone(),
            cwd: Arc::new(parking_lot::RwLock::new(cwd)),
            title: Arc::new(parking_lot::RwLock::new(String::new())),
            command: command_str,
            term: Arc::new(parking_lot::Mutex::new(term)),
            generation: AtomicU64::new(0),
            grid_diff_ring: Arc::new(parking_lot::RwLock::new(GridDiffRing::new(64))),
            alive: AtomicBool::new(true),
            cols: AtomicU64::new(cols as u64),
            rows: AtomicU64::new(rows as u64),
            bracketed_paste_mode: AtomicBool::new(false),
            zoomed: AtomicBool::new(false),
            prompt_marker: AtomicU64::new(0),
            scrollback_buffer: Arc::new(parking_lot::RwLock::new(ScrollbackBuffer::new(10_000))),
            scrollback_version: Arc::new(parking_lot::RwLock::new(ScrollbackVersion::new())),
            pty_master: Arc::new(Mutex::new(pair.master)),
            pty_writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(Some(child))),
            events,
            subscribers: Arc::new(parking_lot::RwLock::new(Vec::new())),
        });

        // §3.1 启动 PTY read loop — 后台线程持续读取 PTY 输出, 喂给 alacritty,
        // 计算 dirty diff, bump generation。线程持有弱引用, pane drop 时自动结束。
        pane.clone().start_pty_read_loop(reader);

        Ok(pane)
    }

    /// §3.1 启动 PTY read 后台线程。
    ///
    /// 该线程持续从 PTY 读取字节, 喂给 alacritty Term, 然后从 dirty_lines
    /// 提取变更行, 生成 GridDiff, push 到 ring 并 bump generation。
    /// Bump generation 后由 connection 层 fan-out PaneDirty 通知到所有 client。
    fn start_pty_read_loop(self: Arc<Self>, mut reader: Box<dyn Read + Send>) {
        let pane_weak = Arc::downgrade(&self);
        // 持有 strong ref 直到线程启动, 防止 race
        let pane_strong = self.clone();

        std::thread::Builder::new()
            .name(format!("pty-read-{}", self.id))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    let Some(pane) = pane_weak.upgrade() else {
                        // Pane 已 drop, 退出线程
                        return;
                    };

                    match reader.read(&mut buf) {
                        Ok(0) => {
                            // EOF — 子进程关闭了 stdout, 标记 pane dead
                            pane.set_alive(false);
                            return;
                        }
                        Ok(n) => {
                            pane.process_pty_bytes(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                            continue;
                        }
                        Err(_) => {
                            pane.set_alive(false);
                            return;
                        }
                    }
                }
                #[allow(unreachable_code)]
                drop(pane_strong);
            })
            .ok();
    }

    /// §3.1 喂 PTY 字节给 alacritty Term, 处理事件, 计算 diff, bump generation。
    fn process_pty_bytes(self: &Arc<Self>, bytes: &[u8]) {
        // §3.1 喂字节给 alacritty (它会调用 Handler 方法更新 grid)
        {
            let mut term = self.term.lock();
            let mut processor = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
            processor.advance(&mut *term, bytes);
        }

        // §3.3 收集 dirty 行 + 处理事件
        let dirty_rows = self.collect_dirty_rows();
        self.handle_pending_events();

        // §3.3 解析 OSC 7 (cwd) / OSC 133 (prompt markers) — 在 alacritty
        // 处理之外独立扫描, 因为 alacritty EventListener 不暴露 OSC 事件。
        self.parse_osc_sequences(bytes);

        if !dirty_rows.is_empty() {
            // §3.3 生成 diff
            let diff = {
                let term = self.term.lock();
                diff_from_dirty(&*term, &dirty_rows)
            };

            // §3.3 bump generation → push 到 ring
            let new_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.grid_diff_ring.write().push(new_gen, diff);

            // §3.3 fan-out PaneDirty 到所有订阅者 (at-most-once)
            self.broadcast_pane_dirty();
        }
    }

    /// §3.3 / §3.4 向所有订阅者推送 PaneDirty (at-most-once, 丢失无害)。
    /// 关闭的订阅者会被自动清理。
    fn broadcast_pane_dirty(&self) {
        let notif = MuxNotification {
            event: Some(mux_protocol::notification::Event::PaneDirty(
                mux_protocol::PaneDirty { pane_id: self.id.clone() },
            )),
        };
        let subs = self.subscribers.read().clone();
        // 收集失败的 sender, 后面移除 (避免持有 closed channel)
        let mut dead = Vec::new();
        for (i, tx) in subs.iter().enumerate() {
            if tx.send(notif.clone()).is_err() {
                dead.push(i);
            }
        }
        if !dead.is_empty() {
            let mut live = self.subscribers.write();
            for i in dead.into_iter().rev() {
                if i < live.len() {
                    live.remove(i);
                }
            }
        }
    }

    /// §3.4 订阅 PaneDirty / PaneRemoved 通知。
    /// 返回的 sender 由连接层持有, drop 时关闭 channel,
    /// broadcast_pane_dirty 下次调用会检测到并清理。
    pub fn add_subscriber(&self, tx: mpsc::UnboundedSender<MuxNotification>) {
        self.subscribers.write().push(tx);
    }

    /// §3.3 从 alacritty Term 收集 dirty 行号 (viewport 坐标)。
    ///
    /// 用 term.damage() + reset_damage() 标准模式。返回 dirty 行号列表。
    fn collect_dirty_rows(&self) -> Vec<usize> {
        let mut term = self.term.lock();
        let mut rows = Vec::new();
        match term.damage() {
            TermDamage::Full => {
                // 整屏 dirty — 所有行
                let n = term.screen_lines();
                rows.extend(0..n);
            }
            TermDamage::Partial(iter) => {
                for line in iter {
                    rows.push(line.line);
                }
            }
        }
        term.reset_damage();
        rows
    }

    /// §3.3 处理 alacritty 通过 EventListener push 的事件。
    fn handle_pending_events(&self) {
        let events: Vec<AlacEvent> = self.events.lock().drain(..).collect();
        for event in events {
            match event {
                AlacEvent::Title(title) => {
                    *self.title.write() = title;
                }
                AlacEvent::ResetTitle => {
                    *self.title.write() = String::new();
                }
                AlacEvent::Bell => {
                    // TODO: fan-out Bell notification to clients
                }
                AlacEvent::PtyWrite(text) => {
                    // §3.1 alacritty 请求写 PTY (e.g. color query response)
                    let _ = self.pty_writer.lock().write_all(text.as_bytes());
                }
                AlacEvent::ClipboardStore(_, _) | AlacEvent::ClipboardLoad(_, _) => {
                    // §16.6 clipboard 通过 OSC 52 由 connection 层处理
                }
                AlacEvent::Exit | AlacEvent::ChildExit(_) => {
                    self.set_alive(false);
                }
                _ => {}
            }
        }
    }

    pub fn get_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// §3.10 SendInput — 向 PTY 写入原始字节。
    pub fn write_input(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.pty_writer.lock();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// §3.10 Paste — 向 PTY 写入文本 (可选 bracketed paste markers)。
    pub fn paste(&self, text: &str) -> anyhow::Result<()> {
        if self.is_bracketed_paste_active() {
            let bracketed = format!("\x1b[2004{}\x1b[2014", text);
            self.write_input(bracketed.as_bytes())
        } else {
            self.write_input(text.as_bytes())
        }
    }

    /// §3.3 fetch_grid_update — 由 connection 层调用响应 RPC。
    pub fn fetch_grid_update(&self, since_generation: u64) -> grid_sync::GridUpdate {
        self.grid_diff_ring
            .read()
            .fetch_update(since_generation, self)
    }

    /// §3.3 get_full_snapshot — 当前 grid 完整快照。
    pub fn get_full_snapshot(&self) -> FullGridSnapshot {
        let term = self.term.lock();
        snapshot_from_term(&*term)
    }

    /// §3.10 Resize — 改 PTY winsize + resize alacritty Term + bump generation。
    pub fn resize(&self, cols: u32, rows: u32) {
        // §3.1 通知 PTY TIOCSWINSZ
        let _ = self.pty_master.lock().resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });

        // §3.1 resize alacritty Term
        {
            let mut term = self.term.lock();
            let size = TermSize::new(cols as usize, rows as usize);
            term.resize(size);
        }

        self.cols.store(cols as u64, Ordering::SeqCst);
        self.rows.store(rows as u64, Ordering::SeqCst);
        // resize 影响整屏 — 用一个标记所有行 dirty 的 diff + bump generation
        let all_rows: Vec<usize> = {
            let term = self.term.lock();
            (0..term.screen_lines()).collect()
        };
        let diff = {
            let term = self.term.lock();
            diff_from_dirty(&*term, &all_rows)
        };
        let new_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.grid_diff_ring.write().push(new_gen, diff);
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

    pub fn set_title(&self, title: String) {
        *self.title.write() = title;
    }

    pub fn get_title(&self) -> String {
        self.title.read().clone()
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
        let buf = self.scrollback_buffer.read();
        let version = self.scrollback_version.read();
        let lines = buf.fetch_lines(from_line, count, direction);
        let total = buf.total_lines();
        let sv = version.encode();
        (lines, total, sv)
    }

    pub fn search_scrollback(
        &self,
        regex: &str,
        from_line: u32,
        direction: u32,
        max_results: u32,
    ) -> (Vec<(u32, grid_sync::RowChange)>, u64) {
        let buf = self.scrollback_buffer.read();
        let version = self.scrollback_version.read();
        let matches = buf.search(regex, from_line, direction, max_results);
        let sv = version.encode();
        (matches, sv)
    }

    pub fn get_scrollback_version(&self) -> u64 {
        self.scrollback_version.read().encode()
    }

    pub fn push_scrollback_row(&self, row: grid_sync::RowChange) {
        let mut buf = self.scrollback_buffer.write();
        buf.push_row(row);
        if buf.is_full() {
            self.scrollback_version.write().bump();
        }
    }

    /// §3.3 设置 pane zoom 状态。
    pub fn set_zoomed(&self, zoomed: bool) {
        self.zoomed.store(zoomed, Ordering::SeqCst);
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

    /// §3.3 广播 ShellIntegrationChanged 到所有订阅者。
    fn broadcast_shell_integration_changed(&self) {
        let notif = MuxNotification {
            event: Some(mux_protocol::notification::Event::ShellIntegrationChanged(
                mux_protocol::ShellIntegrationChanged {
                    cwd: self.get_cwd(),
                },
            )),
        };
        let subs = self.subscribers.read().clone();
        let mut dead = Vec::new();
        for (i, tx) in subs.iter().enumerate() {
            if tx.send(notif.clone()).is_err() {
                dead.push(i);
            }
        }
        if !dead.is_empty() {
            let mut live = self.subscribers.write();
            for i in dead.into_iter().rev() {
                if i < live.len() {
                    live.remove(i);
                }
            }
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // §3.5 pane drop: 标记 dead + 尝试 kill child (避免僵尸进程)
        self.alive.store(false, Ordering::SeqCst);
        if let Some(child) = self.child.lock().take() {
            let mut killer = child.clone_killer();
            let _ = killer.kill();
        }
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
