// §3.1 / §15.1 MuxPaneView — server-canonical terminal panel renderer.
//
// Architecture (§3.1 in-place render-path exception):
//   - DisplayOnly Terminal receives PTY bytes via write_output (primary render path)
//   - TerminalElement provides GPU-accelerated batched text rendering
//   - Keyboard input goes through MuxDomain::send_input (never local PTY)
//   - fetch_grid_update serves as recovery path on reconnect (§15.12)
//
// The client's alacritty instance is a pure renderer — it never owns a PTY.

use std::sync::Arc;
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, KeyDownEvent, Keystroke, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
};
use mux::MuxDomain;
use mux_protocol::{
    fetch_grid_update_response::Update as FetchUpdate, notification::Event as NotifEvent,
    FullGridSnapshot, GridDiff, Notification, RowChange,
};
use project::Project;
use settings::Settings;
use terminal::{Terminal, TerminalBounds, TerminalBuilder, terminal_settings::TerminalSettings};
use theme::ActiveTheme;
use util::paths::PathStyle;

use crate::terminal_element::TerminalElement;
use crate::{TerminalMode, TerminalView};

use workspace::{
    item::{Item, ItemBufferKind, TabTooltipContent},
    ItemHandle, ToolbarItemLocation, Workspace,
};

/// §3.3 View events (for workspace to subscribe)
#[derive(Clone, Debug)]
pub enum MuxPaneEvent {
    TitleChanged,
    CloseRequested,
}

/// §3.3 MuxPaneView — GPUI view for a mux_server pane.
/// Wraps a DisplayOnly Terminal + TerminalView for GPU-accelerated rendering.
pub struct MuxPaneView {
    /// §3.10 server-assigned pane id
    pub pane_id: String,
    /// §3.10 MuxDomain client (shared Arc)
    pub domain: Arc<MuxDomain>,
    /// §3.1 exception: DisplayOnly terminal that receives PTY bytes via write_output
    terminal: Entity<Terminal>,
    /// TerminalView entity for TerminalElement state access (scroll, IME, mode)
    terminal_view: Entity<TerminalView>,
    /// Weak reference to workspace for TerminalElement
    workspace: WeakEntity<Workspace>,
    /// GPUI focus handle — tracked by TerminalElement, receives keyboard events
    focus_handle: FocusHandle,
    /// §3.4 notification subscription task
    notification_task: Option<Task<()>>,
    /// §3.3 client's known latest generation (for fetch_grid_update recovery)
    generation: u64,
    /// §3.3 fetch dedup flag
    fetch_in_flight: bool,
    /// §3.3 current grid snapshot (recovery path for reconnect)
    snapshot: FullGridSnapshot,
    /// §15.7 zoom state
    zoomed: bool,
    /// §3.10 last resize dimensions sent to server (cols, rows)
    last_sent_size: (u32, u32),
}

