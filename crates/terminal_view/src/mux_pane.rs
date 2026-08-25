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
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use mux::MuxDomain;
use mux_protocol::input::{
    KeyDispatchContext, KeyDispatchResult, PaneModes, PrefixAction, PrefixModeConfig,
    PrefixModeMachine, handle_key_event, is_full_screen_active,
};
use mux_protocol::{
    FetchScrollbackResponse, FullGridSnapshot, GridDiff,
    fetch_grid_update_response::Update as FetchUpdate, notification::Event as NotifEvent,
};
use project::Project;
use settings::Settings;
use std::sync::Arc;
use terminal::{
    CursorShape as TerminalCursorShape, Hyperlink as TerminalHyperlink, MAX_SCROLL_HISTORY_LINES,
    Modes, Rgb, StructuredTerminalCell, StructuredTerminalCursor, StructuredTerminalSnapshot,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuxPaneEvent {
    TitleChanged,
    CloseRequested,
    /// §3.1/§16.6 an input transport failed (server unreachable, permission
    /// denied, etc.). Surfaces the error text so the workspace can show a
    /// toast instead of silently dropping the keystroke/mouse event.
    InputFailed {
        message: SharedString,
    },
    /// §16.7 the priority chain matched an extension global shortcut. The
    /// extension host runs off the GPUI thread (§5.2), so the action id is
    /// handed to the workspace instead of being executed here.
    ExtensionAction {
        action_id: SharedString,
    },
}

/// §16.7 Resolves a keystroke to an extension global-shortcut action id.
///
/// The extension host lives outside `terminal_view`, so the lookup is injected
/// with [`MuxPaneView::set_extension_shortcut_resolver`]; without one no
/// extension shortcut can match.
pub type ExtensionShortcutResolver = Arc<dyn Fn(&Keystroke) -> Option<SharedString> + Send + Sync>;

const HISTORY_PAGE_ROWS: u32 = 512;
/// Bound the client-side authoritative history cache independently of the
/// per-page wire bound. This prevents a malicious snapshot from reserving a
/// huge `cols * history_size` vector before the first RPC.
const MAX_SCROLLBACK_CELLS: usize = mux_protocol::MAX_GRID_CELLS * 16;

#[derive(Clone, Debug)]
struct HistoryCache {
    cols: usize,
    history_size: usize,
    history_version: u64,
    cells: Arc<Vec<StructuredTerminalCell>>,
}

#[derive(Debug)]
enum PreparedFetchUpdate {
    NoChange {
        expected_generation: u64,
        generation: u64,
    },
    Snapshot {
        expected_generation: u64,
        generation: u64,
        snapshot: FullGridSnapshot,
        history_cache: HistoryCache,
        structured: StructuredTerminalSnapshot,
    },
}

#[derive(Debug)]
struct PrepareFetchError {
    source: anyhow::Error,
    retry: bool,
}

impl PrepareFetchError {
    fn invalid(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            retry: false,
        }
    }

    fn checkpoint_changed(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            retry: true,
        }
    }
}

impl std::fmt::Display for PrepareFetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for PrepareFetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

fn classify_fetch_rpc_error(error: anyhow::Error) -> PrepareFetchError {
    let message = error.to_string();
    let retryable = [
        "connection closed",
        "request timeout",
        "mux write queue is full",
        "mux write channel disconnected",
    ]
    .iter()
    .any(|marker| message.contains(marker));
    if retryable {
        PrepareFetchError::checkpoint_changed(error)
    } else {
        PrepareFetchError::invalid(error)
    }
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
    /// A delayed retry keeps a transient transport failure from permanently
    /// stopping reconciliation without spinning the GPUI executor.
    fetch_retry_task: Option<Task<()>>,
    /// §3.3 current grid snapshot (recovery path for reconnect)
    snapshot: FullGridSnapshot,
    /// Oldest-to-newest authoritative history for `snapshot`.
    history_cache: HistoryCache,
    /// §15.7 zoom state
    zoomed: bool,
    /// §3.10 last resize dimensions sent to server (cols, rows)
    last_sent_size: (u32, u32),
    /// §16.5 / §16.7 Shared prefix-mode state machine (live input router).
    prefix_machine: PrefixModeMachine,
    prefix_timeout_task: Option<gpui::Task<()>>,
    /// §16.7 extension global-shortcut lookup, injected by the host.
    extension_shortcuts: Option<ExtensionShortcutResolver>,
    /// §16.7 Agent CLI passthrough: while set, every key goes straight to the
    /// PTY without prefix/copy-mode interception.
    agent_cli_mode: bool,
    /// §3.3 read-only attach (Plan 33): the client renders but never writes.
    /// Shared with the mouse input sink, which has no GPUI context.
    read_only: Arc<std::sync::atomic::AtomicBool>,
    /// §3.1 mouse-input transport errors buffered from the input sink (which
    /// has no GPUI context) and drained into InputFailed events at render.
    pending_input_errors: std::sync::Arc<std::sync::Mutex<Vec<SharedString>>>,
}

impl MuxPaneView {
    /// §3.3 Create view with DisplayOnly Terminal + TerminalView.
    /// §3.1 structured snapshots populate the display-only emulator; raw PTY
    /// bytes from PaneOutput are never parsed by this client.
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
        let read_only = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
            #[cfg(not(target_family = "wasm"))]
            let executor = cx.background_executor().clone();
            let errors = pending_input_errors.clone();
            let read_only = read_only.clone();
            let sink: std::sync::Arc<dyn Fn(Vec<u8>) + Send + Sync> =
                std::sync::Arc::new(move |bytes: Vec<u8>| {
                    // §3.3 read-only attach (Plan 33): mouse reports are input
                    // too, so they are dropped alongside keystrokes.
                    if bytes.is_empty() || read_only.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let domain = domain.clone();
                    let pane_id = pane_id.clone();
                    let errors = errors.clone();
                    let send = async move {
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
                    };
                    #[cfg(not(target_family = "wasm"))]
                    executor.spawn(send).detach();
                    #[cfg(target_family = "wasm")]
                    wasm_bindgen_futures::spawn_local(send);
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
            history_size: 0,
            history_version: 0,
            modes: None,
        };
        let history_cache = HistoryCache {
            cols: 80,
            history_size: 0,
            history_version: 0,
            cells: Arc::new(Vec::new()),
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
            fetch_retry_task: None,
            snapshot,
            history_cache,
            zoomed: false,
            last_sent_size: (80, 24),
            prefix_machine: PrefixModeMachine::new(PrefixModeConfig::default()),
            prefix_timeout_task: None,
            extension_shortcuts: None,
            agent_cli_mode: false,
            read_only,
            pending_input_errors,
        };
        view.start_notification_listener(cx);
        // Subscribe before the initial fetch so output produced while the request is
        // in flight cannot fall into a fetch-before-subscribe race. A quiet pane
        // emits no future notification, so construction itself must fetch generation 0.
        view.schedule_fetch(cx);
        view
    }

