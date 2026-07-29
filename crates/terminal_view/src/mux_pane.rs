// §3.1 / §15.1 MuxPaneView — server-canonical terminal panel renderer.
//
// Architecture (§3.1 in-place render-path exception):
//   - DisplayOnly Terminal receives PTY bytes via write_output (primary render path)
//   - TerminalElement provides GPU-accelerated batched text rendering
//   - Keyboard input goes through MuxDomain::send_input (never local PTY)
//   - fetch_grid_update serves as recovery path on reconnect (§15.12)
//
// The client's alacritty instance is a pure renderer — it never owns a PTY.

use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, KeyDownEvent, Keystroke, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
};
use mux::MuxDomain;
use mux_protocol::input::{
    KeyDispatchContext, KeyDispatchResult, PaneModes, PrefixAction, PrefixModeConfig,
    PrefixModeMachine, handle_key_event, is_full_screen_active,
};
use mux_protocol::{
    FullGridSnapshot, GridDiff, fetch_grid_update_response::Update as FetchUpdate,
    notification::Event as NotifEvent,
};
use project::Project;
use settings::Settings;
use std::sync::Arc;
use terminal::{
    CursorShape as TerminalCursorShape, Hyperlink as TerminalHyperlink, Modes, Rgb,
    StructuredTerminalCell, StructuredTerminalCursor, StructuredTerminalSnapshot,
    StructuredUnderlineStyle, Terminal, TerminalBounds, TerminalBuilder,
    terminal_settings::TerminalSettings,
};
use theme::ActiveTheme;
use util::paths::PathStyle;

use crate::terminal_element::TerminalElement;
use crate::{TerminalMode, TerminalView};

use workspace::{
    Workspace,
    item::{Item, ItemBufferKind, TabTooltipContent},
};