impl MuxPaneView {
    /// §3.3 Create view with DisplayOnly Terminal + TerminalView.
    /// PaneOutputChunk bytes feed Terminal::write_output; keyboard goes to MuxDomain.
    pub fn new(
        pane_id: String,
        domain: Arc<MuxDomain>,
        workspace: WeakEntity<Workspace>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // §3.1 exception: create DisplayOnly terminal (no PTY ownership)
        let settings = TerminalSettings::get_global(cx);
        let cursor_shape = settings.cursor_shape;
        let alternate_scroll = settings.alternate_scroll;
        let background_executor = cx.background_executor().clone();
        let window_id = window.window_handle().window_id().as_u64();

        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only_with_bounds(
                cursor_shape,
                alternate_scroll,
                None, // default scroll history
                window_id,
                &background_executor,
                PathStyle::local(),
                // Initial bounds: 80×24 cells at standard monospace metrics.
                // TerminalElement resizes on first prepaint with real font metrics.
                TerminalBounds::new(
                    gpui::px(18.0),  // line_height
                    gpui::px(8.4),   // cell_width
                    gpui::Bounds {
                        origin: gpui::Point::default(),
                        size: gpui::Size {
                            width: gpui::px(8.4 * 80.0),
                            height: gpui::px(18.0 * 24.0),
                        },
                    },
                ),
            )
            .subscribe(cx)
        });

        // TerminalView provides state for TerminalElement (scroll, mode, IME)
        let terminal_view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                workspace.clone(),
                None,
                project,
                window,
                cx,
            )
        });

        let snapshot = FullGridSnapshot {
            cols: 80,
            rows: 24,
            cells: vec![mux_protocol::Cell::default(); 80 * 24],
            cursor: Some(mux_protocol::CursorState {
                col: 0,
                row: 0,
                style: 1,
                visible: true,
            }),
            alternate_screen: false,
        };

        let mut view = Self {
            pane_id: pane_id.clone(),
            domain: domain.clone(),
            terminal,
            terminal_view,
            workspace,
            focus_handle,
            notification_task: None,
            generation: 0,
            fetch_in_flight: false,
            snapshot,
            zoomed: false,
            last_sent_size: (80, 24),
        };
        view.start_notification_listener(cx);
        view.subscribe_pane_output(cx);
        view.schedule_fetch(cx);
        view
    }

    /// §3.4 Listen for PaneOutput (byte stream), PaneDirty, PaneRemoved notifications.
    /// §3.1 exception: PaneOutput bytes are fed directly to the DisplayOnly terminal.
    /// §3.3 adaptive coalescing: batch PaneOutput data and flush once per frame
    /// to avoid excessive entity updates and repaints under high throughput.
    /// §3.4 Listen for PaneOutput (byte stream), PaneDirty, PaneRemoved notifications.
    /// §3.1 exception: PaneOutput bytes are fed directly to the DisplayOnly terminal.
    /// After each batch flush, cx.notify() triggers MuxPaneView repaint so the
    /// TerminalElement reads fresh terminal data on the next frame.
    /// §3.3 debounce: PaneDirty → schedule_fetch is throttled to once per 16ms
    /// (60fps). PaneOutput is the primary render path; PaneDirty only covers
    /// non-bytes changes (cursor, title, alt-screen) that fetch_grid_update provides.
    fn start_notification_listener(&mut self, cx: &mut Context<Self>) {
        let pane_id = self.pane_id.clone();
        let rx = self.domain.subscribe();
        let weak = cx.entity().downgrade();

        let task = cx.spawn(async move |_, cx| {
            let mut pending_output: Vec<u8> = Vec::new();
            let mut pending_dirty = false;
            let mut flush_handle: Option<Task<()>> = None;

            while let Ok(notif) = rx.recv().await {
                let Some(event) = notif.event else { continue };
                match event {
                    // §3.1 exception: primary render path — accumulate bytes, flush per frame
                    NotifEvent::PaneOutput(chunk) if chunk.pane_id == pane_id => {
                        pending_output.extend_from_slice(&chunk.data);
                        // Flush immediately if buffer is large (>64KB)
                        if pending_output.len() > 65536 {
                            Self::do_write_output(&weak, &mut pending_output, &mut flush_handle, cx).await;
                        } else if flush_handle.is_none() {
                            Self::schedule_flush(&weak, &mut pending_output, &mut flush_handle, &mut pending_dirty, cx);
                        }
                    }
                    // §3.3 grid-diff path: debounce to once per frame
                    NotifEvent::PaneDirty(dirty) if dirty.pane_id == pane_id => {
                        pending_dirty = true;
                        if flush_handle.is_none() {
                            Self::schedule_flush(&weak, &mut pending_output, &mut flush_handle, &mut pending_dirty, cx);
                        }
                    }
                    NotifEvent::PaneRemoved(removed) if removed.pane_id == pane_id => {
                        let _ = weak.update(cx, |view, cx| {
                            view.notification_task = None;
                            cx.emit(MuxPaneEvent::CloseRequested);
                        });
                        break;
                    }
                    _ => {}
                }
            }
        });
        self.notification_task = Some(task);
    }

    /// §3.3 Deferred flush: wait ~8ms (half-frame) to batch data, then flush.
    /// Called only when flush_handle is None (at most one pending at a time).
    fn schedule_flush(
        weak: &WeakEntity<Self>,
        pending_output: &mut Vec<u8>,
        flush_handle: &mut Option<Task<()>>,
        pending_dirty: &mut bool,
        cx: &mut AsyncApp,
    ) {
        // Take the data into a local variable before spawning so we have it
        // at closure capture time without borrowing pending_output.
        let has_output = !pending_output.is_empty();
        let has_dirty = *pending_dirty;
        let data = std::mem::take(pending_output);
        *pending_dirty = false;
        let weak2 = weak.clone();

        *flush_handle = Some(cx.spawn(async move |cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(8)).await;
            if !data.is_empty() {
                let _ = weak2.update(cx, |view, cx| {
                    view.terminal.update(cx, |terminal, cx| {
                        terminal.write_output(&data, cx);
                    });
                    // Trigger repaint on MuxPaneView so TerminalElement reads
                    // fresh terminal data next frame.
                    cx.notify();
                });
            } else if has_dirty {
                // No output bytes but dirty flag set — nothing visible changed,
                // schedule a fetch for the next frame anyway (catches cursor/style changes).
                let _ = weak2.update(cx, |view, cx| {
                    view.schedule_fetch(cx);
                });
            }
        }));
    }

    /// §3.3 Synchronous flush: write output and trigger repaint immediately.
    async fn do_write_output(
        weak: &WeakEntity<Self>,
        pending_output: &mut Vec<u8>,
        flush_handle: &mut Option<Task<()>>,
        cx: &mut AsyncApp,
    ) {
        if let Some(handle) = flush_handle.take() {
            handle.detach();
        }
        let data = std::mem::take(pending_output);
        if !data.is_empty() {
            let _ = weak.update(cx, |view, cx| {
                view.terminal.update(cx, |terminal, cx| {
                    terminal.write_output(&data, cx);
                });
                cx.notify();
            });
        }
    }

    /// §3.1 exception: subscribe to PTY byte stream from server.
    fn subscribe_pane_output(&self, cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = domain.subscribe_pane_output(&pane_id).await {
                    tracing::error!(pane_id = %pane_id, error = %e, "subscribe_pane_output failed");
                }
            })
            .detach();
    }

    /// §3.3 Schedule a fetch_grid_update (recovery path for reconnect §15.12).
    fn schedule_fetch(&mut self, cx: &mut Context<Self>) {
        if self.fetch_in_flight {
            return;
        }
        self.fetch_in_flight = true;

        let pane_id = self.pane_id.clone();
        let domain = self.domain.clone();
        let since = self.generation;
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let result = domain.fetch_grid_update(&pane_id, since).await;
            match weak.update(cx, |view, cx| {
                view.fetch_in_flight = false;
                match result {
                    Ok(resp) => {
                        view.apply_fetch_update(resp, cx);
                    }
                    Err(e) => {
                        tracing::error!(pane_id = %pane_id, error = %e, "fetch_grid_update failed");
                    }
                }
            }) {
                Ok(()) => {}
                Err(_) => tracing::warn!("MuxPaneView dropped after fetch"),
            }
        })
        .detach();
    }

    /// §3.3 / §15.12 Apply fetch response. On FullSnapshot, write content to DisplayOnly terminal.
    fn apply_fetch_update(
        &mut self,
        resp: mux_protocol::FetchGridUpdateResponse,
        cx: &mut Context<Self>,
    ) {
        let prev_generation = self.generation;
        self.generation = resp.to_generation;
        match resp.update {
            Some(FetchUpdate::FullSnapshot(full)) => {
                self.snapshot = full;
                // §15.12 reconnect recovery: only write to terminal if generation went
                // backwards (indicates reconnect) or this is the initial fetch (gen 0).
                // During normal operation, PaneOutput byte stream is the render path —
                // writing snapshot here would cause double-rendered characters.
                if prev_generation == 0 || resp.to_generation < prev_generation {
                    self.write_snapshot_to_terminal(cx);
                }
            }
            Some(FetchUpdate::Diff(diff)) => {
                apply_diff_to_snapshot(&mut self.snapshot, &diff);
            }
            None => {}
        }
        cx.notify();
    }

    /// §15.12 Write current snapshot content to DisplayOnly terminal for recovery.
    fn write_snapshot_to_terminal(&mut self, cx: &mut Context<Self>) {
        let text = snapshot_to_text(&self.snapshot);
        let bytes = text.into_bytes();
        // Clear screen + write snapshot + position cursor (ANSI: ESC[2J ESC[H = clear + home)
        let mut clear_and_write = Vec::with_capacity(bytes.len() + 32);
        clear_and_write.extend_from_slice(b"\x1b[2J\x1b[H");
        clear_and_write.extend_from_slice(&bytes);
        // §3.3 Position cursor at snapshot's recorded cursor location.
        // ANSI CSI row;col H is 1-based.
        if let Some(cursor) = &self.snapshot.cursor {
            let row = cursor.row + 1; // 1-based
            let col = cursor.col + 1; // 1-based
            let cursor_pos = format!("\x1b[{};{}H", row, col);
            clear_and_write.extend_from_slice(cursor_pos.as_bytes());
        }
        self.terminal.update(cx, |terminal, cx| {
            terminal.write_output(&clear_and_write, cx);
        });
    }

    /// §3.10 keystroke → terminal bytes → send_input via MuxDomain.
    fn dispatch_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        let bytes = keystroke_to_bytes(keystroke);
        if bytes.is_empty() {
            return;
        }
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = domain.send_input(&pane_id, &bytes).await {
                    tracing::error!(pane_id = %pane_id, error = %e, "send_input failed");
                }
            })
            .detach();
    }

    /// §3.3 Current terminal title (for tabbar). Uses terminal's parsed title from escape sequences.
    pub fn title(&self, cx: &App) -> SharedString {
        self.terminal.read(cx).title(true).into()
    }

    /// §3.10 resize — notify server of new dimensions.
    pub fn resize(&mut self, cols: u32, rows: u32, cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = domain.resize_pane(&pane_id, cols, rows).await {
                    tracing::error!(error = %e, "resize_pane failed");
                }
            })
            .detach();
    }

    /// §15.7 Whether this pane is currently zoomed.
    pub fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    /// §15.7 Set zoom state and notify server.
    pub fn set_zoomed(&mut self, zoomed: bool, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = domain.zoom_pane(&pane_id, zoomed).await {
                    tracing::error!(error = %e, "zoom_pane failed");
                }
            })
            .detach();
    }

    /// Access the underlying terminal entity (for tests/inspection).
    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }
}