    /// §3.1 PaneOutput is a lossy wakeup only. The server remains the sole VT
    /// parser; every render-affecting change is pulled through the structured
    /// grid snapshot/diff path.
    fn start_notification_listener(&mut self, cx: &mut Context<Self>) {
        let pane_id = self.pane_id.clone();
        let rx = self.domain.subscribe();
        let weak = cx.entity().downgrade();

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
            // PaneOutput is only a supplemental dirty signal. The byte payload
            // must never be parsed by the client.
            NotifEvent::PaneOutput(chunk) if chunk.pane_id == pane_id && !chunk.data.is_empty() => {
                *pending_dirty = true;
                true
            }
            NotifEvent::PaneDirty(dirty) if dirty.pane_id == pane_id => {
                *pending_dirty = true;
                true
            }
            NotifEvent::PaneRemoved(removed) if removed.pane_id == pane_id => {
                if let Err(error) = weak.update(cx, |view, cx| {
                    view.notification_task = None;
                    cx.emit(MuxPaneEvent::CloseRequested);
                }) {
                    tracing::debug!(error = %error, "MuxPaneView dropped after pane removal");
                }
                false
            }
            NotifEvent::PaneTitleChanged(changed) if changed.pane_id == pane_id => {
                if let Err(error) = weak.update(cx, |view, cx| {
                    view.terminal.update(cx, |terminal, cx| {
                        terminal.set_display_title(changed.title.clone(), cx);
                    });
                    cx.emit(MuxPaneEvent::TitleChanged);
                }) {
                    tracing::debug!(error = %error, "MuxPaneView dropped after pane title update");
                }
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
            if let Err(error) = weak.update(cx, |view, cx| view.schedule_fetch(cx)) {
                tracing::debug!(error = %error, "MuxPaneView dropped before grid fetch");
            }
        }
    }
    /// §3.3 Schedule a structured fetch. Full snapshots load every matching
    /// history page before returning to the GPUI thread, so partial checkpoints
    /// can never mutate the renderer or advance the local generation.
    fn schedule_fetch(&mut self, cx: &mut Context<Self>) {
        self.fetch_retry_task.take();
        if self.fetch_in_flight {
            self.fetch_pending = true;
            return;
        }
        self.fetch_in_flight = true;
        self.fetch_pending = false;
        let since = self.generation;

        let pane_id = self.pane_id.clone();
        let domain = self.domain.clone();
        let snapshot = self.snapshot.clone();
        let history_cache = self.history_cache.clone();
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let result = prepare_fetch_update(
                &domain,
                &pane_id,
                since,
                snapshot,
                history_cache,
            )
            .await;
            match weak.update(cx, |view, cx| {
                view.fetch_in_flight = false;
                let mut retry_later = false;
                match result {
                    Ok(update) => {
                        if let Err(error) = view.apply_prepared_fetch_update(update, cx) {
                            tracing::error!(pane_id = %pane_id, error = %error, "apply grid update failed");
                            cx.emit(MuxPaneEvent::InputFailed {
                                message: SharedString::from(format!(
                                    "failed to apply mux pane {pane_id} grid: {error}"
                                )),
                            });
                        }
                    }
                    Err(error) => {
                        tracing::error!(pane_id = %pane_id, error = %error.source, "prepare grid update failed");
                        retry_later = error.retry;
                        view.fetch_pending |= error.retry;
                        cx.emit(MuxPaneEvent::InputFailed {
                            message: SharedString::from(format!(
                                "failed to fetch mux pane {pane_id} grid: {}",
                                error.source
                            )),
                        });
                    }
                }
                if view.fetch_pending {
                    if retry_later {
                        view.schedule_fetch_retry(cx);
                    } else {
                        view.schedule_fetch(cx);
                    }
                }
            }) {
                Ok(()) => {}
                Err(_) => tracing::warn!("MuxPaneView dropped after fetch"),
            }
        })
        .detach();
    }

    fn schedule_fetch_retry(&mut self, cx: &mut Context<Self>) {
        if self.fetch_retry_task.is_some() {
            return;
        }
        let weak = cx.entity().downgrade();
        self.fetch_retry_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            if let Err(error) = weak.update(cx, |view, cx| {
                view.fetch_retry_task = None;
                if view.fetch_pending {
                    view.schedule_fetch(cx);
                }
            }) {
                tracing::debug!(error = %error, "MuxPaneView dropped before fetch retry");
            }
        }));
    }

    fn apply_prepared_fetch_update(
        &mut self,
        update: PreparedFetchUpdate,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match update {
            PreparedFetchUpdate::NoChange {
                expected_generation,
                generation,
            } => {
                validate_prepared_generation(self.generation, expected_generation)?;
                self.generation = generation;
            }
            PreparedFetchUpdate::Snapshot {
                expected_generation,
                generation,
                snapshot,
                history_cache,
                structured,
            } => {
                validate_prepared_generation(self.generation, expected_generation)?;
                let (previous_scrollback_offset, _) =
                    self.terminal_view.read(cx).mux_scrollback_state();
                self.terminal
                    .update(cx, |terminal, cx| {
                        terminal.apply_structured_snapshot(&structured, cx)
                    })
                    .map_err(|error| {
                        anyhow::anyhow!("structured terminal import failed: {error}")
                    })?;
                let scrollback_version = (snapshot.history_version, generation);
                let display_offset = usize::try_from(snapshot.display_offset)
                    .map_err(|_| anyhow::anyhow!("mux display offset exceeds client limits"))?;
                self.snapshot = snapshot;
                let history_rows = history_cache.history_size;
                self.history_cache = history_cache;
                self.generation = generation;
                self.terminal_view.update(cx, |view, cx| {
                    view.update_scrollback_version(scrollback_version, cx);
                    view.apply_mux_scrollback_offset(
                        previous_scrollback_offset,
                        display_offset,
                        history_rows,
                        cx,
                    );
                });
            }
        }
        cx.notify();
        Ok(())
    }

    /// §3.10 / §16.7 keystroke → priority chain → routed action.
    fn dispatch_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bytes = keystroke_to_bytes(keystroke);
        if bytes.is_empty() {
            return;
        }

        // Only ask the keymap while waiting for a chord: outside prefix mode a
        // bound key was already dispatched by GPUI before key_down ran, and
        // re-resolving it here would double-execute the action.
        let prefix_binding = if self.prefix_machine.is_prefix_wait() {
            prefix_binding_for(keystroke, window, cx)
        } else {
            None
        };

        let result = self.resolve_keystroke(keystroke, &bytes, prefix_binding.is_some(), cx);
        self.apply_dispatch_result(result, keystroke, prefix_binding, window, cx);
    }

    /// §16.7 Run the shared priority chain for `keystroke` and return its
    /// routing decision, advancing the prefix-mode state machine.
    fn resolve_keystroke(
        &mut self,
        keystroke: &Keystroke,
        bytes: &[u8],
        binding_match: bool,
        cx: &Context<Self>,
    ) -> KeyDispatchResult {
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

        let terminal_view = self.terminal_view.read(cx);
        let ime_composing = terminal_view.is_ime_composing();
        let copy_mode =
            terminal_view.copy_mode_state().active || self.terminal.read(cx).vi_mode_enabled();

        let mut dispatch_context = KeyDispatchContext {
            ime_composing,
            extension_shortcut: self
                .extension_shortcuts
                .as_ref()
                .and_then(|resolve| resolve(keystroke))
                .map(|action_id| action_id.to_string()),
            prefix_mode_machine: self.prefix_machine.clone(),
            pane_modes,
            agent_cli_mode: self.agent_cli_mode,
            copy_mode,
        };

        // Prefix key entry is owned by EnterPrefixMode action; raw path sees unbound keys.
        let result = handle_key_event(bytes, false, binding_match, &mut dispatch_context);
        self.prefix_machine = dispatch_context.prefix_mode_machine;
        result
    }

    /// §16.7 Execute the routing decision produced by [`Self::resolve_keystroke`].
    fn apply_dispatch_result(
        &mut self,
        result: KeyDispatchResult,
        keystroke: &Keystroke,
        prefix_binding: Option<Box<dyn gpui::Action>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            // The IME owns the keystroke; it reaches the PTY as committed text.
            KeyDispatchResult::RouteToIme => {}
            // Prefix key entry: the chord key itself is never forwarded.
            KeyDispatchResult::Passthrough => {}
            KeyDispatchResult::ExecuteExtensionAction(action_id) => {
                cx.emit(MuxPaneEvent::ExtensionAction {
                    action_id: SharedString::from(action_id),
                });
            }
            KeyDispatchResult::ExecutePrefixCommand => {
                self.clear_prefix_timeout();
                match prefix_binding {
                    Some(action) => window.dispatch_action(action, cx),
                    // The chain only reports a prefix command when a binding
                    // matched, so this is unreachable in practice; the key is
                    // still swallowed rather than leaked to the PTY.
                    None => tracing::warn!(
                        pane_id = %self.pane_id,
                        "prefix command without a matching binding"
                    ),
                }
                cx.notify();
            }
            KeyDispatchResult::RouteToCopyMode => {
                self.terminal_view.update(cx, |terminal_view, cx| {
                    terminal_view.dispatch_copy_mode_keystroke(keystroke, cx)
                });
                cx.notify();
            }
            KeyDispatchResult::RouteToAgentCli => {
                self.send_bytes_to_pty(keystroke_to_bytes(keystroke), cx);
            }
            KeyDispatchResult::SendLiteral { bytes: send_bytes }
            | KeyDispatchResult::SendToPty { bytes: send_bytes } => {
                self.send_bytes_to_pty(send_bytes, cx);
            }
        }
    }

    fn clear_prefix_timeout(&mut self) {
        if let Some(task) = self.prefix_timeout_task.take() {
            task.detach();
        }
    }

    /// §16.7 Inject the extension global-shortcut lookup. Without it the
    /// extension step of the priority chain can never match.
    pub fn set_extension_shortcut_resolver(&mut self, resolver: Option<ExtensionShortcutResolver>) {
        self.extension_shortcuts = resolver;
    }

    /// §16.7 Agent CLI passthrough state.
    pub fn set_agent_cli_mode(&mut self, agent_cli_mode: bool, cx: &mut Context<Self>) {
        self.agent_cli_mode = agent_cli_mode;
        cx.notify();
    }

    pub fn agent_cli_mode(&self) -> bool {
        self.agent_cli_mode
    }

    /// §3.3 Read-only attach (Plan 33): the pane renders server output but
    /// never sends input. Set from the attach role once the session is joined.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only
            .store(read_only, std::sync::atomic::Ordering::SeqCst);
        self.terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.set_read_only(read_only, cx);
        });
        cx.notify();
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn send_bytes_to_pty(&self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        // §3.3 read-only attach (Plan 33): the server would reject the write
        // anyway, so drop it here and keep the UI honest about it.
        if bytes.is_empty() || self.is_read_only() {
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

    /// Apply metadata from the server-authoritative pane snapshot without
    /// sending any mutating RPC back to the server.
    pub fn reconcile_metadata_from_snapshot(
        &mut self,
        title: Option<&str>,
        zoomed: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if let Some(title) = title {
            self.terminal.update(cx, |terminal, cx| {
                terminal.set_display_title(title.to_string(), cx);
            });
            cx.emit(MuxPaneEvent::TitleChanged);
        }
        if let Some(zoomed) = zoomed {
            self.zoomed = zoomed;
        }
        cx.notify();
    }

    /// §3.10 resize — notify server of new dimensions.
    pub fn resize(&mut self, cols: u32, rows: u32, _cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        let resize = async move {
            if let Err(e) = domain.resize_pane(&pane_id, cols, rows).await {
                tracing::error!(error = %e, "resize_pane failed");
            }
        };
        #[cfg(not(target_family = "wasm"))]
        _cx.background_executor().spawn(resize).detach();
        #[cfg(target_family = "wasm")]
        wasm_bindgen_futures::spawn_local(resize);
    }

    /// §15.7 Whether this pane is currently zoomed.
    pub fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    /// §15.7 Set zoom state and notify server.
    pub fn set_zoomed(&mut self, zoomed: bool, _cx: &mut Context<Self>) {
        self.zoomed = zoomed;
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        let zoom = async move {
            if let Err(e) = domain.zoom_pane(&pane_id, zoomed).await {
                tracing::error!(error = %e, "zoom_pane failed");
            }
        };
        #[cfg(not(target_family = "wasm"))]
        _cx.background_executor().spawn(zoom).detach();
        #[cfg(target_family = "wasm")]
        wasm_bindgen_futures::spawn_local(zoom);
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

/// §16.7 The prefix-mode command bound to `keystroke`, if the keymap has one.
///
/// Reaching this point means GPUI already tried to dispatch the binding and no
/// handler consumed it (action dispatch stops propagation by default), so the
/// action is re-dispatched here rather than executed twice.
fn prefix_binding_for(
    keystroke: &Keystroke,
    window: &Window,

    cx: &App,
) -> Option<Box<dyn gpui::Action>> {
    let context_stack = window.context_stack();
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let (bindings, _pending) =
        keymap.bindings_for_input(std::slice::from_ref(keystroke), &context_stack);
    bindings
        .first()
        .map(|binding| binding.action().boxed_clone())
}

/// §3.1 keystroke → terminal byte sequence (xterm standard).
/// Handles Ctrl-letter, Alt (ESC prefix), arrow keys, function keys.
pub fn keystroke_to_bytes(keystroke: &Keystroke) -> Vec<u8> {
    let ctrl = keystroke.modifiers.control;
    let alt = keystroke.modifiers.alt;
    let mut bytes = Vec::new();

    let key_char = keystroke
        .key_char
        .as_ref()
        .or_else(|| (keystroke.key.chars().count() == 1).then_some(&keystroke.key));
    if let Some(key_char) = key_char {
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

async fn prepare_fetch_update(
    domain: &MuxDomain,
    pane_id: &str,
    expected_generation: u64,
    mut snapshot: FullGridSnapshot,
    history_cache: HistoryCache,
) -> Result<PreparedFetchUpdate, PrepareFetchError> {
    let response = domain
        .fetch_grid_update(pane_id, expected_generation)
        .await
        .map_err(classify_fetch_rpc_error)?;
    validate_generation_envelope(expected_generation, &response)
        .map_err(PrepareFetchError::invalid)?;
    let generation = response.to_generation;

    match response.update {
        None => Ok(PreparedFetchUpdate::NoChange {
            expected_generation,
            generation,
        }),
        Some(FetchUpdate::Diff(diff)) => {
            apply_diff_to_snapshot(&mut snapshot, &diff).map_err(PrepareFetchError::invalid)?;
            let history_cache = matching_history_cache(&snapshot, Some(&history_cache))
                .cloned()
                .ok_or_else(|| {
                    PrepareFetchError::invalid(anyhow::anyhow!(
                        "mux grid diff changed history metadata without a full snapshot"
                    ))
                })?;
            let structured = structured_terminal_snapshot(&snapshot, &history_cache)
                .map_err(PrepareFetchError::invalid)?;
            Ok(PreparedFetchUpdate::Snapshot {
                expected_generation,
                generation,
                snapshot,
                history_cache,
                structured,
            })
        }
        Some(FetchUpdate::FullSnapshot(full)) => {
            validate_snapshot_metadata(&full)?;
            let (history_cache, fetched_history) =
                match matching_history_cache(&full, Some(&history_cache)) {
                    Some(cache) => (cache.clone(), false),
                    // An empty history is trivially consistent, so committing it
                    // needs neither page fetches nor a checkpoint round trip.
                    None if full.history_size == 0 => (
                        HistoryPageAccumulator::new(&full)
                            .and_then(HistoryPageAccumulator::finish)?,
                        false,
                    ),
                    None => (fetch_history_checkpoint(domain, pane_id, &full).await?, true),
                };
            if fetched_history {
                confirm_grid_checkpoint(domain, pane_id, generation).await?;
            }
            let structured = structured_terminal_snapshot(&full, &history_cache)
                .map_err(PrepareFetchError::invalid)?;
            Ok(PreparedFetchUpdate::Snapshot {
                expected_generation,
                generation,
                snapshot: full,
                history_cache,
                structured,
            })
        }
    }
}

fn validate_snapshot_metadata(
    snapshot: &FullGridSnapshot,
) -> Result<(), PrepareFetchError> {
    let cols = usize::try_from(snapshot.cols)
        .map_err(|_| PrepareFetchError::invalid(anyhow::anyhow!("mux grid columns exceed client limits")))?;
    let rows = usize::try_from(snapshot.rows)
        .map_err(|_| PrepareFetchError::invalid(anyhow::anyhow!("mux grid rows exceed client limits")))?;
    let history_size = usize::try_from(snapshot.history_size).map_err(|_| {
        PrepareFetchError::invalid(anyhow::anyhow!("mux history size exceeds client limits"))
    })?;
    let display_offset = usize::try_from(snapshot.display_offset).map_err(|_| {
        PrepareFetchError::invalid(anyhow::anyhow!("mux display offset exceeds client limits"))
    })?;
    let expected_cells = mux_protocol::checked_grid_cell_count(cols, rows)
        .map_err(|message| PrepareFetchError::invalid(anyhow::anyhow!("invalid mux grid dimensions: {message}")))?;
    if snapshot.cells.len() != expected_cells {
        return Err(PrepareFetchError::invalid(anyhow::anyhow!(
            "mux grid has {} cells, expected {} for {}x{}",
            snapshot.cells.len(),
            expected_cells,
            cols,
            rows
        )));
    }
    if history_size > MAX_SCROLL_HISTORY_LINES {
        return Err(PrepareFetchError::invalid(anyhow::anyhow!(
            "mux history has {history_size} rows, exceeding client limit {MAX_SCROLL_HISTORY_LINES}"
        )));
    }
    if display_offset > history_size {
        return Err(PrepareFetchError::invalid(anyhow::anyhow!(
            "mux display offset {display_offset} exceeds {history_size} history rows"
        )));
    }
    let history_cells = cols.checked_mul(history_size).ok_or_else(|| {
        PrepareFetchError::invalid(anyhow::anyhow!("mux history cell count overflow"))
    })?;
    if history_cells > MAX_SCROLLBACK_CELLS {
        return Err(PrepareFetchError::invalid(anyhow::anyhow!(
            "mux history has {history_cells} cells, exceeding client limit {MAX_SCROLLBACK_CELLS}"
        )));
    }
    if let Some(cursor) = &snapshot.cursor
        && (usize::try_from(cursor.col).unwrap_or(usize::MAX) >= cols
            || usize::try_from(cursor.row).unwrap_or(usize::MAX) >= rows)
    {
        return Err(PrepareFetchError::invalid(anyhow::anyhow!(
            "mux cursor ({}, {}) lies outside {}x{} grid",
            cursor.col,
            cursor.row,
            cols,
            rows
        )));
    }
    Ok(())
}

async fn fetch_history_checkpoint(
    domain: &MuxDomain,
    pane_id: &str,
    snapshot: &FullGridSnapshot,
) -> Result<HistoryCache, PrepareFetchError> {
    let mut accumulator = HistoryPageAccumulator::new(snapshot)?;
    let page_rows = history_page_rows(snapshot.cols as usize);
    while accumulator.next_row < snapshot.history_size {
        let remaining = snapshot.history_size - accumulator.next_row;
        let count = remaining.min(page_rows);
        let page = domain
            .fetch_scrollback(pane_id, accumulator.next_row, 1, count)
            .await
            .map_err(classify_fetch_rpc_error)?;
        let done = accumulator.push(page, count)?;
        if done {
            break;
        }
    }
    accumulator.finish()
}

async fn confirm_grid_checkpoint(
    domain: &MuxDomain,
    pane_id: &str,
    generation: u64,
) -> Result<(), PrepareFetchError> {
    let response = domain
        .fetch_grid_update(pane_id, generation)
        .await
        .map_err(classify_fetch_rpc_error)?;
    validate_generation_envelope(generation, &response).map_err(PrepareFetchError::invalid)?;
    if response.from_generation != generation
        || response.to_generation != generation
        || response.update.is_some()
    {
        return Err(PrepareFetchError::checkpoint_changed(anyhow::anyhow!(
            "mux grid changed while history was being fetched: expected stable generation {generation}, got {} -> {}",
            response.from_generation,
            response.to_generation
        )));
    }
    Ok(())
}

fn matching_history_cache<'a>(
    snapshot: &FullGridSnapshot,
    cache: Option<&'a HistoryCache>,
) -> Option<&'a HistoryCache> {
    let cols = usize::try_from(snapshot.cols).ok()?;
    let history_size = usize::try_from(snapshot.history_size).ok()?;
    cache.filter(|cache| {
        cache.cols == cols
            && cache.history_size == history_size
            && cache.history_version == snapshot.history_version
            && cache.cells.len() == cols.saturating_mul(history_size)
    })
}

fn history_page_rows(cols: usize) -> u32 {
    let rows = mux_protocol::MAX_GRID_CELLS
        .checked_div(cols.max(1))
        .unwrap_or(1)
        .max(1);
    u32::try_from(rows.min(HISTORY_PAGE_ROWS as usize)).unwrap_or(1)
}

struct HistoryPageAccumulator {
    cols: usize,
    history_size: usize,
    history_version: u64,
    next_row: u32,
    cells: Vec<StructuredTerminalCell>,
}

impl HistoryPageAccumulator {
    fn new(snapshot: &FullGridSnapshot) -> Result<Self, PrepareFetchError> {
        let cols = usize::try_from(snapshot.cols).map_err(|_| {
            PrepareFetchError::invalid(anyhow::anyhow!("mux grid columns exceed client limits"))
        })?;
        let history_size = usize::try_from(snapshot.history_size).map_err(|_| {
            PrepareFetchError::invalid(anyhow::anyhow!("mux history size exceeds client limits"))
        })?;
        if cols == 0 || cols > mux_protocol::MAX_GRID_COLUMNS {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history has invalid column count {cols}"
            )));
        }
        if history_size > MAX_SCROLL_HISTORY_LINES {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history has {history_size} rows, exceeding client limit {MAX_SCROLL_HISTORY_LINES}"
            )));
        }
        if snapshot.display_offset > snapshot.history_size {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux display offset {} exceeds {} history rows",
                snapshot.display_offset,
                snapshot.history_size
            )));
        }
        let cell_capacity = cols.checked_mul(history_size).ok_or_else(|| {
            PrepareFetchError::invalid(anyhow::anyhow!("mux history cell count overflow"))
        })?;
        if cell_capacity > MAX_SCROLLBACK_CELLS {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history has {cell_capacity} cells, exceeding client limit {MAX_SCROLLBACK_CELLS}"
            )));
        }
        Ok(Self {
            cols,
            history_size,
            history_version: snapshot.history_version,
            next_row: 0,
            cells: Vec::with_capacity(cell_capacity),
        })
    }

    fn push(
        &mut self,
        page: FetchScrollbackResponse,
        requested_count: u32,
    ) -> Result<bool, PrepareFetchError> {
        let requested_count = usize::try_from(requested_count).map_err(|_| {
            PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history page count exceeds client limits"
            ))
        })?;
        let remaining = self.history_size.saturating_sub(self.next_row as usize);
        if requested_count == 0 || requested_count > remaining {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history page requested {requested_count} rows with {remaining} remaining"
            )));
        }
        if page.lines.len() != requested_count {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history page returned {} rows, expected {requested_count}",
                page.lines.len()
            )));
        }
        if page.scrollback_version != self.history_version {
            return Err(PrepareFetchError::checkpoint_changed(anyhow::anyhow!(
                "mux history changed during pagination: expected version {}, got {}",
                self.history_version,
                page.scrollback_version
            )));
        }
        let total_lines = usize::try_from(page.total_lines).map_err(|_| {
            PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history total row count exceeds client limits"
            ))
        })?;
        if total_lines != self.history_size {
            return Err(PrepareFetchError::checkpoint_changed(anyhow::anyhow!(
                "mux history changed during pagination: expected {} rows, got {}",
                self.history_size,
                page.total_lines
            )));
        }
        if page.lines.is_empty() && self.next_row as usize != self.history_size {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history page at row {} was empty before completion",
                self.next_row
            )));
        }

        let mut page_cells = Vec::with_capacity(page.lines.len().saturating_mul(self.cols));
        let mut expected_row = self.next_row;
        for row in page.lines {
            if row.row != expected_row {
                return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                    "mux history row sequence expected {}, got {}",
                    expected_row,
                    row.row
                )));
            }
            if row.cells.len() != self.cols {
                return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                    "mux history row {} has {} cells, expected {}",
                    row.row,
                    row.cells.len(),
                    self.cols
                )));
            }
            for (column, cell) in row.cells.iter().enumerate() {
                page_cells.push(
                    structured_terminal_cell(
                        cell,
                        &format!("history row {}, column {column}", row.row),
                    )
                    .map_err(PrepareFetchError::invalid)?,
                );
            }
            expected_row = expected_row.checked_add(1).ok_or_else(|| {
                PrepareFetchError::invalid(anyhow::anyhow!("mux history row index overflow"))
            })?;
            if expected_row as usize > self.history_size {
                return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                    "mux history page exceeded declared row count {}",
                    self.history_size
                )));
            }
        }
        self.cells.extend(page_cells);
        self.next_row = expected_row;
        Ok(self.next_row as usize == self.history_size)
    }

    fn finish(self) -> Result<HistoryCache, PrepareFetchError> {
        if self.next_row as usize != self.history_size {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history pagination stopped at row {}, expected {}",
                self.next_row,
                self.history_size
            )));
        }
        let expected_cells = self.cols.checked_mul(self.history_size).ok_or_else(|| {
            PrepareFetchError::invalid(anyhow::anyhow!("mux history cell count overflow"))
        })?;
        if self.cells.len() != expected_cells {
            return Err(PrepareFetchError::invalid(anyhow::anyhow!(
                "mux history has {} cells, expected {}",
                self.cells.len(),
                expected_cells
            )));
        }
        Ok(HistoryCache {
            cols: self.cols,
            history_size: self.history_size,
            history_version: self.history_version,
            cells: Arc::new(self.cells),
        })
    }
}

