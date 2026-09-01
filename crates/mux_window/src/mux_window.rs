//! The mux window layer both z3rm surfaces share.
//!
//! The desktop binary and the WebAssembly client render the same workspace,
//! speak the same mux protocol, and register the same pane action handlers.
//! Everything here is surface-agnostic: the daemon, SSH tunnels, the QuickJS
//! extension host and the file-transfer flows stay in the desktop binary and
//! reach this layer through [`MuxWindowHooks`].

gpui::actions!(z3rm_debug, [DumpAccessibilityTree]);

mod rename_session_modal;
pub use rename_session_modal::RenameSessionModal;

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Context as _;

use gpui::{App, AppContext as _, BorrowAppContext as _, Context, Entity, Global, IntoElement, Render, TaskExt as _, WeakEntity, Window};
use mux_protocol::SessionSnapshot;
use util::ResultExt as _;
use workspace::layout_projection::MuxSnapshot;


/// Surface-provided callbacks the shared layer uses instead of reaching into
/// desktop-only crates (daemon toasts, the QuickJS extension host).
#[derive(Clone, Copy)]
pub struct MuxWindowHooks {
    /// Surface an infrastructure error to the user (desktop: daemon error
    /// toast; browser: log + status surface).
    pub show_error: fn(&mut App, String),
    /// Resolve a QuickJS extension shortcut resolver for a new pane, or `None`
    /// when the surface has no extension host (the browser).
    pub extension_shortcut_resolver:
        fn(&App) -> Option<terminal_view::mux_pane::ExtensionShortcutResolver>,
    /// Dispatch one `MuxPaneEvent::ExtensionAction` id to the surface's
    /// extension host (desktop: QuickJS command registry; browser: no-op).
    pub route_extension_action: fn(&App, &str),
}

impl MuxWindowHooks {
    /// A surface with no extension host that only logs infrastructure errors.
    pub const NOOP: Self = Self {
        show_error: |cx, message| {
            tracing::error!(message, "mux window error");
            let _ = cx;
        },
        extension_shortcut_resolver: |_| None,
        route_extension_action: |_, _| {},
    };
}

/// The surface installs its hooks once at boot; the shared layer reads them
/// from `cx` so long-lived GPUI closures never capture per-call state.
struct GlobalMuxWindowHooks(MuxWindowHooks);

impl Global for GlobalMuxWindowHooks {}

pub fn install_hooks(cx: &mut App, hooks: MuxWindowHooks) {
    cx.set_global(GlobalMuxWindowHooks(hooks));
}

fn hooks(cx: &App) -> MuxWindowHooks {
    cx.try_global::<GlobalMuxWindowHooks>()
        .map(|hooks| hooks.0)
        .unwrap_or(MuxWindowHooks::NOOP)
}

pub fn focus_mux_workspace_pane(
    pane: Entity<workspace::Pane>,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    let Some(item) = pane.read(cx).active_item() else {
        return;
    };
    let Ok(mux_view) = item
        .to_any_view()
        .downcast::<terminal_view::mux_pane::MuxPaneView>()
    else {
        return;
    };
    let pane_id = mux_view.read(cx).pane_id.clone();
    let focus_handle = item.item_focus_handle(cx);
    window.focus(&focus_handle, cx);

    let Some(domain) = mux_domain_for_window(window, cx) else {
        return;
    };
    cx.spawn(async move |_, cx| {
        if let Err(error) = domain.focus_pane(&pane_id).await {
            tracing::error!(pane_id, %error, "focus_pane RPC failed");
            cx.update(|cx| {
                (hooks(cx).show_error)(
                    cx,
                    format!("Failed to focus mux pane {pane_id}: {error}"),
                );
            });
        }
    })
    .detach();
}

pub fn focus_mux_pane_index(
    workspace: &mut workspace::Workspace,
    index: u8,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    if let Some(pane) = workspace.panes().get(index as usize).cloned() {
        focus_mux_workspace_pane(pane, window, cx);
    }
}

/// §15.7 Focus the GPUI pane projecting `pane_id`, for callers that only know
/// the server-side pane id (the session sidebar).
pub fn focus_mux_pane_by_id(
    workspace: &mut workspace::Workspace,
    pane_id: &str,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    let located = workspace.panes().iter().find_map(|pane| {
        let item_index = pane.read(cx).items().position(|item| {
            item.to_any_view()
                .downcast::<terminal_view::mux_pane::MuxPaneView>()
                .is_ok_and(|view| view.read(cx).pane_id == pane_id)
        })?;
        Some((pane.clone(), item_index))
    });
    let Some((pane, item_index)) = located else {
        // The pane belongs to a tab this window does not project; the server
        // stays authoritative and there is nothing local to focus.
        return;
    };
    // Activating first makes the pane's active item the one we mean, so the
    // shared focus helper sends `focus_pane` for the requested id.
    pane.update(cx, |pane, cx| {
        pane.activate_item(item_index, true, true, window, cx);
    });
    focus_mux_workspace_pane(pane, window, cx);
}

pub fn cyclic_pane_index(current: usize, pane_count: usize, forward: bool) -> Option<usize> {
    if pane_count == 0 || current >= pane_count {
        return None;
    }
    Some(if forward {
        (current + 1) % pane_count
    } else if current == 0 {
        pane_count - 1
    } else {
        current - 1
    })
}

pub fn focus_adjacent_mux_pane(
    workspace: &mut workspace::Workspace,
    forward: bool,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    let panes = workspace.panes();
    let Some(current) = panes
        .iter()
        .position(|pane| pane == workspace.active_pane())
    else {
        return;
    };
    let Some(index) = cyclic_pane_index(current, panes.len(), forward) else {
        return;
    };
    focus_mux_workspace_pane(panes[index].clone(), window, cx);
}

/// absent and the pane behaves exactly as it did before — native core
/// commands never depend on the extension host.
pub fn new_mux_pane_view(
    pane_id: String,
    domain: Arc<mux::MuxDomain>,
    workspace: WeakEntity<workspace::Workspace>,
    project: WeakEntity<project::Project>,
    window: &mut Window,
    cx: &mut Context<terminal_view::mux_pane::MuxPaneView>,
) -> terminal_view::mux_pane::MuxPaneView {
    let mut view = terminal_view::mux_pane::MuxPaneView::new(
        pane_id,
        domain,
        workspace,
        project,
        window,
        cx,
    );
    if let Some(resolver) = (hooks(cx).extension_shortcut_resolver)(cx) {
        view.set_extension_shortcut_resolver(Some(resolver));
    }
    view
}