/// §3.1 keystroke → terminal byte sequence (xterm standard).
/// Handles Ctrl-letter, Alt (ESC prefix), arrow keys, function keys.
pub fn keystroke_to_bytes(keystroke: &Keystroke) -> Vec<u8> {
    let ctrl = keystroke.modifiers.control;
    let alt = keystroke.modifiers.alt;
    let mut bytes = Vec::new();

    if let Some(key_char) = keystroke.key_char.as_ref() {
        let ch = key_char.chars().next().unwrap_or('\0');
        if ctrl && ch.is_ascii_alphabetic() {
            let base = if ch.is_ascii_uppercase() { b'A' } else { b'a' };
            let b = (ch as u8).wrapping_sub(base).wrapping_add(1);
            if alt {
                bytes.push(0x1B);
            }
            bytes.push(b);
            return bytes;
        }
        if ctrl {
            let ctrl_byte = match ch {
                '@' => Some(0x00),
                '[' => Some(0x1B),
                '\\' => Some(0x1C),
                ']' => Some(0x1D),
                '^' => Some(0x1E),
                '_' => Some(0x1F),
                ' ' => Some(0x00),
                _ => None,
            };
            if let Some(b) = ctrl_byte {
                if alt {
                    bytes.push(0x1B);
                }
                bytes.push(b);
                return bytes;
            }
        }
        if alt {
            bytes.push(0x1B);
        }
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        bytes.extend_from_slice(s.as_bytes());
        return bytes;
    }

    let esc_seq: &[u8] = match keystroke.key.as_str() {
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "enter" => b"\r",
        "escape" => b"\x1b",
        _ => &[],
    };
    if esc_seq.is_empty() {
        return bytes;
    }
    if alt {
        bytes.push(0x1B);
    }
    bytes.extend_from_slice(esc_seq);
    bytes
}