fn validate_prepared_generation(
    current_generation: u64,
    expected_generation: u64,
) -> anyhow::Result<()> {
    if current_generation != expected_generation {
        anyhow::bail!(
            "mux grid checkpoint changed locally from {} to {} while fetching",
            expected_generation,
            current_generation
        );
    }
    Ok(())
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
        Some(FetchUpdate::Diff(_)) if response.to_generation <= current_generation => {
            anyhow::bail!(
                "mux grid diff does not advance generation {} -> {}",
                response.from_generation,
                response.to_generation
            )
        }
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

/// Convert the active wire snapshot plus its validated history checkpoint into
/// the terminal crate's transport-neutral DTO.
fn structured_terminal_snapshot(
    snapshot: &FullGridSnapshot,
    history_cache: &HistoryCache,
) -> anyhow::Result<StructuredTerminalSnapshot> {
    let cols = usize::try_from(snapshot.cols)
        .map_err(|_| anyhow::anyhow!("mux grid columns exceed client limits"))?;
    let rows = usize::try_from(snapshot.rows)
        .map_err(|_| anyhow::anyhow!("mux grid rows exceed client limits"))?;
    let history_size = usize::try_from(snapshot.history_size)
        .map_err(|_| anyhow::anyhow!("mux history size exceeds client limits"))?;
    let display_offset = usize::try_from(snapshot.display_offset)
        .map_err(|_| anyhow::anyhow!("mux display offset exceeds client limits"))?;
    if history_size > MAX_SCROLL_HISTORY_LINES {
        anyhow::bail!(
            "mux history has {history_size} rows, exceeding client limit {MAX_SCROLL_HISTORY_LINES}"
        );
    }
    if display_offset > history_size {
        anyhow::bail!("mux display offset {display_offset} exceeds {history_size} history rows");
    }
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
    if matching_history_cache(snapshot, Some(history_cache)).is_none() {
        anyhow::bail!("mux history cache does not match full snapshot checkpoint");
    }

    let cells = snapshot
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| structured_terminal_cell(cell, &format!("grid cell {index}")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let history = history_cache.cells.as_ref().clone();

    let cursor = snapshot
        .cursor
        .as_ref()
        .map(|cursor| {
            let cursor_row = usize::try_from(cursor.row)
                .map_err(|_| anyhow::anyhow!("mux cursor row exceeds client limits"))?;
            let cursor_col = usize::try_from(cursor.col)
                .map_err(|_| anyhow::anyhow!("mux cursor column exceeds client limits"))?;
            if cursor_row >= rows || cursor_col >= cols {
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
                point: terminal::Point::new(
                    i32::try_from(cursor_row)
                        .map_err(|_| anyhow::anyhow!("mux cursor row exceeds terminal limits"))?,
                    cursor_col,
                ),
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
        history,
        display_offset,
        cursor,
        alternate_screen: snapshot.alternate_screen,
        modes,
    })
}

fn structured_terminal_cell(
    cell: &mux_protocol::Cell,
    location: &str,
) -> anyhow::Result<StructuredTerminalCell> {
    let mut chars = cell.char.chars();
    let character = chars
        .next()
        .ok_or_else(|| anyhow::anyhow!("mux {location} has no character"))?;
    if chars.next().is_some() {
        anyhow::bail!("mux {location} contains more than one Unicode scalar");
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
    let cols = usize::try_from(snapshot.cols)
        .map_err(|_| anyhow::anyhow!("cached mux grid columns exceed client limits"))?;
    let rows = usize::try_from(snapshot.rows)
        .map_err(|_| anyhow::anyhow!("cached mux grid rows exceed client limits"))?;
    let expected_cells = mux_protocol::checked_grid_cell_count(cols, rows)
        .map_err(|message| anyhow::anyhow!("invalid cached mux grid dimensions: {message}"))?;
    if snapshot.cells.len() != expected_cells {
        anyhow::bail!(
            "cached mux grid has {} cells, expected {expected_cells}",
            snapshot.cells.len()
        );
    }

    for row_change in &diff.rows {
        let row = usize::try_from(row_change.row)
            .map_err(|_| anyhow::anyhow!("mux grid diff row exceeds client limits"))?;
        if row >= rows {
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
        let row = usize::try_from(row_change.row)
            .map_err(|_| anyhow::anyhow!("mux grid diff row exceeds client limits"))?;
        let row_start = row * cols;
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

        // §16.4 a11y: the root exposes the pane title as a labelled group,
        // while the TerminalElement child owns the Terminal/TextRun tree.
        //
        // Prefix and copy mode both change what every key does. A sighted user
        // sees the hint panel and the selection; without saying so here, the
        // pane announces the same name in all three states.
        // Untruncated, unlike the tab title beside it. `title(true)` cuts to 25
        // characters so a tab strip can hold several, and a reader is given one
        // pane at a time — two panes running long commands that differ past the
        // cut would otherwise announce identically.
        let announced_title = self.terminal.read(cx).title(false);
        // Copy mode is the same disjunction the key dispatcher uses: vi mode
        // changes what keys do just as much, and announcing only one of the two
        // would be silent in a state where the keyboard behaves differently.
        let in_copy_mode = self.terminal_view.read(cx).copy_mode_state().active
            || self.terminal.read(cx).vi_mode_enabled();
        let mut states: Vec<&str> = Vec::new();
        if self.is_prefix_mode() {
            states.push("prefix mode");
        } else if in_copy_mode {
            states.push("copy mode");
        }
        // Zooming hides every other pane. A sighted user sees that at once; from
        // the tree it is indistinguishable from a window that only ever had one
        // pane. Same word the sidebar uses for it.
        if self.zoomed {
            states.push("zoomed");
        }
        let announced_title = if states.is_empty() {
            announced_title
        } else {
            format!("{announced_title}, {}", states.join(", "))
        };

        div()
            .size_full()
            .relative()
            .id("mux-pane-root")
            .track_focus(&self.focus_handle)
            .role(gpui::Role::Group)
            .aria_label(SharedString::from(announced_title))
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.is_prefix_mode() {
                    // Drop the timeout; machine stays in PrefixWait so handle_key_event
                    // can still resolve the chord. GPUI keymap may also match PrefixMode.
                    this.clear_prefix_timeout();
                    this.dispatch_keystroke(&event.keystroke, window, cx);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                let ime = this.terminal_view.read(cx).is_ime_composing();
                this.dispatch_keystroke(&event.keystroke, window, cx);
                if !ime {
                    cx.stop_propagation();
                }
            }))
            // §12 复制模式搜索指示器 (Plan 31)
            .when_some(
                self.terminal_view
                    .read(cx)
                    .copy_mode_state()
                    .search_indicator(),
                |this, label| {
                    this.child(
                        gpui::deferred(
                            div()
                                .id("mux-copy-mode-search")
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .p(gpui::Rems(0.25))
                                .bg(colors.editor_background)
                                .rounded_sm()
                                .child(
                                    div()
                                        .text_size(gpui::Rems(0.875))
                                        .text_color(colors.text)
                                        .child(label),
                                ),
                        )
                        .with_priority(1),
                    )
                },
            )
            // §3.3 只读指示器 (Plan 33)
            .when(self.is_read_only(), |this| {
                this.child(
                    gpui::deferred(
                        div()
                            .id("mux-read-only-badge")
                            .absolute()
                            .top_0()
                            .right_0()
                            .p(gpui::Rems(0.5))
                            .bg(colors.editor_background)
                            .rounded_sm()
                            .child(
                                div()
                                    .text_size(gpui::Rems(1.))
                                    .text_color(colors.text_muted)
                                    .child("READ-ONLY"),
                            ),
                    )
                    .with_priority(1),
                )
            })
    }
}

impl Item for MuxPaneView {
    type Event = MuxPaneEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.terminal.read(cx).title(true).into()
    }

    fn tab_announcement_text(&self, _detail: usize, cx: &App) -> SharedString {
        // Uncut. `title(true)` stops at 25 characters so a strip can hold
        // several, and a terminal's title is the command it is running —
        // several panes deep into the same build differ well past that.
        self.terminal.read(cx).title(false).into()
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
            // for the MuxPaneEvent directly via cx.subscribe. §16.7
            // ExtensionAction is likewise routed by a direct subscriber.
            MuxPaneEvent::InputFailed { .. } | MuxPaneEvent::ExtensionAction { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualContext as _};
    use mux_protocol::{
        Cell, CellStyle, Envelope, FetchGridUpdateResponse, FetchScrollbackResponse, Request,
        Response, RowChange, envelope::Payload as EnvelopePayload, request::Body as RequestBody,
        response::Body as ResponseBody,
    };
    use settings::SettingsStore;

    #[cfg(unix)]
    fn serve_initial_grid(
        mut stream: std::os::unix::net::UnixStream,
        expected_pane_id: &str,
    ) -> Result<(), String> {
        use std::io::{Read, Write};

        // Short, because the loop below now uses a timeout to mean "the client
        // has stopped asking" rather than "give up". A join therefore costs at
        // most this long after the last request.
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .map_err(|error| format!("set mock mux read timeout: {error}"))?;

        // The client also sends a `ResizePane` once its viewport size is known,
        // and whether that lands before or after the initial fetch depends on
        // how many frames were drawn first — which changes when accessibility
        // is active. These tests are about the fetch, so skip past anything
        // else rather than pinning the wire order.
        // Every fetch is answered, not just the first. The client sends a
        // `ResizePane` once its viewport size is known and can fetch again
        // afterwards, and a server that answered once and exited left that
        // second request hanging — the pane then kept its blank local grid.
        // That is what made two of these tests fail under load and never on
        // their own.
        let mut answered = 0usize;
        'requests: loop {
            let mut prefix = Vec::with_capacity(mux_protocol::MAX_VARINT_LEN);
            loop {
                let mut byte = [0u8; 1];
                match stream.read_exact(&mut byte) {
                    Ok(()) => {}
                    // The client has gone quiet or hung up; either way there is
                    // nothing left to serve.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::UnexpectedEof
                        ) =>
                    {
                        break 'requests;
                    }
                    Err(error) => {
                        return Err(format!("read initial grid request prefix: {error}"));
                    }
                }
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
            let (request_id, fetch) = match request.body {
                Some(RequestBody::FetchGridUpdate(fetch)) => (request.request_id, fetch),
                Some(RequestBody::ResizePane(_)) => continue,
                body => return Err(format!("expected initial FetchGridUpdate, got {body:?}")),
            };

            if fetch.pane_id != expected_pane_id || fetch.since_generation != 0 {
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
                request_id,
                body: Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                    from_generation: 0,
                    to_generation: 7,
                    output_sequence: 0,
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
                        history_size: 0,
                        history_version: 0,
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
                .map_err(|error| format!("flush initial grid response: {error}"))?;
            answered += 1;
        }

        // Timing out with nothing served is still a failure: it means the
        // client never asked, which is the bug this fixture exists to catch.
        if answered == 0 {
            return Err("client never requested the initial grid".to_string());
        }
        Ok(())
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
                    output_sequence: 0,
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
                        history_size: 0,
                        history_version: 0,
                        modes: None,
                    })),
                })),
            })),
        }
    }

    #[cfg(unix)]
    fn history_grid_response(request_id: u64, generation: u64, active: &str) -> Envelope {
        Envelope {
            version: Some(mux_protocol::PROTOCOL_VERSION),
            payload: Some(EnvelopePayload::Response(Response {
                request_id,
                body: Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                    from_generation: 0,
                    to_generation: generation,
                    output_sequence: 0,
                    update: Some(FetchUpdate::FullSnapshot(FullGridSnapshot {
                        cols: 1,
                        rows: 1,
                        cells: vec![Cell {
                            char: active.to_string(),
                            ..Cell::default()
                        }],
                        cursor: Some(mux_protocol::CursorState {
                            col: 0,
                            row: 0,
                            style: 1,
                            visible: true,
                            blinking: false,
                        }),
                        alternate_screen: false,
                        display_offset: 513,
                        history_size: 513,
                        history_version: 42,
                        modes: Some(mux_protocol::terminal_mode::SHOW_CURSOR),
                    })),
                })),
            })),
        }
    }

    #[cfg(unix)]
    fn serve_paged_history(mut stream: std::os::unix::net::UnixStream) -> Result<(), String> {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| format!("set history server read timeout: {error}"))?;

        let request = read_test_envelope(&mut stream, "initial history grid request")?;
        let request = match request.payload {
            Some(EnvelopePayload::Request(request)) => request,
            payload => return Err(format!("expected initial grid request, got {payload:?}")),
        };
        match request.body {
            Some(RequestBody::FetchGridUpdate(fetch))
                if fetch.pane_id == "history-pane" && fetch.since_generation == 0 => {}
            body => return Err(format!("unexpected initial history grid request: {body:?}")),
        }
        write_test_envelope(
            &mut stream,
            &history_grid_response(request.request_id, 5, "X"),
            "initial history grid response",
        )?;

        for (from_line, count) in [(0, 512), (512, 1)] {
            let request = read_test_envelope(&mut stream, "history page request")?;
            let request = match request.payload {
                Some(EnvelopePayload::Request(request)) => request,
                payload => return Err(format!("expected history page request, got {payload:?}")),
            };
            let fetch = match request.body {
                Some(RequestBody::FetchScrollback(fetch)) => fetch,
                body => return Err(format!("expected FetchScrollback, got {body:?}")),
            };
            if fetch.pane_id != "history-pane"
                || fetch.from_line != from_line
                || fetch.direction != 1
                || fetch.count != count
            {
                return Err(format!("unexpected history page request: {fetch:?}"));
            }
            let lines = (from_line..from_line + count)
                .map(|row| {
                    let character = match row {
                        0 => "A",
                        512 => "Z",
                        _ => "M",
                    };
                    history_row(row, &[character])
                })
                .collect();
            write_test_envelope(
                &mut stream,
                &Envelope {
                    version: Some(mux_protocol::PROTOCOL_VERSION),
                    payload: Some(EnvelopePayload::Response(Response {
                        request_id: request.request_id,
                        body: Some(ResponseBody::Scrollback(FetchScrollbackResponse {
                            lines,
                            total_lines: 513,
                            scrollback_version: 42,
                        })),
                    })),
                },
                "history page response",
            )?;
        }

        let checkpoint = read_test_envelope(&mut stream, "history checkpoint request")?;
        let checkpoint = match checkpoint.payload {
            Some(EnvelopePayload::Request(request)) => request,
            payload => return Err(format!("expected history checkpoint request, got {payload:?}")),
        };
        let checkpoint_fetch = match checkpoint.body {
            Some(RequestBody::FetchGridUpdate(fetch)) => fetch,
            body => return Err(format!("expected history checkpoint grid request, got {body:?}")),
        };
        if checkpoint_fetch.pane_id != "history-pane"
            || checkpoint_fetch.since_generation != 5
        {
            return Err(format!(
                "unexpected history checkpoint request: {checkpoint_fetch:?}"
            ));
        }
        write_test_envelope(
            &mut stream,
            &Envelope {
                version: Some(mux_protocol::PROTOCOL_VERSION),
                payload: Some(EnvelopePayload::Response(Response {
                    request_id: checkpoint.request_id,
                    body: Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                        from_generation: 5,
                        to_generation: 5,
                        output_sequence: 0,
                        update: None,
                    })),
                })),
            },
            "history checkpoint response",
        )?;

        let request = read_test_envelope(&mut stream, "cached history grid request")?;
        let request = match request.payload {
            Some(EnvelopePayload::Request(request)) => request,
            payload => return Err(format!("expected cached grid request, got {payload:?}")),
        };
        match request.body {
            Some(RequestBody::FetchGridUpdate(fetch))
                if fetch.pane_id == "history-pane" && fetch.since_generation == 5 => {}
            body => return Err(format!("unexpected cached history grid request: {body:?}")),
        }
        write_test_envelope(
            &mut stream,
            &history_grid_response(request.request_id, 6, "Y"),
            "cached history grid response",
        )
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

        // The viewport-size `ResizePane` may land before the initial fetch
        // depending on how many frames were drawn first, which changes when
        // accessibility is active. This test is about the fetch/dirty race, so
        // skip anything else rather than pinning the wire order.
        let (first_request_id, first_fetch) = loop {
            let first = read_test_envelope(&mut stream, "first grid request")?;
            let first = match first.payload {
                Some(EnvelopePayload::Request(request)) => request,
                payload => return Err(format!("expected first request, got {payload:?}")),
            };
            match first.body {
                Some(RequestBody::FetchGridUpdate(fetch)) => break (first.request_id, fetch),
                Some(RequestBody::ResizePane(_)) => continue,
                body => return Err(format!("expected first grid fetch, got {body:?}")),
            }
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
            &grid_response(first_request_id, 0, 7, 0),
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
    #[test]
    fn prepare_fetch_pages_history_and_reuses_matching_checkpoint() {
        let (client, server) = std::os::unix::net::UnixStream::pair()
            .unwrap_or_else(|error| panic!("create history socket pair: {error}"));
        client
            .set_nonblocking(true)
            .unwrap_or_else(|error| panic!("set history client nonblocking: {error}"));
        let domain = MuxDomain::connect_with_blocking_stream(client)
            .map(Arc::new)
            .unwrap_or_else(|error| panic!("connect history mux domain: {error}"));
        let server_thread = std::thread::spawn(move || serve_paged_history(server));
        let initial_snapshot = history_snapshot(1, 0, 0);
        let initial_cache = HistoryCache {
            cols: 1,
            history_size: 0,
            history_version: 0,
            cells: Arc::new(Vec::new()),
        };

        let first = futures::executor::block_on(prepare_fetch_update(
            &domain,
            "history-pane",
            0,
            initial_snapshot,
            initial_cache,
        ))
        .unwrap_or_else(|error| panic!("prepare paged history update: {error}"));
        let (snapshot, history_cache) = match first {
            PreparedFetchUpdate::Snapshot {
                expected_generation,
                generation,
                snapshot,
                history_cache,
                structured,
                ..
            } => {
                assert_eq!(expected_generation, 0);
                assert_eq!(generation, 5);
                assert_eq!(structured.history.len(), 513);
                assert_eq!(structured.display_offset, 513);
                assert_eq!(structured.history[0].character, 'A');
                assert_eq!(structured.history[512].character, 'Z');
                assert_eq!(structured.cells[0].character, 'X');
                (snapshot, history_cache)
            }
            update => panic!("expected prepared snapshot, got {update:?}"),
        };

        let second = futures::executor::block_on(prepare_fetch_update(
            &domain,
            "history-pane",
            5,
            snapshot,
            history_cache,
        ))
        .unwrap_or_else(|error| panic!("prepare cached history update: {error}"));
        match second {
            PreparedFetchUpdate::Snapshot {
                expected_generation,
                generation,
                history_cache,
                structured,
                ..
            } => {
                assert_eq!(expected_generation, 5);
                assert_eq!(generation, 6);
                assert_eq!(history_cache.history_version, 42);
                assert_eq!(structured.history.len(), 513);
                assert_eq!(structured.history[0].character, 'A');
                assert_eq!(structured.history[512].character, 'Z');
                assert_eq!(structured.cells[0].character, 'Y');
            }
            update => panic!("expected cached prepared snapshot, got {update:?}"),
        }

        match server_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("paged history server failed: {error}"),
            Err(_) => panic!("paged history server panicked"),
        }
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
        let server_thread = std::thread::spawn(move || serve_initial_grid(server, "quiet-pane"));

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

    /// Prefix and copy mode change what every key does. A sighted user sees the
    /// hint panel or the selection; without saying so in the pane's name, the
    /// pane announces identically in all three states.
    #[cfg(unix)]
    #[gpui::test]
    async fn prefix_mode_changes_what_the_pane_announces(cx: &mut TestAppContext) {
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
            Ok(domain) => std::sync::Arc::new(domain),
            Err(error) => panic!("connect mock mux domain: {error}"),
        };
        let server_thread = std::thread::spawn(move || serve_initial_grid(server, "quiet-pane"));

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
        // Pumped before the join: the mock server blocks reading with a
        // timeout, and the request it is waiting for is sent by a task on this
        // thread. Joining first means the timeout races the scheduler, which is
        // why this passed alone and failed under load.
        cx.run_until_parked();
        match server_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("mock mux server failed: {error}"),
            Err(_) => panic!("mock mux server panicked"),
        }
        cx.run_until_parked();

        cx.activate_a11y(cx.window_handle());
        let pane_label = |cx: &mut gpui::VisualTestContext| {
            let json = cx
                .update(|window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "mux pane");
            gpui::a11y_checks::assert_names_are_distinguishable(&tree, "mux pane");
            gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "mux pane");
            gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "mux pane");
            gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "mux pane");
            gpui::a11y_checks::assert_controls_have_area(&tree, "mux pane");
            gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "mux pane");
            gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "mux pane");
            gpui::a11y_checks::assert_no_role_was_discarded(&tree, "mux pane");
            gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "mux pane");
            gpui::a11y_checks::assert_roles_are_contained(&tree, "mux pane");
            tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .find(|node| node["element_id"].as_str() == Some("Name(\"mux-pane-root\")"))
                .and_then(|node| node["aria"]["label"].as_str().map(str::to_string))
        };

        let plain = pane_label(cx).expect("the pane root is named");
        assert!(
            !plain.contains("prefix mode"),
            "an idle pane must not claim a mode: {plain}"
        );

        // Entered directly rather than through `enter_prefix_mode`, which first
        // asks whether a full-screen application owns the keyboard and passes
        // the prefix key through when one does. The snapshot this mock server
        // sends sets `alternate_screen`, so once it has been applied the pane
        // is running a full-screen application and the mode correctly never
        // engages. What this test is about is what the pane announces for a
        // state, so it sets the state.
        //
        // Nothing here waits for that snapshot, and the assertion below is
        // written so that it does not care whether it has arrived. An earlier
        // version asserted the fixture was full-screen first, which made the
        // test depend on bytes having crossed a real socket by a particular
        // moment; it failed about one run in three under a parallel suite and
        // never once on its own.
        view.update_in(cx, |view, _window, cx| {
            view.prefix_machine = PrefixModeMachine::new(PrefixModeConfig {
                timeout_ms: 5_000,
                full_screen_passthrough: false,
            });
            view.prefix_machine.on_prefix_key();
            cx.notify();
        });
        // Deliberately not pumped: prefix mode arms a timeout that leaves it
        // again, and the test executor advances timers when it would otherwise
        // park. Pumping here raced that timeout — under load, something else
        // was still runnable and the timer did not fire; idle, it did not
        // matter either way. The state is set synchronously and `pane_label`
        // draws, so there is nothing to wait for.
        let announced = pane_label(cx).expect("the pane root is still named");
        // A suffix rather than an equality against the earlier label: the base
        // name is the terminal's title, which the mock server's snapshot can
        // change at any point, and this test is about the mode rather than the
        // title.
        assert!(
            announced.ends_with(", prefix mode"),
            "entering prefix mode has to change what the pane announces: {announced}"
        );

        // Copy mode is the other state where the keyboard behaves differently,
        // and it is reached from a different code path, so it needs its own
        // check rather than being assumed from the prefix case.
        view.update_in(cx, |view, _window, cx| {
            // Leave prefix mode the way a timeout would, so the next assertion
            // is about copy mode rather than a leftover prefix.
            view.prefix_machine.on_timeout();
            view.terminal_view.update(cx, |terminal_view, cx| {
                terminal_view.enter_copy_mode_for_test(cx);
            });
        });
        cx.run_until_parked();

        assert_eq!(
            pane_label(cx).as_deref(),
            Some(format!("{plain}, copy mode").as_str()),
            "entering copy mode has to change what the pane announces"
        );

        // Zooming is orthogonal to the keyboard modes and hides every other
        // pane, so it has to be announced alongside whichever mode is active
        // rather than replacing it.
        view.update_in(cx, |view, _window, cx| {
            view.set_zoomed_from_snapshot(true, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            pane_label(cx).as_deref(),
            Some(format!("{plain}, copy mode, zoomed").as_str()),
            "a zoomed pane looks nothing like an unzoomed one and has to say so"
        );
    }

    /// Copy mode exists so the user can select terminal output. A collapsed
    /// caret is checked elsewhere; a real selection takes a different path,
    /// and it is the one that makes copy mode worth entering with a screen
    /// reader.
    #[cfg(unix)]
    #[gpui::test]
    async fn a_terminal_selection_is_reported_as_a_text_range(cx: &mut TestAppContext) {
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
            Ok(domain) => std::sync::Arc::new(domain),
            Err(error) => panic!("connect mock mux domain: {error}"),
        };
        let server_thread = std::thread::spawn(move || serve_initial_grid(server, "quiet-pane"));

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
        // Pumped before the join: the mock server blocks reading with a
        // timeout, and the request it is waiting for is sent by a task on this
        // thread. Joining first means the timeout races the scheduler, which is
        // why this passed alone and failed under load.
        cx.run_until_parked();
        // Joined at the end rather than here. The server now answers every
        // fetch until the client goes quiet, and the client can fetch again
        // after the resize that follows its first frames — so joining before
        // the assertions would kill it while it still had work to do.
        cx.run_until_parked();

        cx.activate_a11y(cx.window_handle());
        let selection = |cx: &mut gpui::VisualTestContext| {
            let json = cx
                .update(|window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "terminal selection");
            gpui::a11y_checks::assert_no_role_was_discarded(&tree, "terminal selection");
            gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "terminal selection");
            gpui::a11y_checks::assert_roles_are_contained(&tree, "terminal selection");
            gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "terminal selection");
            gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "terminal selection");
            gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "terminal selection");
            gpui::a11y_checks::assert_names_are_distinguishable(&tree, "terminal selection");
            gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "terminal selection");
            gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "terminal selection");
            gpui::a11y_checks::assert_controls_have_area(&tree, "terminal selection");
            gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "terminal selection");
            tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .find(|node| node["aria"]["role"] == "Terminal")
                .and_then(|node| node["aria"]["text_selection"].as_object().cloned())
        };

        // The grid arrives over the socket after the pane is mounted, so the
        // first frames legitimately have no content and no caret in them.
        //
        // A real sleep, which is not the usual advice. `MuxDomain` reads the
        // socket on an OS thread of its own, so what this waits for is not
        // scheduled by GPUI at all: `run_until_parked` returns the moment
        // nothing is runnable and an executor timer only advances the test
        // clock, neither of which gives that thread wall-clock time to make
        // progress. `allow_parking` is on above for exactly this reason.
        let mut caret = None;
        for _ in 0..500 {
            caret = selection(cx);
            if caret.is_some() {
                break;
            }
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let caret = caret.expect("the terminal reports a caret once its grid has arrived");
        assert_eq!(
            caret.get("anchor"),
            caret.get("focus"),
            "with nothing selected the caret is collapsed: {caret:?}"
        );

        view.update(cx, |view, cx| {
            view.terminal.update(cx, |terminal, _| terminal.select_all());
        });
        cx.run_until_parked();

        let range = selection(cx).expect("the terminal still reports a selection");
        assert_ne!(
            range.get("anchor"),
            range.get("focus"),
            "a selection has to be reported as a range, not as a caret: {range:?}"
        );

        match server_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("mock mux server failed: {error}"),
            Err(_) => panic!("mock mux server panicked"),
        }
    }

    /// §15.4 After a reconnect resync, the server-authoritative title/zoom
    /// metadata must land in the view without re-issuing RPCs.
    #[cfg(unix)]
    #[gpui::test]
    async fn reconcile_metadata_from_snapshot_updates_title_and_zoom(cx: &mut TestAppContext) {
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
        let server_thread = std::thread::spawn(move || serve_initial_grid(server, "quiet-pane"));

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

        // §15.4 title + zoom arrive from the authoritative snapshot.
        view.update(cx, |view, cx| {
            view.reconcile_metadata_from_snapshot(Some("vim"), Some(true), cx);
        });
        view.read_with(cx, |view, cx| {
            assert!(view.is_zoomed(), "zoom must be mirrored from snapshot");
            assert_eq!(view.title(cx), "vim");
        });

        // A pane the snapshot no longer marks zoomed is unzoomed locally.
        view.update(cx, |view, cx| {
            view.reconcile_metadata_from_snapshot(None, Some(false), cx);
        });
        view.read_with(cx, |view, cx| {
            assert!(!view.is_zoomed());
        });
    }

    /// §16.7: a pane with an installed extension shortcut resolver matches a
    /// bound chord (normalized to gpui's hyphen form) in the priority chain
    /// and emits `MuxPaneEvent::ExtensionAction` instead of sending the key
    /// to the PTY; an unbound chord never produces an extension action.
    #[cfg(unix)]
    #[gpui::test]
    async fn extension_shortcut_resolver_emits_extension_action_for_bound_chord(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (client, server) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => panic!("create shortcut socket pair: {error}"),
        };
        if let Err(error) = client.set_nonblocking(true) {
            panic!("set shortcut client nonblocking: {error}");
        }
        let domain = match MuxDomain::connect_with_blocking_stream(client) {
            Ok(domain) => Arc::new(domain),
            Err(error) => panic!("connect shortcut mux domain: {error}"),
        };
        let server_thread = std::thread::spawn(move || serve_initial_grid(server, "quiet-pane"));

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
            Ok(Err(error)) => panic!("shortcut mock mux server failed: {error}"),
            Err(_) => panic!("shortcut mock mux server panicked"),
        }
        initial_grid_applied.await;

        // Install a snapshot-backed resolver shaped like the extension host's.
        let bindings = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::BTreeMap::from([(
                "ctrl-shift-p".to_string(),
                "z3rm.command-palette.open".to_string(),
            )]),
        ));
        view.update(cx, |view, _cx| {
            view.set_extension_shortcut_resolver(Some(std::sync::Arc::new(
                move |keystroke: &Keystroke| {
                    let matched = bindings
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(chord, _)| {
                            Keystroke::parse(chord.as_str())
                                .map(|parsed| parsed == *keystroke)
                                .unwrap_or(false)
                        })
                        .map(|(_, action)| SharedString::from(action.clone()));
                    matched
                },
            )));
        });

        // Bound chord: the priority chain routes it to an extension action.
        let extension_action = view.next_event::<MuxPaneEvent>(cx);
        cx.update_window_entity(&view, |view, window, cx| {
            let keystroke = Keystroke::parse("ctrl-shift-p").expect("parse bound chord");
            view.dispatch_keystroke(&keystroke, window, cx);
        });
        let event = extension_action.await;
        assert_eq!(
            event,
            MuxPaneEvent::ExtensionAction {
                action_id: SharedString::from("z3rm.command-palette.open"),
            },
            "a bound extension shortcut must surface as an ExtensionAction event"
        );

        // Unbound chord: never an extension action (the key takes the normal
        // PTY path, so only assert no extension event is queued).
        view.update(cx, |view, _cx| {
            assert!(
                view.extension_shortcuts.is_some(),
                "the resolver stays installed"
            );
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
        let first_fetch_deadline = web_time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            while cx.executor().tick() {}
            if first_fetch.try_recv().is_ok() {
                break;
            }
            assert!(
                web_time::Instant::now() < first_fetch_deadline,
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
        let catch_up_deadline = web_time::Instant::now() + std::time::Duration::from_secs(5);
        let catch_up_state = loop {
            while cx.executor().tick() {}
            let state = view.read_with(cx, |view, _cx| {
                (view.generation, view.fetch_in_flight, view.fetch_pending)
            });
            if state == (8, false, false) || web_time::Instant::now() >= catch_up_deadline {
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

    fn history_snapshot(cols: u32, rows: u32, version: u64) -> FullGridSnapshot {
        FullGridSnapshot {
            cols,
            rows: 1,
            cells: vec![Cell::default(); cols as usize],
            cursor: None,
            alternate_screen: false,
            display_offset: rows,
            history_size: rows,
            history_version: version,
            modes: None,
        }
    }
    #[test]
    fn snapshot_metadata_rejects_wrong_cell_count_and_offset() {
        let mut snapshot = FullGridSnapshot {
            cols: 2,
            rows: 2,
            cells: vec![Cell::default(); 3],
            cursor: None,
            alternate_screen: false,
            display_offset: 0,
            history_size: 0,
            history_version: 1,
            modes: None,
        };
        assert!(validate_snapshot_metadata(&snapshot).is_err());

        snapshot.cells = vec![Cell::default(); 4];
        snapshot.display_offset = 1;
        assert!(validate_snapshot_metadata(&snapshot).is_err());
    }

    #[test]
    fn snapshot_metadata_rejects_history_cell_budget_overflow() {
        let cols = mux_protocol::MAX_GRID_COLUMNS;
        let history_size = MAX_SCROLLBACK_CELLS / cols as usize + 1;
        let snapshot = FullGridSnapshot {
            cols: cols as u32,
            rows: 1,
            cells: vec![Cell::default(); cols as usize],
            cursor: None,
            alternate_screen: false,
            display_offset: 0,
            history_size: history_size as u32,
            history_version: 1,
            modes: None,
        };
        assert!(validate_snapshot_metadata(&snapshot).is_err());
    }

    fn history_row(row: u32, chars: &[&str]) -> RowChange {
        RowChange {
            row,
            cells: chars
                .iter()
                .map(|character| Cell {
                    char: (*character).to_string(),
                    ..Cell::default()
                })
                .collect(),
        }
    }

    #[test]
    fn paged_history_validates_and_preserves_oldest_first_order() {
        let snapshot = history_snapshot(2, 3, 9);
        let mut accumulator = HistoryPageAccumulator::new(&snapshot)
            .unwrap_or_else(|error| panic!("create history accumulator: {error}"));
        let first_done = accumulator
            .push(
                FetchScrollbackResponse {
                    lines: vec![history_row(0, &["A", "a"]), history_row(1, &["B", "b"])],
                    total_lines: 3,
                    scrollback_version: 9,
                },
                2,
            )
            .unwrap_or_else(|error| panic!("append first history page: {error}"));
        assert!(!first_done);
        let second_done = accumulator
            .push(
                FetchScrollbackResponse {
                    lines: vec![history_row(2, &["C", "c"])],
                    total_lines: 3,
                    scrollback_version: 9,
                },
                1,
            )
            .unwrap_or_else(|error| panic!("append second history page: {error}"));
        assert!(second_done);
        let cache = accumulator
            .finish()
            .unwrap_or_else(|error| panic!("finish history pages: {error}"));

        assert_eq!(cache.history_size, 3);
        assert_eq!(
            cache
                .cells
                .iter()
                .map(|cell| cell.character)
                .collect::<Vec<_>>(),
            vec!['A', 'a', 'B', 'b', 'C', 'c']
        );
    }
    #[test]
    fn paged_history_rejects_short_pages() {
        let snapshot = history_snapshot(1, 2, 7);
        let mut accumulator = HistoryPageAccumulator::new(&snapshot)
            .unwrap_or_else(|error| panic!("create history accumulator: {error}"));
        assert!(
            accumulator
                .push(
                    FetchScrollbackResponse {
                        lines: vec![history_row(0, &["A"])],
                        total_lines: 2,
                        scrollback_version: 7,
                    },
                    2,
                )
                .is_err()
        );
        assert_eq!(accumulator.next_row, 0);
        assert!(accumulator.cells.is_empty());
    }

    #[test]
    fn paged_history_rejects_mixed_or_malformed_checkpoints() {
        let snapshot = history_snapshot(2, 2, 7);
        let invalid_pages = [
            FetchScrollbackResponse {
                lines: vec![history_row(0, &["A", "a"])],
                total_lines: 2,
                scrollback_version: 8,
            },
            FetchScrollbackResponse {
                lines: vec![history_row(1, &["A", "a"])],
                total_lines: 2,
                scrollback_version: 7,
            },
            FetchScrollbackResponse {
                lines: vec![history_row(0, &["A"])],
                total_lines: 2,
                scrollback_version: 7,
            },
        ];
        for page in invalid_pages {
            let mut accumulator = HistoryPageAccumulator::new(&snapshot)
                .unwrap_or_else(|error| panic!("create history accumulator: {error}"));
            assert!(accumulator.push(page, 1).is_err());
            assert_eq!(accumulator.next_row, 0);
        }
    }

    #[test]
    fn paged_history_rejects_more_rows_than_requested() {
        let snapshot = history_snapshot(1, 2, 7);
        let mut accumulator = HistoryPageAccumulator::new(&snapshot)
            .unwrap_or_else(|error| panic!("create history accumulator: {error}"));
        assert!(
            accumulator
                .push(
                    FetchScrollbackResponse {
                        lines: vec![history_row(0, &["A"]), history_row(1, &["B"])],
                        total_lines: 2,
                        scrollback_version: 7,
                    },
                    1,
                )
                .is_err()
        );
        assert_eq!(accumulator.next_row, 0);
        assert!(accumulator.cells.is_empty());
    }

    #[test]
    fn matching_history_cache_is_reused_only_for_exact_checkpoint() {
        let snapshot = history_snapshot(2, 2, 7);
        let cache = HistoryCache {
            cols: 2,
            history_size: 2,
            history_version: 7,
            cells: Arc::new(vec![StructuredTerminalCell::default(); 4]),
        };
        assert!(matching_history_cache(&snapshot, Some(&cache)).is_some());

        let mut changed = snapshot.clone();
        changed.history_version = 8;
        assert!(matching_history_cache(&changed, Some(&cache)).is_none());
        changed = snapshot.clone();
        changed.history_size = 1;
        assert!(matching_history_cache(&changed, Some(&cache)).is_none());
        changed = snapshot;
        changed.cols = 3;

        assert!(matching_history_cache(&changed, Some(&cache)).is_none());
    }

    #[test]
    fn prepared_update_generation_gate_rejects_before_commit() {
        assert!(validate_prepared_generation(7, 7).is_ok());
        assert!(validate_prepared_generation(7, 6).is_err());
        assert!(validate_prepared_generation(7, 8).is_err());
    }

    #[test]
    fn history_pages_respect_shared_grid_cell_limit() {
        assert_eq!(history_page_rows(1), HISTORY_PAGE_ROWS);
        assert_eq!(history_page_rows(4_096), 256);
        assert!(history_page_rows(4_096) as usize * 4_096 <= mux_protocol::MAX_GRID_CELLS);
    }

    #[test]
    fn diff_generation_must_continue_from_client_checkpoint() {
        let valid = FetchGridUpdateResponse {
            from_generation: 5,
            to_generation: 6,
            output_sequence: 0,
            update: Some(FetchUpdate::Diff(GridDiff::default())),
        };
        assert!(validate_generation_envelope(5, &valid).is_ok());
        let no_advance = FetchGridUpdateResponse {
            to_generation: 5,
            ..valid.clone()
        };
        assert!(validate_generation_envelope(5, &no_advance).is_err());

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
            output_sequence: 0,
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
            output_sequence: 0,
            update: Some(FetchUpdate::FullSnapshot(FullGridSnapshot {
                cols: 1,
                rows: 1,
                cells: vec![Cell::default()],
                cursor: None,
                alternate_screen: false,
                display_offset: 0,
                history_size: 0,
                history_version: 0,
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
            history_size: 0,
            history_version: 0,
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
            history_size: 0,
            history_version: 0,
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

    /// A terminal in the real window lives inside the workspace pane group,
    /// which is a cached view. Every mux pane test above renders the view in a
    /// window of its own, so none of them exercise the path the product takes —
    /// the one where a cached subtree that stopped prepainting would drop out
    /// of the tree entirely.
    #[cfg(unix)]
    #[gpui::test]
    async fn a_terminal_in_a_workspace_pane_is_announced_on_every_frame(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let params = cx.update(workspace::AppState::test);
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let project = project::Project::test(params.fs.clone(), [], cx).await;
        let window_handle = cx
            .add_window(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let cx = &mut gpui::VisualTestContext::from_window(window_handle.into(), cx);

        // Started only once the workspace exists: the mock server reads with a
        // timeout, and building a project and a window takes longer than it.
        let (client, server) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => panic!("create mock mux socket pair: {error}"),
        };
        if let Err(error) = client.set_nonblocking(true) {
            panic!("set mock mux client nonblocking: {error}");
        }
        let domain = match MuxDomain::connect_with_blocking_stream(client) {
            Ok(domain) => std::sync::Arc::new(domain),
            Err(error) => panic!("connect mock mux domain: {error}"),
        };
        let server_thread = std::thread::spawn(move || serve_initial_grid(server, "hosted-pane"));

        let pane_view = cx.update(|window, cx| {
            cx.new(|cx| {
                MuxPaneView::new(
                    "hosted-pane".to_string(),
                    domain,
                    WeakEntity::new_invalid(),
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            })
        });
        // The pane only asks for its grid once it is on screen, so the mock
        // server is joined after the item has been mounted and drawn.
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(pane_view), true, true, None, window, cx);
        });
        cx.run_until_parked();
        match server_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("mock mux server failed: {error}"),
            Err(_) => panic!("mock mux server panicked"),
        }
        cx.run_until_parked();

        // Focused so the tree check below is about a focused pane rather than
        // an idle one: focus is where a reader starts.
        pane.update_in(cx, |pane, window, cx| {
            pane.focus_active_item(window, cx);
        });
        cx.run_until_parked();

        cx.activate_a11y(cx.window_handle());

        // The grid arrives over the socket after the pane is mounted, so the
        // first frames legitimately have nothing to say. Once the text is
        // there it must stay there: the pane group is a cached view, and a
        // cached subtree that stopped prepainting would drop out of the tree.
        let read_frame = |cx: &mut gpui::VisualTestContext| {
            let json = cx
                .update(|window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            let nodes = tree["nodes"].as_object().expect("the dump lists nodes");
            // The surface has to say it can take focus, not merely accept it
            // when GPUI hands it over: `Action::Focus` is how assistive
            // technology knows it may ask, and `Interactivity` writes it from a
            // method this element overrides without delegating.
            let focused = tree["gpui_focus"]
                .as_str()
                .and_then(|id| nodes.get(id))
                .unwrap_or_else(|| panic!("the terminal holds focus: {json}"));
            assert!(
                focused["aria"]["on_action"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|action| action == "Focus"),
                "the focused surface has to advertise that it takes focus: {focused}"
            );
            gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "hosted terminal");
            let named = nodes
                .values()
                .find(|node| node["element_id"].as_str() == Some("Name(\"mux-pane-root\")"))
                .and_then(|node| node["aria"]["label"].as_str())
                .unwrap_or_default()
                .to_string();
            let text_runs: Vec<String> = nodes
                .values()
                .filter(|node| node["aria"]["role"] == "TextRun")
                .filter_map(|node| node["aria"]["value"].as_str().map(str::to_string))
                .collect();
            (named, text_runs)
        };

        // The first cell carries a combining acute accent, so the plain word is
        // not a substring of what the terminal reports.
        const SERVED_GRID_TEXT: &str = "q\u{301}uiet";

        let mut settled = None;
        for _ in 0..20 {
            let frame = read_frame(cx);
            if frame.1.iter().any(|value| value.contains(SERVED_GRID_TEXT)) {
                settled = Some(frame);
                break;
            }
            cx.run_until_parked();
        }
        let (named, text_runs) =
            settled.expect("the served grid text never reached the accessibility tree");
        assert!(!named.is_empty(), "the hosted terminal must be announced");

        for frame in 1..=2 {
            let (named, text_runs_again) = read_frame(cx);
            assert!(
                !named.is_empty(),
                "the pane lost its name on redraw {frame}"
            );
            assert_eq!(
                text_runs_again, text_runs,
                "the grid stopped reaching the tree on redraw {frame}"
            );
        }
    }
}