/// §16.7 Route every `MuxPaneEvent::ExtensionAction` the pane emits to the
/// extension host, which dispatches it to the owning extension through the
/// command registry. A pane that never matches an extension shortcut emits
/// nothing; without a host the route is a logged no-op.
pub fn subscribe_mux_pane_extension_actions(
    view: &Entity<terminal_view::mux_pane::MuxPaneView>,
    cx: &mut App,
) {
    cx.subscribe(view, |_, event, cx| {
        if let terminal_view::mux_pane::MuxPaneEvent::ExtensionAction { action_id } = event {
            (hooks(cx).route_extension_action)(cx, action_id.as_ref());
        }
    })
    .detach();
}

pub fn apply_mux_layout_to_workspace(
    workspace: &mut workspace::Workspace,
    layout: &workspace::layout_projection::LayoutTree,
    focused_pane_id: Option<&str>,
    domain: Arc<mux::MuxDomain>,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    let mut existing: std::collections::HashMap<String, Entity<workspace::Pane>> =
        std::collections::HashMap::default();
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            if let Ok(view) = item
                .to_any_view()
                .downcast::<terminal_view::mux_pane::MuxPaneView>()
            {
                let pane_id = view.read(cx).pane_id.clone();
                existing.entry(pane_id).or_insert_with(|| pane.clone());
            }
        }
    }
    workspace.apply_layout_snapshot(
        layout,
        focused_pane_id,
        existing,
        |workspace, window, cx| workspace.add_pane_for_layout(window, cx),
        |workspace, pane, pane_id, window, cx| {
            let view = cx.new(|cx| {
                new_mux_pane_view(
                    pane_id,
                    domain.clone(),
                    workspace.weak_handle(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            subscribe_mux_pane_extension_actions(&view, cx);
            let item: Box<dyn workspace::ItemHandle> = Box::new(view);
            workspace.add_item(pane.clone(), item, None, true, true, window, cx);
        },
        window,
        cx,
    );
}


#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MuxConnectionState {
    #[default]
    Connected,
    Disconnected,
    Reconnecting,
}

impl MuxConnectionState {
    fn begin_reconnect(&mut self) -> bool {
        if *self == Self::Reconnecting {
            return false;
        }
        *self = Self::Reconnecting;
        true
    }

    fn finish_reconnect(&mut self, succeeded: bool) {
        *self = if succeeded {
            Self::Connected
        } else {
            Self::Disconnected
        };
    }

    fn mark_disconnected(&mut self) -> bool {
        if *self == Self::Disconnected {
            return false;
        }
        *self = Self::Disconnected;
        true
    }
}

/// §3.3 One GPUI window's mux binding.
///
/// A window owns its own `MuxDomain`, i.e. its own socket, client identity and
/// server-minted window id. That is what makes window teardown precise: closing
/// the window closes exactly one connection, and the server releases exactly
/// that window's session membership — including when the process crashes.
///
/// §16.6 For a remote (`attach --ssh`) window, `ssh_session` additionally
/// owns the SSH ControlMaster + socket forward: it must stay alive as long as
/// the window renders, so it lives here and dies only when the binding is
/// removed. The type parameter exists so the carry-over contract (rebinding a
/// window must not drop the session) is testable without a live tunnel.
#[cfg(feature = "ssh")]
pub type DefaultWindowHeld = mux::SshSession;
#[cfg(not(feature = "ssh"))]
pub type DefaultWindowHeld = ();

pub struct MuxWindow<T = DefaultWindowHeld> {
    pub domain: Arc<mux::MuxDomain>,
    pub session_id: String,
    pub ssh_session: Option<Arc<futures::lock::Mutex<T>>>,
    pub connection_state: MuxConnectionState,
}

/// §15.4 The window's persistent connection indicator.
///
/// A toast is the wrong surface for this: it tells the user once and then the
/// window looks identical to a healthy one. An offline remote window has to
/// keep saying so until it is reconnected.
pub struct MuxConnectionStatusItem {
    state: MuxConnectionState,
}

impl MuxConnectionStatusItem {
    pub fn new() -> Self {
        Self {
            state: MuxConnectionState::Connected,
        }
    }

    fn set_state(&mut self, state: MuxConnectionState, cx: &mut Context<Self>) {
        if self.state == state {
            return;
        }
        self.state = state;
        cx.notify();
    }
}

impl Render for MuxConnectionStatusItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        use ui::prelude::*;

        // Recovery is announced but not drawn. The status bar goes back to
        // showing nothing once the connection returns, which is right for a
        // user who can see the pane responding again and wrong for one who was
        // told the connection dropped and is never told otherwise — clearing
        // the announcement leaves them believing they are still detached.
        let (state_text, visible_color) = match self.state {
            MuxConnectionState::Connected => ("Connected", None),
            MuxConnectionState::Disconnected => ("Disconnected", Some(ui::Color::Error)),
            MuxConnectionState::Reconnecting => ("Reconnecting…", Some(ui::Color::Warning)),
        };
        // Losing the connection is conveyed only by this text and its color, and
        // it happens while the user is working somewhere else entirely. A polite
        // live region is what tells a screen reader to announce the change
        // without the user having to go looking for it; assertive would cut off
        // whatever they were reading for something they cannot act on instantly.
        gpui::div()
            .id("mux-connection-status")
            .role(gpui::Role::Status)
            .aria_live(gpui::accesskit::Live::Polite)
            .aria_announcement(format!("Mux connection: {state_text}"))
            .when_some(visible_color, |element, color| {
                element.child(
                    ui::Label::new(state_text)
                        .size(ui::LabelSize::Small)
                        .color(color),
                )
            })
    }
}

impl workspace::StatusItemView for MuxConnectionStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

/// §3.3 Client-side view of which windows share which session (Plan 32).
///
/// `windows` holds the windows this process owns; `roster` is the server's
/// authoritative membership, rebuilt from the at-least-once `WindowAdded` /
/// `WindowRemoved` lifecycle stream.
#[derive(Default)]
pub struct MuxWindows {
    pub windows: std::collections::HashMap<gpui::WindowId, MuxWindow>,
    roster: std::collections::HashMap<String, std::collections::BTreeSet<String>>,
    pub status_items: std::collections::HashMap<gpui::WindowId, WeakEntity<MuxConnectionStatusItem>>,
}

impl Global for MuxWindows {}