/// §3.3 把 GridDiff 应用到 FullGridSnapshot。
/// RowChange.cells 按位置替换整行 (index = column)。
/// spec §3.3 row-major flat array; 越界行/列静默丢弃。
pub fn apply_diff_to_snapshot(snapshot: &mut FullGridSnapshot, diff: &GridDiff) {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    for row_change in &diff.rows {
        let row = row_change.row as usize;
        if row >= rows {
            continue;
        }
        for (col, cell) in row_change.cells.iter().enumerate() {
            if col >= cols {
                break;
            }
            let flat = row * cols + col;
            if flat < snapshot.cells.len() {
                snapshot.cells[flat] = cell.clone();
            }
        }
    }
}

/// §3.3 把 FullGridSnapshot 渲染成纯文本。
/// 输出格式: 每行 cols 个字符, 行间以 \n 分隔。空 cell 用空格占位。
pub fn snapshot_to_text(snapshot: &FullGridSnapshot) -> String {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    let mut text = String::with_capacity(cols * rows + rows);
    for row in 0..rows {
        for col in 0..cols {
            let flat = row * cols + col;
            let ch = snapshot
                .cells
                .get(flat)
                .and_then(|c| c.char.chars().next())
                .unwrap_or(' ');
            text.push(ch);
        }
        if row < rows - 1 {
            text.push('\n');
        }
    }
    text
}

impl Focusable for MuxPaneView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<MuxPaneEvent> for MuxPaneView {}