/// §3.3 View events (for workspace to subscribe)
#[derive(Clone, Debug)]
pub enum MuxPaneEvent {
    TitleChanged,
    CloseRequested,
    /// §3.1/§16.6 an input transport failed (server unreachable, permission
    /// denied, etc.). Surfaces the error text so the workspace can show a
    /// toast instead of silently dropping the keystroke/mouse event.
    InputFailed {
        message: SharedString,
    },
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
    /// A dirty signal arrived while a fetch was in flight. Completion must
    /// immediately pull again so a newer server generation cannot be stranded.
    fetch_pending: bool,
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
                    gpui::px(18.0), // line_height
                    gpui::px(8.4),  // cell_width
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
                blinking: false,
            }),
            alternate_screen: false,
            display_offset: 0,
            modes: None,
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
            fetch_pending: false,
            snapshot,
            zoomed: false,
            last_sent_size: (80, 24),
            prefix_machine: PrefixModeMachine::new(PrefixModeConfig::default()),
            prefix_timeout_task: None,
            pending_input_errors,
        };
        view.start_notification_listener(cx);
        // Subscribe before the initial fetch so output produced while the request is
        // in flight cannot fall into a fetch-before-subscribe race. A quiet pane
        // emits no future notification, so construction itself must fetch generation 0.
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

        // §3.1 server-canonical render path: PaneOutput and PaneDirty are both
        // dirty signals that trigger fetch_grid_update. PTY bytes are never
        // fed to the client terminal — the server owns the sole emulator.
        let task = cx.spawn(async move |_, cx| {
            let mut pending_dirty = false;

            loop {
                let notif = if !pending_dirty {
                    match rx.recv().await {
                        Ok(n) => n,
                        Err(_) => break,
                    }
                } else {
                    match rx.try_recv() {
                        Ok(n) => n,
                        Err(err)
                            if err.to_string().contains("empty")
                                || format!("{err:?}").contains("Empty") =>
                        {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(8))
                                .await;
                            while let Ok(n) = rx.try_recv() {
                                if !Self::accumulate_notification(
                                    &pane_id,
                                    n,
                                    &mut pending_dirty,
                                    &weak,
                                    cx,
                                ) {
                                    return;
                                }
                            }
                            Self::flush_pending(&weak, &mut pending_dirty, cx).await;
                            continue;
                        }
                        Err(_) => break,
                    }
                };

                if !Self::accumulate_notification(&pane_id, notif, &mut pending_dirty, &weak, cx) {
                    break;
                }
            }
        });
        self.notification_task = Some(task);
    }

    /// Returns false when the pane was removed and the listener should exit.
    fn accumulate_notification(
        pane_id: &str,
        notif: mux_protocol::Notification,
        pending_dirty: &mut bool,
        weak: &WeakEntity<Self>,
        cx: &mut AsyncApp,
    ) -> bool {
        let Some(event) = notif.event else {
            return true;
        };
        match event {
            // §3.1: PaneOutput is a dirty signal only — bytes are never parsed.
            NotifEvent::PaneOutput(chunk) if chunk.pane_id == pane_id => {
                *pending_dirty = true;
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
                    view.terminal.update(cx, |t, cx| {
                        t.write_output(format!("\x1b]2;{}\x07", changed.title).as_bytes(), cx);
                    });
                    cx.emit(MuxPaneEvent::TitleChanged);
                });
                true
            }
            NotifEvent::PaneBell(bell) if bell.pane_id == pane_id => {
                *pending_dirty = true;
                true
            }
            _ => true,
        }
    }

    async fn flush_pending(weak: &WeakEntity<Self>, pending_dirty: &mut bool, cx: &mut AsyncApp) {
        let dirty = std::mem::take(pending_dirty);
        if dirty {
            let _ = weak.update(cx, |view, cx| {
                view.schedule_fetch(cx);
            });
        }
    }

    /// §3.3 Schedule a fetch_grid_update (recovery path for reconnect §15.12).
    fn schedule_fetch(&mut self, cx: &mut Context<Self>) {
        if self.fetch_in_flight {
            self.fetch_pending = true;
            return;
        }
        self.fetch_in_flight = true;
        self.fetch_pending = false;

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
                        if let Err(error) = view.apply_fetch_update(resp, cx) {
                            tracing::error!(pane_id = %pane_id, error = %error, "apply grid update failed");
                            cx.emit(MuxPaneEvent::InputFailed {
                                message: SharedString::from(format!(
                                    "failed to apply mux pane {pane_id} grid: {error}"
                                )),
                            });
                        }
                    }
                    Err(error) => {
                        tracing::error!(pane_id = %pane_id, error = %error, "fetch_grid_update failed");
                        cx.emit(MuxPaneEvent::InputFailed {
                            message: SharedString::from(format!(
                                "failed to fetch mux pane {pane_id} grid: {error}"
                            )),
                        });
                    }
                }
                if view.fetch_pending {
                    view.schedule_fetch(cx);
                }
            }) {
                Ok(()) => {}
                Err(_) => tracing::warn!("MuxPaneView dropped after fetch"),
            }
        })
        .detach();
    }

    /// §3.3 / §15.12 Apply a fetched update transactionally. The local
    /// generation advances only after the structured grid was validated and
    /// imported successfully, so a malformed response remains retryable.
    fn apply_fetch_update(
        &mut self,
        resp: mux_protocol::FetchGridUpdateResponse,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        validate_generation_envelope(self.generation, &resp)?;
        let mut candidate = self.snapshot.clone();
        match resp.update {
            Some(FetchUpdate::FullSnapshot(full)) => candidate = full,
            Some(FetchUpdate::Diff(diff)) => apply_diff_to_snapshot(&mut candidate, &diff)?,
            None => {
                self.generation = resp.to_generation;
                cx.notify();
                return Ok(());
            }
        }

        self.write_snapshot_to_terminal(&candidate, cx)?;
        self.snapshot = candidate;
        self.generation = resp.to_generation;
        cx.notify();
        Ok(())
    }

    /// Import the server-owned viewport directly into the display-only
    /// Alacritty grid. `display_offset` is not applied here: the current wire
    /// snapshot contains only the already-selected server viewport, not enough
    /// history to reconstruct a local scrollback buffer at that offset.
    fn write_snapshot_to_terminal(
        &mut self,
        snapshot: &FullGridSnapshot,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let structured = structured_terminal_snapshot(snapshot)?;
        self.terminal
            .update(cx, |terminal, cx| {
                terminal.apply_structured_snapshot(&structured, cx)
            })
            .map_err(|error| anyhow::anyhow!("structured terminal import failed: {error}"))
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
                let timeout = std::time::Duration::from_millis(if timeout_ms == 0 {
                    500
                } else {
                    timeout_ms
                });
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

fn validate_generation_envelope(
    current_generation: u64,
    response: &mux_protocol::FetchGridUpdateResponse,
) -> anyhow::Result<()> {
    if response.to_generation < response.from_generation {
        anyhow::bail!(
            "mux grid generation regressed within response: {} -> {}",
            response.from_generation,
            response.to_generation
        );
    }
    match &response.update {
        Some(FetchUpdate::FullSnapshot(_)) => Ok(()),
        Some(FetchUpdate::Diff(_)) if response.from_generation == current_generation => Ok(()),
        Some(FetchUpdate::Diff(_)) => anyhow::bail!(
            "mux grid diff starts at generation {}, client is at {}",
            response.from_generation,
            current_generation
        ),
        None if response.from_generation == current_generation
            && response.to_generation == current_generation =>
        {
            Ok(())
        }
        None => anyhow::bail!(
            "mux no-change response {} -> {} does not match client generation {}",
            response.from_generation,
            response.to_generation,
            current_generation
        ),
    }
}

/// Convert the wire snapshot into the terminal crate's transport-neutral DTO.
fn structured_terminal_snapshot(
    snapshot: &FullGridSnapshot,
) -> anyhow::Result<StructuredTerminalSnapshot> {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    let expected_cells = mux_protocol::checked_grid_cell_count(cols, rows)
        .map_err(|message| anyhow::anyhow!("invalid mux grid dimensions: {message}"))?;
    if snapshot.cells.len() != expected_cells {
        anyhow::bail!(
            "mux grid has {} cells, expected {} for {}x{}",
            snapshot.cells.len(),
            expected_cells,
            cols,
            rows
        );
    }

    let cells = snapshot
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let mut chars = cell.char.chars();
            let character = chars
                .next()
                .ok_or_else(|| anyhow::anyhow!("mux grid cell {index} has no character"))?;
            if chars.next().is_some() {
                anyhow::bail!("mux grid cell {index} contains more than one Unicode scalar");
            }
            let style = cell.style.as_ref().cloned().unwrap_or_default();
            let underline = match style.underline_style {
                2 => StructuredUnderlineStyle::Single,
                3 => StructuredUnderlineStyle::Double,
                4 => StructuredUnderlineStyle::Curly,
                5 => StructuredUnderlineStyle::Dotted,
                6 => StructuredUnderlineStyle::Dashed,
                _ if style.underline => StructuredUnderlineStyle::Single,
                _ => StructuredUnderlineStyle::None,
            };
            let hyperlink = cell.hyperlink.as_ref().and_then(|hyperlink| {
                (!hyperlink.uri.is_empty()).then(|| {
                    TerminalHyperlink::new(
                        (!hyperlink.id.is_empty()).then_some(hyperlink.id.as_str()),
                        hyperlink.uri.clone(),
                    )
                })
            });
            Ok(StructuredTerminalCell {
                character,
                zerowidth: cell.zerowidth.chars().collect(),
                foreground: rgb_from_u32(cell.foreground),
                background: rgb_from_u32(cell.background),
                bold: style.bold,
                italic: style.italic,
                underline,
                underline_color: style.underline_color.map(rgb_from_u32),
                strikethrough: style.strikethrough,
                dim: style.dim,
                reverse: style.reverse,
                wide_char: style.wide_char,
                wide_char_spacer: style.wide_char_spacer,
                leading_wide_char_spacer: style.leading_wide_char_spacer,
                wrapline: style.wrapline,
                hidden: style.hidden,
                hyperlink,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let cursor = snapshot
        .cursor
        .as_ref()
        .map(|cursor| {
            if cursor.row as usize >= rows || cursor.col as usize >= cols {
                anyhow::bail!(
                    "mux cursor ({}, {}) is outside {}x{} grid",
                    cursor.col,
                    cursor.row,
                    cols,
                    rows
                );
            }
            let shape = match cursor.style {
                0 | 1 => TerminalCursorShape::Block,
                2 => TerminalCursorShape::Bar,
                3 => TerminalCursorShape::Underline,
                4 => TerminalCursorShape::HollowBlock,
                5 => TerminalCursorShape::Hidden,
                _ => TerminalCursorShape::Block,
            };
            Ok(StructuredTerminalCursor {
                point: terminal::Point::new(cursor.row as i32, cursor.col as usize),
                shape,
                visible: cursor.visible,
                blinking: cursor.blinking,
            })
        })
        .transpose()?;

    let modes = snapshot
        .modes
        .map(Modes::from_bits_truncate)
        .unwrap_or_else(|| {
            if snapshot.alternate_screen {
                Modes::ALT_SCREEN
            } else {
                Modes::NONE
            }
        });
    Ok(StructuredTerminalSnapshot {
        cols,
        rows,
        cells,
        cursor,
        alternate_screen: snapshot.alternate_screen,
        modes,
    })
}

fn rgb_from_u32(color: u32) -> Rgb {
    Rgb {
        r: ((color >> 16) & 0xff) as u8,
        g: ((color >> 8) & 0xff) as u8,
        b: (color & 0xff) as u8,
    }
}

/// §3.3 Apply a row-complete GridDiff to the cached FullGridSnapshot.
/// Every row is validated before mutation so malformed wire data cannot advance
/// the client's generation or leave a partially updated cache.
pub fn apply_diff_to_snapshot(
    snapshot: &mut FullGridSnapshot,
    diff: &GridDiff,
) -> anyhow::Result<()> {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    let expected_cells = cols
        .checked_mul(rows)
        .ok_or_else(|| anyhow::anyhow!("cached mux grid dimensions overflow"))?;
    if snapshot.cells.len() != expected_cells {
        anyhow::bail!(
            "cached mux grid has {} cells, expected {expected_cells}",
            snapshot.cells.len()
        );
    }
    for row_change in &diff.rows {
        if row_change.row as usize >= rows {
            anyhow::bail!(
                "mux grid diff row {} is outside {rows} rows",
                row_change.row
            );
        }
        if row_change.cells.len() != cols {
            anyhow::bail!(
                "mux grid diff row {} has {} cells, expected {cols}",
                row_change.row,
                row_change.cells.len()
            );
        }
    }

    for row_change in &diff.rows {
        let row_start = row_change.row as usize * cols;
        snapshot.cells[row_start..row_start + cols].clone_from_slice(&row_change.cells);
    }
    Ok(())
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
    use gpui::TestAppContext;
    use mux_protocol::{
        Cell, CellStyle, Envelope, FetchGridUpdateResponse, Request, Response, RowChange,
        envelope::Payload as EnvelopePayload, request::Body as RequestBody,
        response::Body as ResponseBody,
    };
    use settings::SettingsStore;

    #[cfg(unix)]
    fn serve_initial_grid(mut stream: std::os::unix::net::UnixStream) -> Result<(), String> {
        use std::io::{Read, Write};

        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| format!("set mock mux read timeout: {error}"))?;

        let mut prefix = Vec::with_capacity(mux_protocol::MAX_VARINT_LEN);
        loop {
            let mut byte = [0u8; 1];
            stream
                .read_exact(&mut byte)
                .map_err(|error| format!("read initial grid request prefix: {error}"))?;
            prefix.push(byte[0]);
            if byte[0] & 0x80 == 0 {
                break;
            }
            if prefix.len() == mux_protocol::MAX_VARINT_LEN {
                return Err("initial grid request used an overlong frame prefix".to_string());
            }
        }

        let (raw_len, prefix_len) = mux_protocol::parse_len_prefix(&prefix)
            .map_err(|error| format!("parse initial grid request prefix: {error}"))?
            .ok_or_else(|| "initial grid request prefix was incomplete".to_string())?;
        let payload_len = mux_protocol::check_frame_len(raw_len)
            .map_err(|error| format!("validate initial grid request length: {error}"))?;
        let mut framed = prefix;
        framed.resize(prefix_len + payload_len, 0);
        stream
            .read_exact(&mut framed[prefix_len..])
            .map_err(|error| format!("read initial grid request payload: {error}"))?;

        let (envelope, consumed) = mux_protocol::unframe(&framed)
            .map_err(|error| format!("decode initial grid request: {error}"))?;
        if consumed != framed.len() {
            return Err(format!(
                "initial grid request left {} trailing bytes",
                framed.len() - consumed
            ));
        }
        let request = match envelope.payload {
            Some(EnvelopePayload::Request(request)) => request,
            payload => {
                return Err(format!(
                    "expected initial request envelope, got {payload:?}"
                ));
            }
        };
        let fetch = match request.body {
            Some(RequestBody::FetchGridUpdate(fetch)) => fetch,
            body => return Err(format!("expected initial FetchGridUpdate, got {body:?}")),
        };
        if fetch.pane_id != "quiet-pane" || fetch.since_generation != 0 {
            return Err(format!(
                "unexpected initial fetch target/generation: {}@{}",
                fetch.pane_id, fetch.since_generation
            ));
        }

        let cells = ["q", "u", "i", "e", "t"]
            .into_iter()
            .enumerate()
            .map(|(index, char)| Cell {
                char: char.to_string(),
                style: (index == 0).then(|| CellStyle {
                    bold: true,
                    italic: true,
                    underline: true,
                    strikethrough: true,
                    dim: true,
                    reverse: true,
                    underline_style: 4,
                    underline_color: Some(0x070809),
                    wide_char: true,
                    wrapline: true,
                    ..Default::default()
                }),
                foreground: if index == 0 { 0x010203 } else { 0xdddddd },
                background: if index == 0 { 0x040506 } else { 0x000000 },
                zerowidth: if index == 0 { "\u{301}" } else { "" }.to_string(),
                hyperlink: (index == 0).then(|| mux_protocol::Hyperlink {
                    id: "quiet-link".to_string(),
                    uri: "https://example.com/quiet".to_string(),
                }),
            })
            .collect();
        let response = Envelope {
            version: Some(mux_protocol::PROTOCOL_VERSION),
            payload: Some(EnvelopePayload::Response(Response {
                request_id: request.request_id,
                body: Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                    from_generation: 0,
                    to_generation: 7,
                    update: Some(FetchUpdate::FullSnapshot(FullGridSnapshot {
                        cols: 5,
                        rows: 1,
                        cells,
                        cursor: Some(mux_protocol::CursorState {
                            col: 4,
                            row: 0,
                            style: 3,
                            visible: true,
                            blinking: true,
                        }),
                        alternate_screen: true,
                        display_offset: 0,
                        modes: Some(
                            mux_protocol::terminal_mode::ALT_SCREEN
                                | mux_protocol::terminal_mode::APP_CURSOR
                                | mux_protocol::terminal_mode::BRACKETED_PASTE,
                        ),
                    })),
                })),
            })),
        };
        let response = mux_protocol::frame(&response)
            .map_err(|error| format!("encode initial grid response: {error}"))?;
        stream
            .write_all(&response)
            .map_err(|error| format!("write initial grid response: {error}"))?;
        stream
            .flush()
            .map_err(|error| format!("flush initial grid response: {error}"))
    }

    #[cfg(unix)]
    fn read_test_envelope(
        stream: &mut std::os::unix::net::UnixStream,
        context: &str,
    ) -> Result<Envelope, String> {
        use std::io::Read;

        let mut prefix = Vec::with_capacity(mux_protocol::MAX_VARINT_LEN);
        loop {
            let mut byte = [0u8; 1];
            stream
                .read_exact(&mut byte)
                .map_err(|error| format!("read {context} prefix: {error}"))?;
            prefix.push(byte[0]);
            if byte[0] & 0x80 == 0 {
                break;
            }
            if prefix.len() == mux_protocol::MAX_VARINT_LEN {
                return Err(format!("{context} used an overlong frame prefix"));
            }
        }
        let (raw_len, prefix_len) = mux_protocol::parse_len_prefix(&prefix)
            .map_err(|error| format!("parse {context} prefix: {error}"))?
            .ok_or_else(|| format!("{context} prefix was incomplete"))?;
        let payload_len = mux_protocol::check_frame_len(raw_len)
            .map_err(|error| format!("validate {context} length: {error}"))?;
        let mut framed = prefix;
        framed.resize(prefix_len + payload_len, 0);
        stream
            .read_exact(&mut framed[prefix_len..])
            .map_err(|error| format!("read {context} payload: {error}"))?;
        let (envelope, consumed) =
            mux_protocol::unframe(&framed).map_err(|error| format!("decode {context}: {error}"))?;
        if consumed != framed.len() {
            return Err(format!(
                "{context} left {} trailing bytes",
                framed.len() - consumed
            ));
        }
        Ok(envelope)
    }

    #[cfg(unix)]
    fn write_test_envelope(
        stream: &mut std::os::unix::net::UnixStream,
        envelope: &Envelope,
        context: &str,
    ) -> Result<(), String> {
        use std::io::Write;

        let framed =
            mux_protocol::frame(envelope).map_err(|error| format!("encode {context}: {error}"))?;
        stream
            .write_all(&framed)
            .map_err(|error| format!("write {context}: {error}"))?;
        stream
            .flush()
            .map_err(|error| format!("flush {context}: {error}"))
    }

    #[cfg(unix)]
    fn grid_response(request_id: u64, from: u64, to: u64, cursor_row: u32) -> Envelope {
        Envelope {
            version: Some(mux_protocol::PROTOCOL_VERSION),
            payload: Some(EnvelopePayload::Response(Response {
                request_id,
                body: Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                    from_generation: from,
                    to_generation: to,
                    update: Some(FetchUpdate::FullSnapshot(FullGridSnapshot {
                        cols: 2,
                        rows: 2,
                        cells: vec![
                            Cell {
                                char: " ".to_string(),
                                ..Cell::default()
                            };
                            4
                        ],
                        cursor: Some(mux_protocol::CursorState {
                            col: 0,
                            row: cursor_row,
                            style: 1,
                            visible: true,
                            blinking: false,
                        }),
                        alternate_screen: false,
                        display_offset: 0,
                        modes: None,
                    })),
                })),
            })),
        }
    }

    #[cfg(unix)]
    fn serve_dirty_during_fetch(
        mut stream: std::os::unix::net::UnixStream,
        first_fetch_received: async_channel::Sender<()>,
        release_first_response: async_channel::Receiver<()>,
    ) -> Result<(), String> {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| format!("set race server read timeout: {error}"))?;

        let first = read_test_envelope(&mut stream, "first grid request")?;
        let first = match first.payload {
            Some(EnvelopePayload::Request(request)) => request,
            payload => return Err(format!("expected first request, got {payload:?}")),
        };
        let first_fetch = match first.body {
            Some(RequestBody::FetchGridUpdate(fetch)) => fetch,
            body => return Err(format!("expected first grid fetch, got {body:?}")),
        };
        if first_fetch.pane_id != "race-pane" || first_fetch.since_generation != 0 {
            return Err(format!(
                "unexpected first fetch: {}@{}",
                first_fetch.pane_id, first_fetch.since_generation
            ));
        }

        first_fetch_received
            .send_blocking(())
            .map_err(|error| format!("signal first grid fetch: {error}"))?;
        release_first_response
            .recv_blocking()
            .map_err(|error| format!("wait to release first response: {error}"))?;
        write_test_envelope(
            &mut stream,
            &grid_response(first.request_id, 0, 7, 0),
            "first grid response",
        )?;

        let second = loop {
            let envelope = read_test_envelope(&mut stream, "catch-up request")?;
            let request = match envelope.payload {
                Some(EnvelopePayload::Request(request)) => request,
                payload => return Err(format!("expected catch-up request, got {payload:?}")),
            };
            match request.body {
                Some(RequestBody::FetchGridUpdate(fetch)) => break (request.request_id, fetch),
                Some(RequestBody::ResizePane(resize)) => {
                    if resize.pane_id != "race-pane" || resize.cols != 2 || resize.rows != 2 {
                        return Err(format!("unexpected resize during catch-up: {resize:?}"));
                    }
                    write_test_envelope(
                        &mut stream,
                        &Envelope {
                            version: Some(mux_protocol::PROTOCOL_VERSION),
                            payload: Some(EnvelopePayload::Response(Response {
                                request_id: request.request_id,
                                body: None,
                            })),
                        },
                        "resize response",
                    )?;
                }
                body => return Err(format!("expected catch-up grid fetch, got {body:?}")),
            }
        };
        let (second_request_id, second_fetch) = second;
        if second_fetch.pane_id != "race-pane" || second_fetch.since_generation != 7 {
            return Err(format!(
                "unexpected catch-up fetch: {}@{}",
                second_fetch.pane_id, second_fetch.since_generation
            ));
        }
        write_test_envelope(
            &mut stream,
            &grid_response(second_request_id, 7, 8, 1),
            "catch-up grid response",
        )
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn new_fetches_generation_zero_for_a_quiet_pane(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (client, server) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => panic!("create mock mux socket pair: {error}"),
        };
        if let Err(error) = client.set_nonblocking(true) {
            panic!("set mock mux client nonblocking: {error}");
        }
        let domain = match MuxDomain::connect_with_blocking_stream(client) {
            Ok(domain) => Arc::new(domain),
            Err(error) => panic!("connect mock mux domain: {error}"),
        };
        let server_thread = std::thread::spawn(move || serve_initial_grid(server));

        let (view, cx) = cx.add_window_view(|window, cx| {
            MuxPaneView::new(
                "quiet-pane".to_string(),
                domain,
                WeakEntity::new_invalid(),
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let initial_grid_applied = view.condition::<MuxPaneEvent>(cx, |view, _cx| {
            view.generation == 7 && !view.fetch_in_flight
        });
        match server_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("mock mux server failed: {error}"),
            Err(_) => panic!("mock mux server panicked"),
        }
        initial_grid_applied.await;

        view.read_with(cx, |view, cx| {
            assert_eq!(view.generation, 7);
            assert!(!view.fetch_in_flight);
            assert_eq!(snapshot_to_text(&view.snapshot), "quiet");

            let content = view.terminal.read(cx).last_content();
            assert!(content.mode.contains(Modes::ALT_SCREEN));
            assert_eq!(content.cursor.point, terminal::Point::new(0, 4));
            assert_eq!(content.cursor.shape, TerminalCursorShape::Underline);
            let cell = content
                .cells
                .iter()
                .find(|cell| cell.point == terminal::Point::new(0, 0))
                .unwrap_or_else(|| panic!("structured q cell missing from terminal content"));
            assert_eq!(cell.character(), 'q');
            assert_eq!(
                cell.foreground(),
                terminal::Color::Spec(Rgb { r: 1, g: 2, b: 3 })
            );
            assert_eq!(
                cell.background(),
                terminal::Color::Spec(Rgb { r: 4, g: 5, b: 6 })
            );
            assert!(cell.is_bold());
            assert!(cell.is_italic());
            assert!(cell.has_underline());
            assert!(cell.has_strikeout());
            assert!(cell.is_dim());
            assert!(cell.is_inverse());
            assert_eq!(cell.zerowidth(), Some(['\u{301}'].as_slice()));
            assert!(cell.has_undercurl());
            let hyperlink = cell
                .hyperlink()
                .unwrap_or_else(|| panic!("mux hyperlink missing"));
            assert_eq!(hyperlink.id(), Some("quiet-link"));
            assert_eq!(hyperlink.uri(), "https://example.com/quiet");
            assert!(content.mode.contains(Modes::APP_CURSOR));
            assert!(content.mode.contains(Modes::BRACKETED_PASTE));
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn dirty_during_fetch_triggers_cursor_catch_up(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (client, server) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => panic!("create fetch-race socket pair: {error}"),
        };
        if let Err(error) = client.set_nonblocking(true) {
            panic!("set fetch-race client nonblocking: {error}");
        }
        let domain = match MuxDomain::connect_with_blocking_stream(client) {
            Ok(domain) => Arc::new(domain),
            Err(error) => panic!("connect fetch-race mux domain: {error}"),
        };
        let (first_fetch_received, first_fetch) = async_channel::bounded(1);
        let (release_first_response, first_response_release) = async_channel::bounded(1);
        let server_thread = std::thread::spawn(move || {
            serve_dirty_during_fetch(server, first_fetch_received, first_response_release)
        });

        let (view, cx) = cx.add_window_view(|window, cx| {
            MuxPaneView::new(
                "race-pane".to_string(),
                domain.clone(),
                WeakEntity::new_invalid(),
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let first_fetch_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            while cx.executor().tick() {}
            if first_fetch.try_recv().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < first_fetch_deadline,
                "mock server did not receive the initial grid fetch"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        domain.broadcast_notification(mux_protocol::Notification {
            event: Some(NotifEvent::PaneDirty(mux_protocol::PaneDirty {
                pane_id: "race-pane".to_string(),
            })),
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(9));
        cx.run_until_parked();
        view.read_with(cx, |view, _cx| {
            assert!(view.fetch_in_flight);
            assert!(view.fetch_pending);
        });
        release_first_response
            .send_blocking(())
            .unwrap_or_else(|error| panic!("release first grid response: {error}"));
        let catch_up_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let catch_up_state = loop {
            while cx.executor().tick() {}
            let state = view.read_with(cx, |view, _cx| {
                (view.generation, view.fetch_in_flight, view.fetch_pending)
            });
            if state == (8, false, false) || std::time::Instant::now() >= catch_up_deadline {
                break state;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        match server_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!(
                "fetch-race server failed while client was at generation={}, in_flight={}, pending={}: {error}",
                catch_up_state.0, catch_up_state.1, catch_up_state.2,
            ),
            Err(_) => panic!("fetch-race server panicked"),
        }
        assert_eq!(
            catch_up_state,
            (8, false, false),
            "grid catch-up did not settle"
        );

        view.read_with(cx, |view, cx| {
            assert_eq!(view.generation, 8);
            assert!(!view.fetch_in_flight);
            assert!(!view.fetch_pending);
            assert_eq!(
                view.terminal.read(cx).last_content().cursor.point,
                terminal::Point::new(1, 0)
            );
        });
    }

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
    fn diff_generation_must_continue_from_client_checkpoint() {
        let valid = FetchGridUpdateResponse {
            from_generation: 5,
            to_generation: 6,
            update: Some(FetchUpdate::Diff(GridDiff::default())),
        };
        assert!(validate_generation_envelope(5, &valid).is_ok());

        let stale = FetchGridUpdateResponse {
            from_generation: 4,
            ..valid.clone()
        };
        assert!(validate_generation_envelope(5, &stale).is_err());
    }

    #[test]
    fn no_change_generation_must_equal_client_checkpoint() {
        let valid = FetchGridUpdateResponse {
            from_generation: 5,
            to_generation: 5,
            update: None,
        };
        assert!(validate_generation_envelope(5, &valid).is_ok());

        let future = FetchGridUpdateResponse {
            to_generation: 6,
            ..valid
        };
        assert!(validate_generation_envelope(5, &future).is_err());
    }

    #[test]
    fn full_snapshot_can_authoritatively_reset_generation() {
        let reset = FetchGridUpdateResponse {
            from_generation: 0,
            to_generation: 3,
            update: Some(FetchUpdate::FullSnapshot(FullGridSnapshot {
                cols: 1,
                rows: 1,
                cells: vec![Cell::default()],
                cursor: None,
                alternate_screen: false,
                display_offset: 0,
                modes: None,
            })),
        };
        assert!(validate_generation_envelope(99, &reset).is_ok());
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
            modes: None,
        };
        let diff = GridDiff {
            rows: vec![RowChange {
                row: 0,
                cells: vec![
                    Cell {
                        char: "a".to_string(),
                        ..Default::default()
                    },
                    Cell {
                        char: "X".to_string(),
                        ..Default::default()
                    },
                    Cell {
                        char: "c".to_string(),
                        ..Default::default()
                    },
                ],
            }],
        };
        if let Err(error) = apply_diff_to_snapshot(&mut snapshot, &diff) {
            panic!("valid row diff failed: {error}");
        }
        assert_eq!(snapshot.cells[0].char, "a");
        assert_eq!(snapshot.cells[1].char, "X");
        assert_eq!(snapshot.cells[2].char, "c");

        let before = snapshot.clone();
        let diff_oob = GridDiff {
            rows: vec![RowChange {
                row: 99,
                cells: vec![Cell::default(); 3],
            }],
        };
        assert!(apply_diff_to_snapshot(&mut snapshot, &diff_oob).is_err());
        assert_eq!(snapshot.cells, before.cells);

        let short_row = GridDiff {
            rows: vec![RowChange {
                row: 0,
                cells: vec![Cell::default(); 2],
            }],
        };
        assert!(apply_diff_to_snapshot(&mut snapshot, &short_row).is_err());
        assert_eq!(snapshot.cells, before.cells);
    }

    #[test]
    fn test_snapshot_to_text() {
        let snapshot = FullGridSnapshot {
            cols: 3,
            rows: 2,
            cells: vec![
                Cell {
                    char: "a".to_string(),
                    ..Default::default()
                },
                Cell {
                    char: "b".to_string(),
                    ..Default::default()
                },
                Cell {
                    char: "c".to_string(),
                    ..Default::default()
                },
                Cell {
                    char: "d".to_string(),
                    ..Default::default()
                },
                Cell {
                    char: "e".to_string(),
                    ..Default::default()
                },
                Cell {
                    char: " ".to_string(),
                    ..Default::default()
                },
            ],
            cursor: None,
            alternate_screen: false,
            display_offset: 0,
            modes: None,
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