impl MuxWindows {
    fn apply_window_event(&mut self, event: &mux_protocol::notification::Event) -> bool {
        match event {
            mux_protocol::notification::Event::WindowAdded(added) => {
                self.roster
                    .entry(added.session_id.clone())
                    .or_default()
                    .insert(added.window_id.clone());
                true
            }
            mux_protocol::notification::Event::WindowRemoved(removed) => {
                if let Some(windows) = self.roster.get_mut(&removed.session_id) {
                    windows.remove(&removed.window_id);
                    if windows.is_empty() {
                        self.roster.remove(&removed.session_id);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn session_window_ids(&self, session_id: &str) -> Vec<String> {
        self.roster
            .get(session_id)
            .map(|windows| windows.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// §3.3 Rebind `window_id` to a new (domain, session) binding.
///
/// Any SSH session held for the window is carried across the swap: switching
/// sessions must not tear down the tunnel, which dies only when the window
/// binding is removed (`take_mux_window`). Generic over the held resource so
/// the carry-over contract is unit-testable without a live SSH tunnel.
pub fn rebind_mux_window<T>(
    windows: &mut std::collections::HashMap<gpui::WindowId, MuxWindow<T>>,
    window_id: gpui::WindowId,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    ssh_session: Option<T>,
) {
    let previous = windows.remove(&window_id);
    let (held, connection_state) = previous
        .map(|existing| (existing.ssh_session, existing.connection_state))
        .unwrap_or_default();
    windows.insert(
        window_id,
        MuxWindow {
            domain,
            session_id,
            ssh_session: held.or_else(|| {
                ssh_session.map(|ssh_session| Arc::new(futures::lock::Mutex::new(ssh_session)))
            }),
            connection_state,
        },
    );
}

pub fn register_mux_window(
    window_id: gpui::WindowId,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    ssh_session: Option<DefaultWindowHeld>,
    cx: &mut App,
) {
    if cx.try_global::<MuxWindows>().is_none() {
        cx.set_global(MuxWindows::default());
    }
    cx.update_global::<MuxWindows, ()>(|windows, _| {
        rebind_mux_window(
            &mut windows.windows,
            window_id,
            domain,
            session_id,
            ssh_session,
        );
    });
}

pub fn take_mux_window(window_id: gpui::WindowId, cx: &mut App) -> Option<MuxWindow> {
    if cx.try_global::<MuxWindows>().is_none() {
        return None;
    }
    cx.update_global::<MuxWindows, Option<MuxWindow>>(|windows, _| {
        windows.status_items.remove(&window_id);
        windows.windows.remove(&window_id)
    })
}
pub struct MuxReconnectRequest<T> {
    pub domain: Arc<mux::MuxDomain>,
    pub session_id: String,
    pub ssh_session: Arc<futures::lock::Mutex<T>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MuxReconnectUnavailable {
    WindowNotBound,
    LocalWindow,
    InProgress,
}

pub fn begin_mux_reconnect<T>(
    windows: &mut std::collections::HashMap<gpui::WindowId, MuxWindow<T>>,
    window_id: gpui::WindowId,
) -> Result<MuxReconnectRequest<T>, MuxReconnectUnavailable> {
    let binding = windows
        .get_mut(&window_id)
        .ok_or(MuxReconnectUnavailable::WindowNotBound)?;
    let ssh_session = binding
        .ssh_session
        .clone()
        .ok_or(MuxReconnectUnavailable::LocalWindow)?;
    if !binding.connection_state.begin_reconnect() {
        return Err(MuxReconnectUnavailable::InProgress);
    }
    Ok(MuxReconnectRequest {
        domain: binding.domain.clone(),
        session_id: binding.session_id.clone(),
        ssh_session,
    })
}

pub fn finish_mux_reconnect<T>(
    windows: &mut std::collections::HashMap<gpui::WindowId, MuxWindow<T>>,
    window_id: gpui::WindowId,
    domain: &Arc<mux::MuxDomain>,
    succeeded: bool,
) -> bool {
    let Some(binding) = windows.get_mut(&window_id) else {
        return false;
    };
    if !Arc::ptr_eq(&binding.domain, domain) {
        return false;
    }
    binding.connection_state.finish_reconnect(succeeded);
    true
}

pub fn mark_remote_mux_window_disconnected<T>(
    windows: &mut std::collections::HashMap<gpui::WindowId, MuxWindow<T>>,
    window_id: gpui::WindowId,
    domain: &Arc<mux::MuxDomain>,
) -> bool {
    let Some(binding) = windows.get_mut(&window_id) else {
        return false;
    };
    if binding.ssh_session.is_none() || !Arc::ptr_eq(&binding.domain, domain) {
        return false;
    }
    binding.connection_state.mark_disconnected()
}


pub fn publish_mux_connection_state(window_id: gpui::WindowId, cx: &mut App) {
    let Some(windows) = cx.try_global::<MuxWindows>() else {
        return;
    };
    let Some(state) = windows
        .windows
        .get(&window_id)
        .map(|binding| binding.connection_state)
    else {
        return;
    };
    let Some(item) = windows.status_items.get(&window_id).cloned() else {
        return;
    };
    item.update(cx, |item, cx| item.set_state(state, cx)).ok();
}

/// §15.4 Watch a remote window's tunnel and surface the outage.
///
/// The lifecycle notification stream cannot carry this: its subscriber channel
/// stays open when the transport dies (only a dropped domain closes it), so a
/// dead tunnel is indistinguishable from an idle one. `check_connection` issues

/// Falls back to the process-wide `AppState` domain so windows opened outside
/// the multi-window path (and every pre-Plan-32 caller) keep working.
pub fn mux_domain_for_window(window: &Window, cx: &App) -> Option<Arc<mux::MuxDomain>> {
    let window_id = window.window_handle().window_id();
    cx.try_global::<MuxWindows>()
        .and_then(|windows| windows.windows.get(&window_id))
        .map(|mux_window| mux_window.domain.clone())
        .or_else(|| workspace::AppState::try_global(cx).and_then(|state| state.mux_domain.clone()))
}

/// §3.3 The session `window` renders, preferring this window's own binding.
pub fn mux_session_for_window(window: &Window, cx: &App) -> Option<String> {
    let window_id = window.window_handle().window_id();
    cx.try_global::<MuxWindows>()
        .and_then(|windows| windows.windows.get(&window_id))
        .map(|mux_window| mux_window.session_id.clone())
}

pub const MAX_OPEN_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenFileRoute {
    Matched,
    Unbound,
    WrongSession,
}

pub fn mux_binding_for_window(
    window: &Window,
    cx: &App,
) -> Option<(Arc<mux::MuxDomain>, String)> {
    let window_id = window.window_handle().window_id();
    cx.try_global::<MuxWindows>()
        .and_then(|windows| windows.windows.get(&window_id))
        .map(|mux_window| (mux_window.domain.clone(), mux_window.session_id.clone()))
}

/// §15.4 Project a snapshot into this window, with panes the desktop knows how
/// to build: mux pane views wired to the QuickJS extension host.
pub fn install_snapshot_panes(
    workspace: &mut workspace::Workspace,
    snapshot: &MuxSnapshot,
    domain: Arc<mux::MuxDomain>,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    workspace::layout_projection::install_snapshot_panes(
        workspace,
        snapshot,
        |workspace, pane_id, window, cx| {
            let view = cx.new(|cx| {
                new_mux_pane_view(
                    pane_id,
                    domain.clone(),
                    workspace.weak_handle(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            subscribe_mux_pane_extension_actions(&view, cx);
            Box::new(view)
        },
        window,
        cx,
    );
}

/// §3.3 Open one GPUI window bound to its own mux connection (Plan 32).
///
/// The window attaches with a server-minted window id before it is opened, so
/// the layout it renders is the authoritative snapshot the server handed back

/// extension host, so the sidebar is registered unconditionally alongside the
/// window's mux binding rather than by an extension.
pub fn install_session_sidebar(
    multi_workspace: &mut workspace::MultiWorkspace,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    snapshot: Option<&mux_protocol::SessionSnapshot>,
    restore_width: Option<gpui::Pixels>,
    window: &mut Window,
    cx: &mut Context<workspace::MultiWorkspace>,
    on_request: Rc<dyn Fn(&WeakEntity<workspace::Workspace>, &Arc<mux::MuxDomain>, sidebar::SidebarRequest, &mut Window, &mut App)>,
) {
    let workspace = multi_workspace.workspace().downgrade();
    let handler_domain = domain.clone();
    let sidebar = cx.new(|cx| {
        sidebar::Sidebar::new(
            domain,
            session_id,
            snapshot,
            Rc::new(move |request, window: &mut Window, cx: &mut App| {
                on_request(&workspace, &handler_domain, request, window, cx);
            }),
            window,
            cx,
        )
    });
    multi_workspace.register_sidebar(sidebar, cx);
    if let (Some(width), Some(sidebar)) = (restore_width, multi_workspace.sidebar()) {
        sidebar.set_width(Some(width), cx);
    }
}


/// tabs but left focus wherever it happened to be is not the state the server
/// holds. An empty id means the server has no focused pane, not pane "".
pub fn focused_pane_from_layout_change(
    layout_change: &mux_protocol::SessionLayoutChanged,
) -> Option<String> {
    layout_change
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.focused_pane_id.clone())
        .filter(|pane_id| !pane_id.is_empty())
}

/// §15.4 / §15.12 Reconcile a window from the server's lifecycle stream.
///
/// `SessionLayoutChanged` carries the authoritative layout tree, which is
/// projected into this window's workspace. `WindowAdded` / `WindowRemoved`
/// maintain the client's view of which windows share the session (§3.4
/// at-least-once), and a `WindowRemoved` naming *this* window means the server
/// dropped it — surfaced to the user rather than silently ignored.
pub fn watch_mux_session_notifications(
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    window_handle: gpui::WindowHandle<workspace::MultiWorkspace>,
    cx: &mut gpui::AsyncApp,
) {
    let notifications = domain.subscribe();
    let mux_window_id = domain.window_id();
    // Weak, so a closed window's connection is not pinned open by this task:
    // the socket closes with the last strong handle, and the notification
    // stream then ends, which is what stops this loop.
    let domain = Arc::downgrade(&domain);
    cx.spawn(async move |cx| {
        while let Ok(notification) = notifications.recv().await {
            let Some(event) = notification.event else {
                continue;
            };
            match &event {
                mux_protocol::notification::Event::SessionLayoutChanged(layout_change) => {
                    let Some(proto_layout) = layout_change.layout.as_ref() else {
                        continue;
                    };
                    let layout =
                        workspace::layout_projection::LayoutTree::from_proto(proto_layout);
                    let focused_pane = focused_pane_from_layout_change(layout_change);
                    let Some(domain) = domain.upgrade() else {
                        break;
                    };
                    if let Err(error) = cx.update_window(window_handle.into(), move |_, window, cx| {
                        let Some(multi_workspace) =
                            window.root::<workspace::MultiWorkspace>().flatten()
                        else {
                            return;
                        };
                        let Some(workspace) =
                            multi_workspace.read(cx).workspaces().next().cloned()
                        else {
                            return;
                        };
                        workspace.update(cx, |workspace, cx| {
                            apply_mux_layout_to_workspace(
                                workspace,
                                &layout,
                                focused_pane.as_deref(),
                                domain,
                                window,
                                cx,
                            );
                        });
                    }) {
                        tracing::debug!(error = %error, "app context closed during SessionLayoutChanged reconcile");
                        break;
                    }
                }
                mux_protocol::notification::Event::WindowAdded(_)
                | mux_protocol::notification::Event::WindowRemoved(_) => {
                    let dropped_this_window = matches!(
                        &event,
                        mux_protocol::notification::Event::WindowRemoved(removed)
                            if removed.window_id == mux_window_id
                    );
                    let session_id = session_id.clone();
                    cx.update(|cx| {
                        if cx.try_global::<MuxWindows>().is_none() {
                            cx.set_global(MuxWindows::default());
                        }
                        let windows = cx.update_global::<MuxWindows, Vec<String>>(|windows, _| {
                            windows.apply_window_event(&event);
                            windows.session_window_ids(&session_id)
                        });
                        tracing::info!(
                            session_id = %session_id,
                            windows = windows.len(),
                            "mux session window membership changed"
                        );
                        if dropped_this_window {
                            (hooks(cx).show_error)(
                                cx,
                                "This window was removed from the mux session".to_string(),
                            );
                        }
                    });
                }
                _ => {}
            }
        }
        // Reaching here means the domain was dropped, i.e. the window is going
        // away. A live tunnel that dies never ends this loop — the subscriber
        // channel outlives the transport — which is why outage detection lives
        // in `watch_remote_mux_connection` instead.
    })
    .detach();
}

/// §16.9 Forward a layout ratio resize to the server.
pub fn forward_layout_resize(
    window: &Window,
    cx: &mut gpui::App,
    pane_id: String,
    direction: mux_protocol::split_node::SplitDirection,
    delta: f32,
) {
    let Some(domain) = mux_domain_for_window(window, cx) else {
        return;
    };
    // The foreground executor accepts non-Send futures, which the domain may
    // contain on the browser build.
    cx.spawn(async move |_| {
        if let Err(error) = domain.resize_layout(&pane_id, direction, delta).await {
            tracing::warn!(error = %error, "resize_layout RPC failed");
        }
    })
    .detach();
}

/// §16.9 Report the split ratios this window has moved away from the server's.
///
/// A drag moves one divider, so this usually sends one request; the ratios are
/// absolute, so re-sending one changes nothing. Each split is its own request
/// because each is its own node — there is no "set the whole tree" call, and
/// there should not be: the server owns the layout.
pub fn forward_layout_ratios(
    workspace: &workspace::Workspace,
    window: &Window,
    cx: &mut gpui::App,
) {
    let drift = workspace.mux_layout_ratio_drift();
    let Some(domain) = mux_domain_for_window(window, cx) else {
        return;
    };
    cx.spawn(async move |_| {
        for (node_id, ratios) in drift {
            if let Err(error) = domain.set_layout_ratios(&node_id, ratios).await {
                tracing::warn!(error = %error, node_id, "set_layout_ratios RPC failed");
            }
        }
    })
    .detach();
}

/// §16.9 Forward a dropped tab as a pane move.
///
/// The drop already happened locally; this asks the server for the same thing
/// so the two agree. The authoritative `SessionLayoutChanged` that comes back
/// is what the window ends up rendering, which is also what corrects a drop
/// the server would not accept.
pub fn forward_tab_drop(
    workspace: &workspace::Workspace,
    item_id: gpui::EntityId,
    target_item_id: Option<gpui::EntityId>,
    split_direction: Option<workspace::SplitDirection>,
    before: bool,
    window: &Window,
    cx: &mut gpui::App,
) {
    use mux_protocol::split_node::SplitDirection as WireDirection;

    // Which two mux panes the drop involves. An item that renders no mux pane,
    // a drop with nothing under it, or a pane dropped onto itself all mean the
    // same thing here: there is nothing to ask the server for.
    let Some((pane_id, target_pane_id)) = mux_pane_id_for_item(workspace, item_id, cx)
        .zip(target_item_id.and_then(|target| mux_pane_id_for_item(workspace, target, cx)))
        .filter(|(pane_id, target_pane_id)| pane_id != target_pane_id)
    else {
        return;
    };

    let (direction, before) = match split_direction {
        Some(workspace::SplitDirection::Left) => (WireDirection::LeftRight, true),
        Some(workspace::SplitDirection::Right) => (WireDirection::LeftRight, false),
        Some(workspace::SplitDirection::Up) => (WireDirection::TopBottom, true),
        Some(workspace::SplitDirection::Down) => (WireDirection::TopBottom, false),
        // Dropped into a tab strip. The server has no stacking, so the closest
        // placement it can hold is beside the target on the axis already there.
        None => {
            let direction = workspace
                .get_server_layout()
                .and_then(|layout| layout.parent_direction(&target_pane_id))
                .map(|direction| match direction {
                    workspace::layout_projection::SplitDirection::LeftRight => {
                        WireDirection::LeftRight
                    }
                    workspace::layout_projection::SplitDirection::TopBottom => {
                        WireDirection::TopBottom
                    }
                })
                .unwrap_or(WireDirection::LeftRight);
            (direction, before)
        }
    };

    let Some(domain) = mux_domain_for_window(window, cx) else {
        return;
    };
    cx.spawn(async move |_| {
        if let Err(error) = domain
            .move_pane(&pane_id, &target_pane_id, direction, before)
            .await
        {
            tracing::warn!(error = %error, pane_id, target_pane_id, "move_pane RPC failed");
        }
    })
    .detach();
}

/// The mux pane an item renders, if it renders one at all.
fn mux_pane_id_for_item(
    workspace: &workspace::Workspace,
    item_id: gpui::EntityId,
    cx: &gpui::App,
) -> Option<String> {
    workspace.panes().iter().find_map(|pane| {
        pane.read(cx)
            .items()
            .find(|item| item.item_id() == item_id)
            .and_then(|item| {
                item.to_any_view()
                    .downcast::<terminal_view::mux_pane::MuxPaneView>()
                    .ok()
            })
            .map(|view| view.read(cx).pane_id.clone())
    })
}

/// §15.7 Register the mux pane action handlers both surfaces share: split,
/// focus, tabs, resize, zoom, prefix mode. Desktop-only handlers (attach,
/// detach, reconnect, kill, new window) stay in the desktop binary because
/// they drive the daemon; error surfaces arrive through [`MuxWindowHooks::show_error`].
pub fn register_core_mux_actions(workspace: &mut workspace::Workspace, window: &mut Window, cx: &mut gpui::Context<workspace::Workspace>) {
                    workspace
                        .register_action(|workspace, _: &settings::mux_actions::SplitRight, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let pane_id = mux_view.read(cx).pane_id.clone();
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                match domain.split_pane(&pane_id, mux_protocol::split_node::SplitDirection::LeftRight).await {
                                    Ok(new_pane_id) => {
                                        if let Err(e) = window_handle.update(cx, |_, window, cx| {
                                            if let Err(e) = weak_workspace.update(cx, |workspace, cx| {
                                                let view = cx.new(|cx| {
                                                    new_mux_pane_view(new_pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                                });
                                                subscribe_mux_pane_extension_actions(&view, cx);
                                                let item: Box<dyn workspace::ItemHandle> = Box::new(view);
                                                workspace.split_item(workspace::SplitDirection::Right, item, window, cx);
                                            }) {
                                                tracing::debug!(error = %e, "workspace dropped during mux_pane::SplitRight handler");
                                            }
                                        }) {
                                            tracing::debug!(error = %e, "window dropped during mux_pane::SplitRight handler");
                                        }
                                    }
                                    Err(error) => {
                                        tracing::error!(pane_id, %error, "mux_pane::SplitRight failed");
                                        cx.update(|_, cx| (hooks(cx).show_error)(
                                            cx,
                                            format!("Failed to split mux pane {pane_id}: {error}"),
                                        ))?;
                                    }
                                }
                                anyhow::Ok(())
                            }).detach();
                        })
                        .register_action(|workspace, _: &settings::mux_actions::SplitDown, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let pane_id = mux_view.read(cx).pane_id.clone();
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                match domain.split_pane(&pane_id, mux_protocol::split_node::SplitDirection::TopBottom).await {
                                    Ok(new_pane_id) => {
                                        if let Err(e) = window_handle.update(cx, |_, window, cx| {
                                            if let Err(e) = weak_workspace.update(cx, |workspace, cx| {
                                                let view = cx.new(|cx| {
                                                    new_mux_pane_view(new_pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                                });
                                                subscribe_mux_pane_extension_actions(&view, cx);
                                                let item: Box<dyn workspace::ItemHandle> = Box::new(view);
                                                workspace.split_item(workspace::SplitDirection::Down, item, window, cx);
                                            }) {
                                                tracing::debug!(error = %e, "workspace dropped during mux_pane::SplitDown handler");
                                            }
                                        }) {
                                            tracing::debug!(error = %e, "window dropped during mux_pane::SplitDown handler");
                                        }
                                    }
                                    Err(error) => {
                                        tracing::error!(pane_id, %error, "mux_pane::SplitDown failed");
                                        cx.update(|_, cx| (hooks(cx).show_error)(
                                            cx,
                                            format!("Failed to split mux pane {pane_id}: {error}"),
                                        ))?;
                                    }
                                }
                                anyhow::Ok(())
                            }).detach();
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusLeft, window, cx| {
                            if let Some(pane) = workspace.find_pane_in_direction(workspace::SplitDirection::Left, cx) {
                                focus_mux_workspace_pane(pane, window, cx);
                            }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusRight, window, cx| {
                            if let Some(pane) = workspace.find_pane_in_direction(workspace::SplitDirection::Right, cx) {
                                focus_mux_workspace_pane(pane, window, cx);
                            }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusUp, window, cx| {
                            if let Some(pane) = workspace.find_pane_in_direction(workspace::SplitDirection::Up, cx) {
                                focus_mux_workspace_pane(pane, window, cx);
                            }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusDown, window, cx| {
                            if let Some(pane) = workspace.find_pane_in_direction(workspace::SplitDirection::Down, cx) {
                                focus_mux_workspace_pane(pane, window, cx);
                            }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusNextPane, window, cx| {
                            focus_adjacent_mux_pane(workspace, true, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPrevPane, window, cx| {
                            focus_adjacent_mux_pane(workspace, false, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::NextTab, window, cx| {
                            workspace.active_pane().update(cx, |pane, cx| {
                                pane.activate_next_item(&workspace::pane::ActivateNextItem::default(), window, cx);
                            });
                        })
                        .register_action(|workspace, _: &settings::mux_actions::PrevTab, window, cx| {
                            workspace.active_pane().update(cx, |pane, cx| {
                                pane.activate_previous_item(&workspace::pane::ActivatePreviousItem::default(), window, cx);
                            });
                        })
                        .register_action(|workspace, action: &settings::mux_actions::FocusPaneIndex, window, cx| {
                            focus_mux_pane_index(workspace, action.index, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane0, window, cx| {
                            focus_mux_pane_index(workspace, 0, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane1, window, cx| {
                            focus_mux_pane_index(workspace, 1, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane2, window, cx| {
                            focus_mux_pane_index(workspace, 2, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane3, window, cx| {
                            focus_mux_pane_index(workspace, 3, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane4, window, cx| {
                            focus_mux_pane_index(workspace, 4, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane5, window, cx| {
                            focus_mux_pane_index(workspace, 5, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane6, window, cx| {
                            focus_mux_pane_index(workspace, 6, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane7, window, cx| {
                            focus_mux_pane_index(workspace, 7, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane8, window, cx| {
                            focus_mux_pane_index(workspace, 8, window, cx);
                        })
                        .register_action(|workspace, action: &settings::mux_actions::EnterPrefixMode, _window, cx| {
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            mux_view.update(cx, |view, cx| view.enter_prefix_mode(action.timeout_ms, cx));
                        })
                        .register_action(|workspace, action: &settings::mux_actions::SendLiteral, _window, cx| {
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            mux_view.update(cx, |view, cx| view.send_literal(&action.keystroke, cx));
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeLeft, window, cx| {
                            workspace.resize_pane(gpui::Axis::Horizontal, gpui::px(-50.0), window, cx);
                            let pane_id = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx).map(|v| v.read(cx).pane_id.clone());
                            if let Some(id) = pane_id { forward_layout_resize(window, cx, id, mux_protocol::split_node::SplitDirection::LeftRight, -0.05); }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeRight, window, cx| {
                            workspace.resize_pane(gpui::Axis::Horizontal, gpui::px(50.0), window, cx);
                            let pane_id = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx).map(|v| v.read(cx).pane_id.clone());
                            if let Some(id) = pane_id { forward_layout_resize(window, cx, id, mux_protocol::split_node::SplitDirection::LeftRight, 0.05); }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeUp, window, cx| {
                            workspace.resize_pane(gpui::Axis::Vertical, gpui::px(-50.0), window, cx);
                            let pane_id = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx).map(|v| v.read(cx).pane_id.clone());
                            if let Some(id) = pane_id { forward_layout_resize(window, cx, id, mux_protocol::split_node::SplitDirection::TopBottom, -0.05); }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeDown, window, cx| {
                            workspace.resize_pane(gpui::Axis::Vertical, gpui::px(50.0), window, cx);
                            let pane_id = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx).map(|v| v.read(cx).pane_id.clone());
                            if let Some(id) = pane_id { forward_layout_resize(window, cx, id, mux_protocol::split_node::SplitDirection::TopBottom, 0.05); }
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeEqual, _window, cx| {
                            workspace.reset_pane_sizes(cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::CloseTab, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let pane_id = mux_view.read(cx).pane_id.clone();
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                match domain.close_pane(&pane_id).await {
                                    Ok(()) => {
                                        window_handle.update(cx, |_, window, cx| {
                                            weak_workspace.update(cx, |workspace, cx| {
                                                workspace.active_pane().update(cx, |pane, cx| {
                                                    pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
                                                        .detach_and_log_err(cx);
                                                });
                                            })
                                        })??;
                                    }
                                    Err(error) => {
                                        tracing::error!(pane_id, %error, "mux_pane::CloseTab failed");
                                        cx.update(|_, cx| (hooks(cx).show_error)(
                                            cx,
                                            format!("Failed to close mux pane {pane_id}: {error}"),
                                        ))?;
                                    }
                                }
                                anyhow::Ok(())
                            }).detach();
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ZoomToggle, window, cx| {
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let new_zoom = !mux_view.read(cx).is_zoomed();
                            // Updates the view's zoom state and notifies the server
                            // (zoom_pane RPC is fire-and-forget; errors logged in set_zoomed).
                            mux_view.update(cx, |view, cx| view.set_zoomed(new_zoom, cx));
                            // Reflect the zoom into the workspace's zoomed view.
                            let pane = workspace.active_pane().clone();
                            workspace.set_pane_zoomed(pane, new_zoom, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::NewTab, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let known_session = mux_session_for_window(window, cx);
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                let session_id = if let Some(session_id) =
                                    known_session.or_else(|| domain.last_attached_session_id())
                                {
                                    Some(session_id)
                                } else {
                                    match domain.list_sessions().await {
                                        Ok(sessions) => sessions.first().map(|session| session.id.clone()),
                                        Err(error) => {
                                            tracing::error!(%error, "mux_pane::NewTab list_sessions failed");
                                            cx.update(|_, cx| (hooks(cx).show_error)(
                                                cx,
                                                format!("Failed to find a mux session for the new tab: {error}"),
                                            ))?;
                                            None
                                        }
                                    }
                                };
                                let Some(session_id) = session_id else {
                                    cx.update(|_, cx| (hooks(cx).show_error)(
                                        cx,
                                        "No mux session is available for the new tab".to_string(),
                                    ))?;
                                    return anyhow::Ok(());
                                };
                                let size = mux_protocol::TerminalSize { cols: 80, rows: 24 };
                                let tab_id = format!("tab-{}", nanoid::nanoid!());
                                match domain.spawn_pane(&session_id, &tab_id, size, None, None).await {
                                    Ok(new_pane_id) => {
                                        if let Err(error) = window_handle.update(cx, |_, window, cx| {
                                            if let Err(error) = weak_workspace.update(cx, |workspace, cx| {
                                                let pane = workspace.active_pane().clone();
                                                let view = cx.new(|cx| {
                                                    new_mux_pane_view(new_pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                                });
                                                subscribe_mux_pane_extension_actions(&view, cx);
                                                let item: Box<dyn workspace::ItemHandle> = Box::new(view);
                                                workspace.add_item(pane, item, None, true, true, window, cx);
                                            }) {
                                                tracing::debug!(%error, "workspace dropped during mux_pane::NewTab handler");
                                            }
                                        }) {
                                            tracing::debug!(%error, "window dropped during mux_pane::NewTab handler");
                                        }
                                    }
                                    Err(error) => {
                                        tracing::error!(session_id, %error, "mux_pane::NewTab spawn failed");
                                        cx.update(|_, cx| (hooks(cx).show_error)(
                                            cx,
                                            format!("Failed to create mux tab in session {session_id}: {error}"),
                                        ))?;
                                    }
                                }
                                anyhow::Ok(())
                            }).detach();
                        });

    workspace.register_action(
        |workspace, _: &settings::mux_actions::RenameSession, window, cx| {
            let Some(domain) = mux_domain_for_window(window, cx) else {
                return;
            };
            let Some(session_id) = mux_session_for_window(window, cx) else {
                return;
            };
            workspace.toggle_modal(window, cx, move |window, cx| {
                RenameSessionModal::new(domain, session_id, window, cx)
            });
        },
    );

    // §16.9 The divider drag lives in `workspace`, which has the ratios but
    // not the socket, so it raises an event and the forwarding happens here.
    let this = cx.entity();
    cx.subscribe_in(&this, window, |workspace, _, event, window, cx| match event {
        workspace::Event::LayoutRatiosChanged => forward_layout_ratios(workspace, window, cx),
        workspace::Event::TabDropped {
            item_id,
            target_item_id,
            split_direction,
            before,
        } => forward_tab_drop(
            workspace,
            *item_id,
            *target_item_id,
            *split_direction,
            *before,
            window,
            cx,
        ),
        _ => {}
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use gpui::AppContext as _;

    #[test]
    fn cyclic_pane_navigation_wraps_both_directions() {
        assert_eq!(super::cyclic_pane_index(0, 0, true), None);
        assert_eq!(super::cyclic_pane_index(2, 2, true), None);
        assert_eq!(super::cyclic_pane_index(0, 3, true), Some(1));
        assert_eq!(super::cyclic_pane_index(2, 3, true), Some(0));
        assert_eq!(super::cyclic_pane_index(0, 3, false), Some(2));
        assert_eq!(super::cyclic_pane_index(2, 3, false), Some(1));
    }

    #[test]
    fn reconnect_state_serializes_attempts_and_records_outcomes() {
        let mut state = super::MuxConnectionState::Disconnected;

        assert!(state.begin_reconnect());
        assert_eq!(state, super::MuxConnectionState::Reconnecting);
        assert!(
            !state.begin_reconnect(),
            "a second reconnect must not start while one is in flight"
        );

        state.finish_reconnect(false);
        assert_eq!(state, super::MuxConnectionState::Disconnected);
        assert!(state.begin_reconnect());
        state.finish_reconnect(true);
        assert_eq!(state, super::MuxConnectionState::Connected);
    }

    fn window_added(session_id: &str, window_id: &str) -> mux_protocol::notification::Event {
        mux_protocol::notification::Event::WindowAdded(mux_protocol::WindowAdded {
            window_id: window_id.to_string(),
            session_id: session_id.to_string(),
        })
    }

    fn window_removed(session_id: &str, window_id: &str) -> mux_protocol::notification::Event {
        mux_protocol::notification::Event::WindowRemoved(mux_protocol::WindowRemoved {
            window_id: window_id.to_string(),
            session_id: session_id.to_string(),
        })
    }

    /// §3.3 / §3.4 The client's view of session membership is rebuilt purely
    /// from the at-least-once `WindowAdded` / `WindowRemoved` stream (Plan 32).
    #[test]
    fn window_events_maintain_the_session_roster() {
        let mut windows = super::MuxWindows::default();

        assert!(windows.apply_window_event(&window_added("session-1", "win-1")));
        assert!(windows.apply_window_event(&window_added("session-1", "win-2")));
        assert_eq!(
            windows.session_window_ids("session-1"),
            vec!["win-1".to_string(), "win-2".to_string()]
        );

        // Duplicates are expected: lifecycle delivery is at-least-once.
        windows.apply_window_event(&window_added("session-1", "win-2"));
        assert_eq!(windows.session_window_ids("session-1").len(), 2);

        assert!(windows.apply_window_event(&window_removed("session-1", "win-1")));
        assert_eq!(
            windows.session_window_ids("session-1"),
            vec!["win-2".to_string()]
        );

        windows.apply_window_event(&window_removed("session-1", "win-2"));
        assert!(windows.session_window_ids("session-1").is_empty());

        assert!(
            !windows.apply_window_event(&mux_protocol::notification::Event::PaneDirty(
                mux_protocol::PaneDirty {
                    pane_id: "pane-1".to_string(),
                }
            )),
            "non-window events must not be treated as membership changes"
        );
    }

    /// §3.3 A window that never joined must not corrupt another session's roster.
    #[test]
    fn removing_an_unknown_window_is_a_no_op() {
        let mut windows = super::MuxWindows::default();
        windows.apply_window_event(&window_added("session-1", "win-1"));

        windows.apply_window_event(&window_removed("session-2", "win-9"));

        assert_eq!(
            windows.session_window_ids("session-1"),
            vec!["win-1".to_string()]
        );
        assert!(windows.session_window_ids("session-2").is_empty());
    }

    /// §3.3 / §16.6 Swapping a window's binding (sidebar session switch) must
    /// carry any held SSH session across the swap: the tunnel keeps the
    /// window's remote connection alive and is released only when the window
    /// owner itself is removed (window close -> `take_mux_window`).
    #[test]
    fn rebinding_a_window_preserves_the_held_ssh_session_until_removed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        /// Stand-in for `mux::SshSession`: counts drops so the carry-over
        /// contract is observable without a live SSH tunnel.
        struct SessionProbe(Arc<AtomicUsize>);

        impl Drop for SessionProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        /// A domain over an EOF-only stream: the mux I/O thread exits
        /// immediately and the domain is never used for requests here.
        fn dummy_domain() -> Arc<mux::MuxDomain> {
            Arc::new(
                mux::MuxDomain::connect_with_blocking_stream(std::io::Cursor::new(Vec::new()))
                    .expect("dummy domain"),
            )
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let window_id = gpui::WindowId::from(1u64);
        let mut windows: std::collections::HashMap<gpui::WindowId, super::MuxWindow<SessionProbe>> =
            std::collections::HashMap::new();

        super::rebind_mux_window(
            &mut windows,
            window_id,
            dummy_domain(),
            "session-1".to_string(),
            Some(SessionProbe(drops.clone())),
        );

        // Session switch in the same window: the held session must survive.
        super::rebind_mux_window(
            &mut windows,
            window_id,
            dummy_domain(),
            "session-2".to_string(),
            None,
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "a session switch must not drop the held SSH session"
        );
        assert_eq!(
            windows.get(&window_id).map(|binding| binding.session_id.as_str()),
            Some("session-2"),
            "the new binding must win"
        );
        assert_eq!(windows.len(), 1, "rebinding must not leak window slots");

        // Removing the window owner (what take_mux_window does on close)
        // releases the session.
        windows.remove(&window_id);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "removing the window owner must drop the SSH session"
        );
    }

    /// §15.4 Reconnect broadcasts a synthetic `SessionLayoutChanged` carrying
    /// the whole snapshot. Reading only `.layout` out of it silently drops the
    /// focused pane, which is part of the state the reconnect has to restore.
    #[test]
    fn layout_change_carries_the_authoritative_focus() {
        let with_focus = mux_protocol::SessionLayoutChanged {
            layout: None,
            snapshot: Some(mux_protocol::SessionSnapshot {
                focused_pane_id: "pane-7".to_string(),
                ..Default::default()
            }),
        };
        assert_eq!(
            super::focused_pane_from_layout_change(&with_focus),
            Some("pane-7".to_string())
        );

        let unfocused = mux_protocol::SessionLayoutChanged {
            layout: None,
            snapshot: Some(mux_protocol::SessionSnapshot::default()),
        };
        assert_eq!(super::focused_pane_from_layout_change(&unfocused), None);

        let no_snapshot = mux_protocol::SessionLayoutChanged {
            layout: None,
            snapshot: None,
        };
        assert_eq!(super::focused_pane_from_layout_change(&no_snapshot), None);
    }

    /// Losing the mux connection is shown as small coloured text in the status
    /// bar, while the user is working inside a pane. Without a live region the
    /// change is never announced, so a screen-reader user keeps typing into a
    /// session that is no longer attached.
    #[gpui::test]
    async fn mux_connection_state_is_announced_when_it_changes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let window = cx.add_window(|_, _| super::MuxConnectionStatusItem {
            state: super::MuxConnectionState::Disconnected,
        });
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the status window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "mux connection status");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "mux connection status");
        gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "mux connection status");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "mux connection status");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "mux connection status");
        gpui::a11y_checks::assert_controls_have_area(&tree, "mux connection status");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "mux connection status");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "mux connection status");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "mux connection status");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "mux connection status");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "mux connection status");
        gpui::a11y_checks::assert_live_regions_can_speak(&tree, "mux connection status");

        let status = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .find(|node| node["aria"]["role"] == "Status")
            .expect("the connection indicator must be reported as a status");
        assert_eq!(
            status["aria"]["live"].as_str(),
            Some("Polite"),
            "a status that changes on its own has to be a live region"
        );
        // The value, not the label: macOS speaks `node.value()` and raises no
        // announcement at all without one.
        assert_eq!(
            status["aria"]["value"].as_str(),
            Some("Mux connection: Disconnected"),
            "the announcement has to say what changed, not just \"Disconnected\""
        );
    }

    /// The all-clear, not the alarm. Nothing is drawn once the connection is
    /// back, and if the announcement is dropped along with the text then a
    /// reader who was told the session detached is never told it returned.
    #[gpui::test]
    async fn mux_reconnection_is_announced_even_though_nothing_is_drawn(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let window = cx.add_window(|_, _| super::MuxConnectionStatusItem {
            state: super::MuxConnectionState::Disconnected,
        });
        cx.activate_a11y(window.into());

        let announcement = |cx: &mut gpui::TestAppContext| -> Option<String> {
            let json = cx
                .update_window(window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("the status window is still open")
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            gpui::a11y_checks::assert_live_regions_can_speak(&tree, "mux reconnection");
            tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .find(|node| node["aria"]["role"] == "Status")
                .and_then(|node| node["aria"]["value"].as_str())
                .map(str::to_owned)
        };

        assert_eq!(
            announcement(cx).as_deref(),
            Some("Mux connection: Disconnected"),
            "the drop is the precondition: without it the recovery says nothing"
        );

        window
            .update(cx, |item, _, cx| {
                item.state = super::MuxConnectionState::Connected;
                cx.notify();
            })
            .expect("the status window is still open");

        assert_eq!(
            announcement(cx).as_deref(),
            Some("Mux connection: Connected"),
            "coming back is the half a reader cannot see for themselves"
        );
    }
}