impl Render for MuxPaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        // §3.10 resize forwarding: detect terminal dimension changes and notify server.
        // TerminalElement resizes the DisplayOnly terminal during prepaint; we check
        // the resulting grid size and forward to mux_server so the PTY matches.
        let bounds = self.terminal.read(cx).last_content().terminal_bounds;
        let cols = bounds.num_columns() as u32;
        let rows = bounds.num_lines() as u32;
        if cols > 0 && rows > 0 && (cols, rows) != self.last_sent_size {
            self.last_sent_size = (cols, rows);
            self.resize(cols, rows, cx);
        }

        let colors = cx.theme().colors();
        let focused = self.focus_handle.is_focused(window);
        let terminal_handle = self.terminal.clone();
        let terminal_view_handle = self.terminal_view.clone();

        div()
            .size_full()
            .id("mux-pane-root")
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .child(
                div()
                    .size_full()
                    .id("mux-terminal-container")
                    .bg(colors.editor_background)
                    .child(TerminalElement::new(
                        terminal_handle,
                        terminal_view_handle,
                        self.workspace.clone(),
                        self.focus_handle.clone(),
                        focused,
                        true, // cursor_visible
                        None, // block_below_cursor
                        TerminalMode::Standalone,
                    )),
            )
            // §3.1 keyboard input → MuxDomain::send_input (DisplayOnly terminal drops input)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.dispatch_keystroke(&event.keystroke, cx);
                cx.stop_propagation();
            }))
    }
}

impl Item for MuxPaneView {
    type Event = MuxPaneEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.terminal.read(cx).title(true).into()
    }

    fn suggested_filename(&self, cx: &App) -> SharedString {
        self.terminal.read(cx).title(true).into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.terminal.read(cx).title(true).into())
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        self.tab_tooltip_text(cx).map(TabTooltipContent::Text)
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::None
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(None)
    }

    fn is_dirty(&self, _cx: &App) -> bool {
        false
    }

    fn breadcrumb_location(&self, _cx: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::{Cell, CellStyle};

    #[test]
    fn test_keystroke_to_bytes_ctrl_c() {
        let keystroke = Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            key: "c".to_string(),
            key_char: Some("c".to_string()),
        };
        assert_eq!(keystroke_to_bytes(&keystroke), vec![0x03]);
    }

    #[test]
    fn test_keystroke_to_bytes_enter() {
        let keystroke = Keystroke {
            modifiers: Default::default(),
            key: "enter".to_string(),
            key_char: None,
        };
        assert_eq!(keystroke_to_bytes(&keystroke), vec![b'\r']);
    }

    #[test]
    fn test_keystroke_to_bytes_arrow_up() {
        let keystroke = Keystroke {
            modifiers: Default::default(),
            key: "up".to_string(),
            key_char: None,
        };
        assert_eq!(keystroke_to_bytes(&keystroke), b"\x1b[A".to_vec());
    }

    #[test]
    fn test_keystroke_to_bytes_alt_a() {
        let keystroke = Keystroke {
            modifiers: gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
            key: "a".to_string(),
            key_char: Some("a".to_string()),
        };
        assert_eq!(keystroke_to_bytes(&keystroke), vec![0x1B, b'a']);
    }

    #[test]
    fn test_apply_diff_to_snapshot() {
        let mut snapshot = FullGridSnapshot {
            cols: 3,
            rows: 2,
            cells: vec![Cell::default(); 6],
            cursor: None,
            alternate_screen: false,
        };
        let diff = GridDiff {
            rows: vec![RowChange {
                row: 0,
                cells: vec![
                    Cell { char: "a".to_string(), ..Default::default() },
                    Cell { char: "X".to_string(), ..Default::default() },
                    Cell { char: "c".to_string(), ..Default::default() },
                ],
            }],
        };
        apply_diff_to_snapshot(&mut snapshot, &diff);
        assert_eq!(snapshot.cells[0].char, "a");
        assert_eq!(snapshot.cells[1].char, "X");
        assert_eq!(snapshot.cells[2].char, "c");
        // Out-of-bounds row is silently ignored
        let diff_oob = GridDiff {
            rows: vec![RowChange {
                row: 99,
                cells: vec![Cell { char: "Z".to_string(), ..Default::default() }],
            }],
        };
        apply_diff_to_snapshot(&mut snapshot, &diff_oob);
        assert_eq!(snapshot.cells.len(), 6);
    }

    #[test]
    fn test_snapshot_to_text() {
        let snapshot = FullGridSnapshot {
            cols: 3,
            rows: 2,
            cells: vec![
                Cell { char: "a".to_string(), ..Default::default() },
                Cell { char: "b".to_string(), ..Default::default() },
                Cell { char: "c".to_string(), ..Default::default() },
                Cell { char: "d".to_string(), ..Default::default() },
                Cell { char: "e".to_string(), ..Default::default() },
                Cell { char: " ".to_string(), ..Default::default() },
            ],
            cursor: None,
            alternate_screen: false,
        };
        assert_eq!(snapshot_to_text(&snapshot), "abc\nde ");
    }
}
