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
    InteractiveElement, KeyDownEvent, Keystroke, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
};
use mux::MuxDomain;
use mux_protocol::{
    fetch_grid_update_response::Update as FetchUpdate, notification::Event as NotifEvent,
    FullGridSnapshot, GridDiff, Notification, RowChange,
};
use mux_protocol::input::{
    handle_key_event, is_full_screen_active, KeyDispatchContext, KeyDispatchResult, PaneModes,
    PrefixAction, PrefixModeConfig, PrefixModeMachine,
};
use project::Project;
use settings::Settings;
use terminal::{
    Modes, Terminal, TerminalBounds, TerminalBuilder, terminal_settings::TerminalSettings,
};
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
    /// §3.1/§16.6 an input transport failed (server unreachable, permission
    /// denied, etc.). Surfaces the error text so the workspace can show a
    /// toast instead of silently dropping the keystroke/mouse event.
    InputFailed { message: SharedString },
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
    /// §16.5 / §16.7 Shared prefix-mode state machine (live input router).
    prefix_machine: PrefixModeMachine,
    prefix_timeout_task: Option<gpui::Task<()>>,
    /// §3.1 mouse-input transport errors buffered from the input sink (which
    /// has no GPUI context) and drained into InputFailed events at render.
    pending_input_errors: std::sync::Arc<std::sync::Mutex<Vec<SharedString>>>,
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
        // §3.1 shared with the mouse input sink (which has no GPUI context):
        // transport errors land here and are drained into InputFailed events
        // at render time.
        let pending_input_errors =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<SharedString>::new()));

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

        // §16.6 Mouse reports from DisplayOnly TerminalElement must reach
        // the server-owned PTY. Keyboard already goes through send_bytes_to_pty;
        // this sink covers mouse_mode write_to_pty paths. Transport errors are
        // buffered into `pending_input_errors` and drained at render.
        {
            let domain = domain.clone();
            let pane_id = pane_id.clone();
            let executor = cx.background_executor().clone();
            let errors = pending_input_errors.clone();
            let sink: std::sync::Arc<dyn Fn(Vec<u8>) + Send + Sync> =
                std::sync::Arc::new(move |bytes: Vec<u8>| {
                    if bytes.is_empty() {
                        return;
                    }
                    let domain = domain.clone();
                    let pane_id = pane_id.clone();
                    let errors = errors.clone();
                    executor
                        .spawn(async move {
                            if let Err(error) = domain.send_input(&pane_id, &bytes).await {
                                tracing::error!(
                                    pane_id = %pane_id,
                                    error = %error,
                                    "mouse send_input failed"
                                );
                                if let Ok(mut buf) = errors.lock() {
                                    buf.push(SharedString::from(format!("{error}")));
                                }
                            }
                        })
                        .detach();
                });
            terminal.update(cx, |terminal, _cx| {
                terminal.set_input_sink(Some(sink));
            });
        }

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
            display_offset: 0,
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
            prefix_machine: PrefixModeMachine::new(PrefixModeConfig::default()),
            prefix_timeout_task: None,
            pending_input_errors,
        };
        view.start_notification_listener(cx);
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

        // Coalescing loop: accumulate small PaneOutput/PaneDirty signals, then
        // flush after an 8ms quiet window (or immediately above 64KiB). Unlike
        // the prior Task-latch design, this loop always re-arms after each flush
        // because there is no cross-scope Option that stays Some forever.
        let task = cx.spawn(async move |_, cx| {
            let mut pending_output: Vec<u8> = Vec::new();
            let mut pending_dirty = false;

            loop {
                // Block for the next notification when idle; otherwise drain
                // whatever is already queued without waiting.
                let notif = if pending_output.is_empty() && !pending_dirty {
                    match rx.recv().await {
                        Ok(n) => n,
                        Err(_) => break,
                    }
                } else {
                    match rx.try_recv() {
                        Ok(n) => n,
                        Err(err) if err.to_string().contains("empty") || format!("{err:?}").contains("Empty") => {
                            // Quiet window: batch for ~half a frame, then flush.
                            if pending_output.len() <= 65536 {
                                cx.background_executor()
                                    .timer(std::time::Duration::from_millis(8))
                                    .await;
                                // Drain anything that arrived during the wait.
                                while let Ok(n) = rx.try_recv() {
                                    if !Self::accumulate_notification(
                                        &pane_id,
                                        n,
                                        &mut pending_output,
                                        &mut pending_dirty,
                                        &weak,
                                        cx,
                                    ) {
                                        return;
                                    }
                                }
                            }
                            Self::flush_pending(&weak, &mut pending_output, &mut pending_dirty, cx)
                                .await;
                            continue;
                        }
                        Err(_) => break,
                    }
                };

                if !Self::accumulate_notification(
                    &pane_id,
                    notif,
                    &mut pending_output,
                    &mut pending_dirty,
                    &weak,
                    cx,
                ) {
                    break;
                }

                if pending_output.len() > 65536 {
                    Self::flush_pending(&weak, &mut pending_output, &mut pending_dirty, cx).await;
                }
            }
        });
        self.notification_task = Some(task);
    }

    /// Returns false when the pane was removed and the listener should exit.
    fn accumulate_notification(
        pane_id: &str,
        notif: mux_protocol::Notification,
        pending_output: &mut Vec<u8>,
        pending_dirty: &mut bool,
        weak: &WeakEntity<Self>,
        cx: &mut AsyncApp,
    ) -> bool {
        let Some(event) = notif.event else {
            return true;
        };
        match event {
            NotifEvent::PaneOutput(chunk) if chunk.pane_id == pane_id => {
                pending_output.extend_from_slice(&chunk.data);
                true
            }
            NotifEvent::PaneDirty(dirty) if dirty.pane_id == pane_id => {
                *pending_dirty = true;
                true
            }
            NotifEvent::PaneRemoved(removed) if removed.pane_id == pane_id => {
                let _ = weak.update(cx, |view, cx| {
                    view.notification_task = None;
                    cx.emit(MuxPaneEvent::CloseRequested);
                });
                false
            }
            NotifEvent::PaneTitleChanged(changed) if changed.pane_id == pane_id => {
                let _ = weak.update(cx, |view, cx| {
                    // §3.4 Set terminal title via OSC 2 escape sequence.
                    view.terminal.update(cx, |t, cx| {
                        t.write_output(
                            format!("\x1b]2;{}\x07", changed.title).as_bytes(),
                            cx,
                        );
                    });
                    cx.emit(MuxPaneEvent::TitleChanged);
                });
                true
            }
            NotifEvent::PaneBell(bell) if bell.pane_id == pane_id => {
                // §15.13 Bell notification — treat like dirty to trigger re-render.
                *pending_dirty = true;
                true
            }
            _ => true,
        }
    }

    async fn flush_pending(
        weak: &WeakEntity<Self>,
        pending_output: &mut Vec<u8>,
        pending_dirty: &mut bool,
        cx: &mut AsyncApp,
    ) {
        let data = std::mem::take(pending_output);
        let dirty = std::mem::take(pending_dirty);
        if !data.is_empty() {
            let _ = weak.update(cx, |view, cx| {
                view.terminal.update(cx, |terminal, cx| {
                    terminal.write_output(&data, cx);
                });
                cx.notify();
            });
        } else if dirty {
            let _ = weak.update(cx, |view, cx| {
                view.schedule_fetch(cx);
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
                // §16.6 / StubSweep3 #7: Write changed rows to the DisplayOnly
                // Terminal so the rendered view reflects the diff, not just the
                // cached snapshot. A full snapshot rewrite via
                // write_snapshot_to_terminal is the simplest/safest approach:
                // performing per-row ANSI rewrites for arbitrary GridDiff
                // contents (partial rows, cursor changes) would require careful
                // per-cell position tracking and risks visual artifacts. A full
                // rewrite always produces a correct display, and GridDiff
                // payloads are small (delta frames during reconnect recovery).
                self.write_snapshot_to_terminal(cx);
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
        // §15.12 Restore the server-authoritative scroll position after the
        // snapshot text/cursor are written. u32 → usize is lossless on every
        // target (usize is at least 32 bits); clamping happens in the Terminal.
        let display_offset = self.snapshot.display_offset as usize;
        self.terminal.update(cx, |terminal, cx| {
            terminal.write_output(&clear_and_write, cx);
            terminal.scroll_to_display_offset(display_offset);
        });
    }

    /// §3.10 / §16.7 keystroke → priority chain → MuxDomain::send_input.
    fn dispatch_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        let bytes = keystroke_to_bytes(keystroke);
        if bytes.is_empty() {
            return;
        }

        let mode = self.terminal.read(cx).last_content().mode;
        let pane_modes = PaneModes {
            alt_screen: mode.contains(Modes::ALT_SCREEN),
            bracketed_paste: mode.contains(Modes::BRACKETED_PASTE),
            mouse_tracking: mode.intersects(Modes::MOUSE_MODE),
            any_decset: mode.intersects(
                Modes::APP_CURSOR
                    | Modes::APP_KEYPAD
                    | Modes::FOCUS_IN_OUT
                    | Modes::ALTERNATE_SCROLL
                    | Modes::SGR_MOUSE
                    | Modes::UTF8_MOUSE,
            ),
        };
        self.prefix_machine
            .set_full_screen_passthrough(is_full_screen_active(&pane_modes));

        let ime_composing = self.terminal_view.read(cx).is_ime_composing();
        let copy_mode = self.terminal.read(cx).vi_mode_enabled();

        let mut ctx_dispatch = KeyDispatchContext {
            ime_composing,
            // Extension shortcuts are consumed by the GPUI keymap before raw key_down.
            extension_shortcut: None,
            prefix_mode_machine: self.prefix_machine.clone(),
            pane_modes,
            agent_cli_mode: false,
            copy_mode,
        };

        // Prefix key entry is owned by EnterPrefixMode action; raw path sees unbound keys.
        let result = handle_key_event(&bytes, false, false, &mut ctx_dispatch);
        self.prefix_machine = ctx_dispatch.prefix_mode_machine;

        match result {
            KeyDispatchResult::RouteToIme
            | KeyDispatchResult::ExecuteExtensionAction(_)
            | KeyDispatchResult::ExecutePrefixCommand
            | KeyDispatchResult::Passthrough
            | KeyDispatchResult::RouteToCopyMode => {}
            KeyDispatchResult::RouteToAgentCli => {
                self.send_bytes_to_pty(bytes, cx);
            }
            KeyDispatchResult::SendLiteral { bytes: send_bytes }
            | KeyDispatchResult::SendToPty { bytes: send_bytes } => {
                self.send_bytes_to_pty(send_bytes, cx);
            }
        }
    }

    fn send_bytes_to_pty(&self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.spawn(async move |this, cx| {
            if let Err(error) = domain.send_input(&pane_id, &bytes).await {
                tracing::error!(pane_id = %pane_id, error = %error, "send_input failed");
                let message = SharedString::from(format!("{error}"));
                let _ = this.update(cx, |_, view_cx| {
                    view_cx.emit(MuxPaneEvent::InputFailed { message });
                });
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

    /// §15.4 Seed zoom from an authoritative snapshot without re-issuing the
    /// zoom_pane RPC (server already owns the flag in PaneInfo.zoomed).
    pub fn set_zoomed_from_snapshot(&mut self, zoomed: bool, cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        cx.notify();
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    /// §16.5 Enter prefix mode via the shared PrefixModeMachine.
    pub fn enter_prefix_mode(&mut self, timeout_ms: u64, cx: &mut Context<Self>) {
        let mode = self.terminal.read(cx).last_content().mode;
        let fullscreen = is_full_screen_active(&PaneModes {
            alt_screen: mode.contains(Modes::ALT_SCREEN),
            bracketed_paste: mode.contains(Modes::BRACKETED_PASTE),
            mouse_tracking: mode.intersects(Modes::MOUSE_MODE),
            any_decset: false,
        });
        let config = PrefixModeConfig {
            timeout_ms: if timeout_ms == 0 { 500 } else { timeout_ms },
            full_screen_passthrough: fullscreen,
        };
        // Keep machine config; on_prefix_key uses full_screen_passthrough.
        self.prefix_machine = PrefixModeMachine::new(config);
        match self.prefix_machine.on_prefix_key() {
            PrefixAction::EnterPrefixMode => {
                cx.notify();
                let timeout = std::time::Duration::from_millis(
                    if timeout_ms == 0 { 500 } else { timeout_ms },
                );
                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(timeout).await;
                    let _ = this.update(cx, |view, cx| {
                        if view.prefix_machine.is_prefix_wait() {
                            view.prefix_machine.on_timeout();
                            view.prefix_timeout_task = None;
                            cx.notify();
                        }
                    });
                });
                self.prefix_timeout_task = Some(task);
            }
            PrefixAction::Passthrough => {
                // Fullscreen: chord key is not intercepted (caller may SendLiteral).
            }
            _ => {}
        }
    }

    /// §16.5 Send a literal keystroke to the PTY (double-tap escape).
    /// `keystroke` is a tmux-style name (`C-b`, `Enter`, …) from the keymap.
    pub fn send_literal(&mut self, keystroke: &str, cx: &mut Context<Self>) {
        let bytes = mux_protocol::parse_key(keystroke);
        if bytes.is_empty() {
            // Fall back to raw UTF-8 only for single printable characters.
            if keystroke.chars().count() == 1 {
                self.send_bytes_to_pty(keystroke.as_bytes().to_vec(), cx);
            } else {
                tracing::warn!(%keystroke, "send_literal: unparseable keystroke");
            }
        } else {
            self.send_bytes_to_pty(bytes, cx);
        }
        if self.prefix_machine.is_prefix_wait() {
            self.prefix_machine.on_timeout();
        }
        self.prefix_timeout_task = None;
        cx.notify();
    }

    fn is_prefix_mode(&self) -> bool {
        self.prefix_machine.is_prefix_wait()
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
        // §3.1 drain mouse-input transport errors buffered by the input sink
        // (which has no GPUI context) and surface them as InputFailed events.
        let drained: Vec<SharedString> = match self.pending_input_errors.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        };
        for message in drained {
            cx.emit(MuxPaneEvent::InputFailed { message });
        }
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

        let mut dispatch_context = gpui::KeyContext::new_with_defaults();
        dispatch_context.add("Terminal");
        if self.is_prefix_mode() {
            dispatch_context.add("PrefixMode");
        }

        // §16.4 a11y: the TerminalElement child exposes Role::Terminal + TextRun
        // synthetic children per visible line. The root div stays role-less to
        // avoid a nested duplicate Terminal role in the a11y tree.

        div()
            .size_full()
            .id("mux-pane-root")
            .track_focus(&self.focus_handle)
            .aria_label(self.terminal.read(cx).title(true))
            .key_context(dispatch_context)
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
            // §16.7 keyboard → shared input router → MuxDomain::send_input
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if this.is_prefix_mode() {
                    // Drop the timeout; machine stays in PrefixWait so handle_key_event
                    // can still resolve the chord. GPUI keymap may also match PrefixMode.
                    if let Some(task) = this.prefix_timeout_task.take() {
                        task.detach();
                    }
                    this.dispatch_keystroke(&event.keystroke, cx);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                let ime = this.terminal_view.read(cx).is_ime_composing();
                this.dispatch_keystroke(&event.keystroke, cx);
                if !ime {
                    cx.stop_propagation();
                }
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

    fn to_item_events(event: &MuxPaneEvent, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            MuxPaneEvent::CloseRequested => f(workspace::item::ItemEvent::CloseItem),
            MuxPaneEvent::TitleChanged => f(workspace::item::ItemEvent::UpdateTab),
            // §3.1 InputFailed is informational only — it does not change tab
            // state. Subscribers that want to surface it (toast/status) listen
            // for the MuxPaneEvent directly via cx.subscribe.
            MuxPaneEvent::InputFailed { .. } => {}
        }
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
            display_offset: 0,
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
            display_offset: 0,
        };
        assert_eq!(snapshot_to_text(&snapshot), "abc\nde ");
    }

    /// §3.1 the mouse input sink buffers transport errors into an
    /// `Arc<Mutex<Vec<SharedString>>>` shared with the view; render drains it.
    /// This tests the drain contract directly: pushed errors come out once and
    /// the buffer is empty afterward, so render never re-emits a stale error
    /// and never drops one (poisoned lock yields empty, not a panic).
    #[test]
    fn pending_input_errors_buffer_drains_once() {
        let buffer: std::sync::Arc<std::sync::Mutex<Vec<SharedString>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        buffer
            .lock()
            .unwrap()
            .push(SharedString::from("mux server error: permission denied"));
        buffer
            .lock()
            .unwrap()
            .push(SharedString::from("connection closed"));

        let drained: Vec<SharedString> = match buffer.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        };
        assert_eq!(drained.len(), 2);
        assert!(drained[0].as_ref().contains("permission denied"));
        assert!(drained[1].as_ref().contains("connection closed"));

        // Second drain is empty — render never re-emits.
        let again = match buffer.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        };
        assert!(again.is_empty(), "buffer must be empty after drain");
    }
}
