//! §15.7 Native mux session tree sidebar.
//!
//! Shows the server's authoritative session list, and for the session this
//! window renders, its tabs and panes. Session switching and pane focusing are
//! core commands, so this surface is native GPUI and must keep working when the
//! QuickJS extension host is absent, crashed or fuel-limited (spec §15.7,
//! ADR-0005).
//!
//! Data flow follows the mux model (spec §3.3/§3.4): the session list is pulled
//! with `list_sessions`, the initial tab/pane tree comes from the authoritative
//! attach snapshot, and afterwards the tree is maintained from the lifecycle
//! notification stream. The sidebar never re-attaches on its own, because attach
//! has server-visible side effects.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyContext,
    ListAlignment, ListState, Pixels, Render, SharedString, Subscription, Task, Window, list,
    prelude::*, px,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use mux::MuxDomain;
use mux_protocol::{SessionInfo, SessionSnapshot, notification::Event};
use serde::{Deserialize, Serialize};
use ui::{ListItem, prelude::*};
use workspace::{
    Sidebar as WorkspaceSidebar, SidebarEvent, SidebarSide, layout_projection::LayoutTree,
};

const DEFAULT_WIDTH: Pixels = px(300.0);
const MIN_WIDTH: Pixels = px(200.0);
const MAX_WIDTH: Pixels = px(800.0);

gpui::actions!(
    mux_sidebar,
    [
        /// §15.7 Focuses the mux sidebar's filter editor.
        FocusFilter,
    ]
);

// ============================================================
// Requests the sidebar hands back to its owner
// ============================================================

/// What activating a sidebar row asks the owner to do.
///
/// The sidebar deliberately does not perform these itself: attaching a session
/// reprojects the whole workspace layout, and focusing a pane needs the
/// window's GPUI pane/item mapping. Both live in the binary that owns the mux
/// window, so the sidebar stays free of those dependencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRequest {
    ActivateSession(String),
    FocusPane(String),
}

pub type RequestHandler = Rc<dyn Fn(SidebarRequest, &mut Window, &mut App)>;

// ============================================================
// Serialization
// ============================================================

#[derive(Default, Serialize, Deserialize)]
struct SerializedSidebar {
    #[serde(default)]
    width: Option<f32>,
}

// ============================================================
// Session tree model
// ============================================================

#[derive(Clone, Debug, PartialEq)]
struct PaneNode {
    id: String,
    title: String,
    is_alive: bool,
    zoomed: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct TabNode {
    id: String,
    title: String,
    panes: Vec<PaneNode>,
}

/// Client-side projection of one session's tab/pane structure.
#[derive(Clone, Debug, Default, PartialEq)]
struct SessionTree {
    tabs: Vec<TabNode>,
    focused_pane_id: Option<String>,
    /// Panes that rang the terminal bell since they were last focused.
    bells: HashSet<String>,
}

impl SessionTree {
    fn from_snapshot(snapshot: &SessionSnapshot) -> Self {
        let tabs = snapshot
            .tabs
            .iter()
            .map(|tab| TabNode {
                id: tab.id.clone(),
                title: tab.title.clone(),
                panes: tab
                    .panes
                    .iter()
                    .map(|pane| PaneNode {
                        id: pane.id.clone(),
                        title: pane.title.clone(),
                        is_alive: pane.is_alive,
                        zoomed: pane.zoomed,
                    })
                    .collect(),
            })
            .collect();
        Self {
            tabs,
            focused_pane_id: (!snapshot.focused_pane_id.is_empty())
                .then(|| snapshot.focused_pane_id.clone()),
            bells: HashSet::default(),
        }
    }

    fn pane_mut(&mut self, pane_id: &str) -> Option<&mut PaneNode> {
        self.tabs
            .iter_mut()
            .flat_map(|tab| tab.panes.iter_mut())
            .find(|pane| pane.id == pane_id)
    }

    fn contains_pane(&self, pane_id: &str) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.panes.iter().any(|pane| pane.id == pane_id))
    }

    fn insert_pane(&mut self, pane_id: &str, tab_id: &str) -> bool {
        if self.contains_pane(pane_id) {
            return false;
        }
        let pane = PaneNode {
            id: pane_id.to_string(),
            title: String::new(),
            is_alive: true,
            zoomed: false,
        };
        match self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            Some(tab) => tab.panes.push(pane),
            None => self.tabs.push(TabNode {
                id: tab_id.to_string(),
                title: String::new(),
                panes: vec![pane],
            }),
        }
        true
    }

    fn remove_pane(&mut self, pane_id: &str) -> bool {
        let mut removed = false;
        for tab in self.tabs.iter_mut() {
            let before = tab.panes.len();
            tab.panes.retain(|pane| pane.id != pane_id);
            removed |= tab.panes.len() != before;
        }
        if removed {
            self.tabs.retain(|tab| !tab.panes.is_empty());
            self.bells.remove(pane_id);
            if self.focused_pane_id.as_deref() == Some(pane_id) {
                self.focused_pane_id = None;
            }
        }
        removed
    }

    /// Drops panes the authoritative layout no longer contains.
    ///
    /// Only prunes: additions arrive as `PaneAdded`, which carries the owning
    /// tab id that the layout tree does not model. Losing a `PaneRemoved` would
    /// otherwise leave a zombie row (spec §3.4).
    fn retain_layout_panes(&mut self, layout: &LayoutTree) -> bool {
        let live: HashSet<String> = layout.pane_ids().into_iter().collect();
        let stale: Vec<String> = self
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter(|pane| !live.contains(&pane.id))
            .map(|pane| pane.id.clone())
            .collect();
        let mut changed = false;
        for pane_id in stale {
            changed |= self.remove_pane(&pane_id);
        }
        changed
    }

    /// Applies one lifecycle notification. Returns whether the tree changed.
    fn apply_event(&mut self, event: &Event) -> bool {
        match event {
            Event::PaneAdded(added) => self.insert_pane(&added.pane_id, &added.tab_id),
            Event::PaneRemoved(removed) => self.remove_pane(&removed.pane_id),
            Event::PaneFocused(focused) => {
                self.bells.remove(&focused.pane_id);
                let changed = self.focused_pane_id.as_deref() != Some(focused.pane_id.as_str());
                self.focused_pane_id = Some(focused.pane_id.clone());
                changed
            }
            Event::PaneTitleChanged(changed) => match self.pane_mut(&changed.pane_id) {
                Some(pane) if pane.title != changed.title => {
                    pane.title = changed.title.clone();
                    true
                }
                _ => false,
            },
            Event::PaneZoomed(zoomed) => match self.pane_mut(&zoomed.pane_id) {
                Some(pane) if pane.zoomed != zoomed.zoomed => {
                    pane.zoomed = zoomed.zoomed;
                    true
                }
                _ => false,
            },
            Event::PaneBell(bell) => {
                self.contains_pane(&bell.pane_id) && self.bells.insert(bell.pane_id.clone())
            }
            Event::TabTitleChanged(changed) => {
                match self.tabs.iter_mut().find(|tab| tab.id == changed.tab_id) {
                    Some(tab) if tab.title != changed.title => {
                        tab.title = changed.title.clone();
                        true
                    }
                    _ => false,
                }
            }
            Event::SessionLayoutChanged(changed) => match changed.layout.as_ref() {
                Some(layout) => self.retain_layout_panes(&LayoutTree::from_proto(layout)),
                None => false,
            },
            _ => false,
        }
    }
}

// ============================================================
// List entries
// ============================================================

#[derive(Clone, Debug, PartialEq)]
pub enum ListEntry {
    Session {
        session_id: SharedString,
        name: SharedString,
        attached_clients: u32,
        is_current: bool,
    },
    Tab {
        tab_id: SharedString,
        title: SharedString,
        first_pane_id: Option<SharedString>,
    },
    Pane {
        pane_id: SharedString,
        title: SharedString,
        is_alive: bool,
        zoomed: bool,
        is_focused: bool,
        has_bell: bool,
    },
}

impl ListEntry {
    fn indent_level(&self) -> usize {
        match self {
            ListEntry::Session { .. } => 0,
            ListEntry::Tab { .. } => 1,
            ListEntry::Pane { .. } => 2,
        }
    }

    fn label(&self) -> &SharedString {
        match self {
            ListEntry::Session { name, .. } => name,
            ListEntry::Tab { title, .. } => title,
            ListEntry::Pane { title, .. } => title,
        }
    }

    fn element_id(&self) -> SharedString {
        match self {
            ListEntry::Session { session_id, .. } => format!("session-{session_id}").into(),
            ListEntry::Tab { tab_id, .. } => format!("tab-{tab_id}").into(),
            ListEntry::Pane { pane_id, .. } => format!("pane-{pane_id}").into(),
        }
    }

    fn request(&self) -> Option<SidebarRequest> {
        match self {
            ListEntry::Session { session_id, .. } => {
                Some(SidebarRequest::ActivateSession(session_id.to_string()))
            }
            ListEntry::Tab { first_pane_id, .. } => first_pane_id
                .as_ref()
                .map(|pane_id| SidebarRequest::FocusPane(pane_id.to_string())),
            ListEntry::Pane { pane_id, .. } => {
                Some(SidebarRequest::FocusPane(pane_id.to_string()))
            }
        }
    }
}

/// Flattens sessions plus the current session's tabs and panes into DFS order.
fn build_entries(
    sessions: &[SessionInfo],
    current_session_id: &str,
    tree: &SessionTree,
) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    for session in sessions {
        let is_current = session.id == current_session_id;
        let name = if session.name.is_empty() {
            SharedString::from(session.id.clone())
        } else {
            SharedString::from(session.name.clone())
        };
        entries.push(ListEntry::Session {
            session_id: session.id.clone().into(),
            name,
            attached_clients: session.attached_clients,
            is_current,
        });
        if !is_current {
            continue;
        }
        for tab in &tree.tabs {
            let title = if tab.title.is_empty() {
                SharedString::from(format!("tab {}", tab.id))
            } else {
                SharedString::from(tab.title.clone())
            };
            entries.push(ListEntry::Tab {
                tab_id: tab.id.clone().into(),
                title,
                first_pane_id: tab
                    .panes
                    .first()
                    .map(|pane| SharedString::from(pane.id.clone())),
            });
            for pane in &tab.panes {
                let title = if pane.title.is_empty() {
                    SharedString::from(format!("pane {}", pane.id))
                } else {
                    SharedString::from(pane.title.clone())
                };
                entries.push(ListEntry::Pane {
                    pane_id: pane.id.clone().into(),
                    title,
                    is_alive: pane.is_alive,
                    zoomed: pane.zoomed,
                    is_focused: tree.focused_pane_id.as_deref() == Some(pane.id.as_str()),
                    has_bell: tree.bells.contains(&pane.id),
                });
            }
        }
    }
    entries
}

/// Keeps a row when it matches, an ancestor matches, or a descendant matches,
/// so a filtered tree stays connected.
fn filter_entries(entries: &[ListEntry], query: &str) -> Vec<ListEntry> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return entries.to_vec();
    }

    let matched: Vec<bool> = entries
        .iter()
        .map(|entry| entry.label().to_lowercase().contains(&query))
        .collect();
    let levels: Vec<usize> = entries.iter().map(ListEntry::indent_level).collect();

    // (level, whether that row or one of its ancestors matched)
    let mut ancestors: Vec<(usize, bool)> = Vec::new();
    let mut inherited = vec![false; entries.len()];
    for index in 0..entries.len() {
        while ancestors
            .last()
            .is_some_and(|(level, _)| *level >= levels[index])
        {
            ancestors.pop();
        }
        let from_ancestor = ancestors.last().is_some_and(|(_, matched)| *matched);
        inherited[index] = from_ancestor;
        ancestors.push((levels[index], from_ancestor || matched[index]));
    }

    entries
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            if matched[*index] || inherited[*index] {
                return true;
            }
            entries
                .iter()
                .enumerate()
                .skip(index + 1)
                .take_while(|(descendant, _)| levels[*descendant] > levels[*index])
                .any(|(descendant, _)| matched[descendant])
        })
        .map(|(_, entry)| entry.clone())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionMove {
    Next,
    Previous,
    First,
    Last,
}

/// Selection stays inside the visible rows: an empty list has no selection, and
/// stepping past either end clamps instead of wrapping.
fn move_selection(
    current: Option<usize>,
    length: usize,
    movement: SelectionMove,
) -> Option<usize> {
    let last = length.checked_sub(1)?;
    Some(match (movement, current) {
        (SelectionMove::First, _) => 0,
        (SelectionMove::Last, _) => last,
        (SelectionMove::Next, None) => 0,
        (SelectionMove::Previous, None) => last,
        (SelectionMove::Next, Some(index)) => index.saturating_add(1).min(last),
        (SelectionMove::Previous, Some(index)) => index.saturating_sub(1),
    })
}

// ============================================================
// Sidebar
// ============================================================

pub struct Sidebar {
    domain: Arc<MuxDomain>,
    session_id: String,
    request_handler: RequestHandler,
    sessions: Vec<SessionInfo>,
    tree: SessionTree,
    entries: Vec<ListEntry>,
    selected_index: Option<usize>,
    filter_editor: Entity<Editor>,
    list_state: ListState,
    width: Pixels,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _refresh_task: Option<Task<()>>,
    _notification_task: Option<Task<()>>,
}

impl Sidebar {
    pub fn new(
        domain: Arc<MuxDomain>,
        session_id: String,
        snapshot: Option<&SessionSnapshot>,
        request_handler: RequestHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter sessions…", window, cx);
            editor
        });
        let subscriptions = vec![cx.subscribe(&filter_editor, |this, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.rebuild_entries(cx);
            }
        })];

        let mut this = Self {
            domain,
            session_id,
            request_handler,
            sessions: Vec::new(),
            tree: snapshot.map(SessionTree::from_snapshot).unwrap_or_default(),
            entries: Vec::new(),
            selected_index: None,
            filter_editor,
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            width: DEFAULT_WIDTH,
            focus_handle,
            _subscriptions: subscriptions,
            _refresh_task: None,
            _notification_task: None,
        };
        this.rebuild_entries(cx);
        this.refresh_sessions(cx);
        this.start_notification_listener(cx);
        this
    }

    /// Pulls the authoritative session list (spec §3.3 push signal, pull data).
    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            match domain.list_sessions().await {
                Ok(sessions) => {
                    if let Err(error) = this.update(cx, |this, cx| {
                        this.sessions = sessions;
                        this.rebuild_entries(cx);
                    }) {
                        tracing::debug!(?error, "sidebar dropped before session list arrived");
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "sidebar failed to list mux sessions");
                }
            }
        }));
    }

    /// Maintains the tree from the lifecycle stream instead of re-attaching.
    fn start_notification_listener(&mut self, cx: &mut Context<Self>) {
        let notifications = self.domain.subscribe();
        self._notification_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(notification) = notifications.recv().await {
                let Some(event) = notification.event else {
                    continue;
                };
                if this
                    .update(cx, |this, cx| this.apply_event(&event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn apply_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        match event {
            // Window membership changes the attached-client counts the session
            // rows display, and is the only signal that a session appeared or
            // went away while this window was running.
            Event::WindowAdded(_) | Event::WindowRemoved(_) => self.refresh_sessions(cx),
            other => {
                if self.tree.apply_event(other) {
                    self.rebuild_entries(cx);
                }
            }
        }
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        let query = self.filter_editor.read(cx).text(cx);
        let entries = build_entries(&self.sessions, &self.session_id, &self.tree);
        self.entries = filter_entries(&entries, &query);
        self.selected_index = match self.entries.len() {
            0 => None,
            length => self.selected_index.map(|index| index.min(length - 1)),
        };
        // Resetting drops the scroll position, so only do it when the row count
        // actually moved: title and zoom updates must not scroll the list home.
        if self.list_state.item_count() != self.entries.len() {
            self.list_state.reset(self.entries.len());
        }
        cx.notify();
    }

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        self.selected_index = index.filter(|index| *index < self.entries.len());
        if let Some(index) = self.selected_index {
            self.list_state.scroll_to_reveal_item(index);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let next = move_selection(self.selected_index, self.entries.len(), SelectionMove::Next);
        self.select(next, cx);
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous =
            move_selection(self.selected_index, self.entries.len(), SelectionMove::Previous);
        self.select(previous, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        let first = move_selection(self.selected_index, self.entries.len(), SelectionMove::First);
        self.select(first, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        let last = move_selection(self.selected_index, self.entries.len(), SelectionMove::Last);
        self.select(last, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index {
            self.activate_entry(index, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.select(None, cx);
    }

    fn activate_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let Some(request) = entry.request() else {
            return;
        };
        self.selected_index = Some(index);
        // Cloned out of `self` so the handler can freely touch this window.
        let handler = self.request_handler.clone();
        handler(request, window, cx);
        cx.notify();
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.filter_editor.focus_handle(cx);
        window.focus(&focus, cx);
    }

    fn render_entry(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.entries.get(index).cloned() else {
            return div().into_any_element();
        };
        let selected = self.selected_index == Some(index);

        let (icon, label, secondary, muted) = match &entry {
            ListEntry::Session {
                name,
                attached_clients,
                is_current,
                ..
            } => (
                IconName::Server,
                name.clone(),
                Some(SharedString::from(if *is_current {
                    format!("{attached_clients} attached · current")
                } else {
                    format!("{attached_clients} attached")
                })),
                false,
            ),
            ListEntry::Tab { title, .. } => (IconName::Tab, title.clone(), None, false),
            ListEntry::Pane {
                title,
                is_alive,
                zoomed,
                has_bell,
                ..
            } => {
                let mut markers = Vec::new();
                if *zoomed {
                    markers.push("zoomed");
                }
                if *has_bell {
                    markers.push("bell");
                }
                if !*is_alive {
                    markers.push("exited");
                }
                (
                    IconName::Terminal,
                    title.clone(),
                    (!markers.is_empty()).then(|| SharedString::from(markers.join(" · "))),
                    !*is_alive,
                )
            }
        };

        let focused_pane = matches!(entry, ListEntry::Pane { is_focused: true, .. });
        let label_color = if muted {
            Color::Disabled
        } else if focused_pane {
            Color::Accent
        } else {
            Color::Default
        };

        ListItem::new(entry.element_id())
            .indent_level(entry.indent_level())
            .toggle_state(selected)
            .start_slot(
                Icon::new(icon)
                    .size(IconSize::Small)
                    .color(if muted { Color::Disabled } else { Color::Muted }),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .color(label_color)
                            .single_line(),
                    )
                    .when_some(secondary, |element, secondary| {
                        element.child(
                            Label::new(secondary)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_entry(index, window, cx);
            }))
            .into_any_element()
    }

    fn render_header(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("sidebar-header")
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(self.filter_editor.clone()),
            )
    }

    fn render_empty_state(&self, _cx: &App) -> impl IntoElement {
        v_flex()
            .id("sidebar-empty")
            .w_full()
            .flex_1()
            .justify_center()
            .items_center()
            .p_4()
            .child(
                Label::new("No mux sessions")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn dispatch_context(&self, _window: &mut Window, _cx: &App) -> KeyContext {
        let mut key_context = KeyContext::default();
        key_context.add("WorkspaceSidebar");
        key_context
    }
}

impl WorkspaceSidebar for Sidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        // No `SerializeNeeded` here: the resize handle drives this on every
        // drag frame and persists once the drag ends.
        self.width = width.unwrap_or(DEFAULT_WIDTH).clamp(MIN_WIDTH, MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        !self.tree.bells.is_empty()
    }

    fn side(&self, _cx: &App) -> SidebarSide {
        SidebarSide::Left
    }

    /// Opening the sidebar re-pulls the session list, which is the only part of
    /// the tree the server does not push notifications for.
    ///
    /// Focus deliberately stays on the sidebar container so the arrow keys and
    /// `menu::Confirm` work immediately; the filter is one `FocusFilter` away.
    fn prepare_for_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_sessions(cx);
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        let serialized = SerializedSidebar {
            width: Some(f32::from(self.width)),
        };
        match serde_json::to_string(&serialized) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::error!(%error, "failed to serialize sidebar state");
                None
            }
        }
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match serde_json::from_str::<SerializedSidebar>(state) {
            Ok(serialized) => {
                if let Some(width) = serialized.width {
                    self.width = px(width).clamp(MIN_WIDTH, MAX_WIDTH);
                }
            }
            Err(error) => {
                tracing::error!(%error, "failed to restore sidebar state; keeping defaults");
            }
        }
        cx.notify();
    }
}

impl gpui::EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let background = colors
            .title_bar_background
            .blend(colors.panel_background.opacity(0.25));
        let side = self.side(cx);
        let is_empty = self.entries.is_empty();

        v_flex()
            .id("workspace-sidebar")
            .key_context(self.dispatch_context(window, cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_filter))
            .h_full()
            .w(self.width)
            .bg(background)
            .when(side == SidebarSide::Left, |element| element.border_r_1())
            .when(side == SidebarSide::Right, |element| element.border_l_1())
            .border_color(colors.border)
            .child(self.render_header(window, cx))
            .when(is_empty, |element| {
                element.child(self.render_empty_state(cx))
            })
            .when(!is_empty, |element| {
                element.child(
                    div().flex_1().min_h_0().child(
                        list(self.list_state.clone(), cx.processor(Self::render_entry))
                            .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                            .size_full(),
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::{PaneInfo, TabInfo};

    fn session(id: &str, name: &str, attached_clients: u32) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            name: name.to_string(),
            cwd: "/tmp".to_string(),
            created_timestamp: 0,
            attached_clients,
        }
    }

    fn pane(id: &str, title: &str) -> PaneInfo {
        PaneInfo {
            id: id.to_string(),
            cwd: "/tmp".to_string(),
            title: title.to_string(),
            command: String::new(),
            generation: 0,
            size: None,
            is_alive: true,
            zoomed: false,
        }
    }

    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            tabs: vec![
                TabInfo {
                    id: "tab-1".to_string(),
                    title: "editor".to_string(),
                    panes: vec![pane("pane-1", "vim"), pane("pane-2", "cargo watch")],
                },
                TabInfo {
                    id: "tab-2".to_string(),
                    title: "logs".to_string(),
                    panes: vec![pane("pane-3", "journalctl")],
                },
            ],
            layout: None,
            focused_pane_id: "pane-2".to_string(),
            focused_tab_id: "tab-1".to_string(),
            session_id: "session-a".to_string(),
        }
    }

    fn labels(entries: &[ListEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| format!("{}{}", "  ".repeat(entry.indent_level()), entry.label()))
            .collect()
    }

    #[test]
    fn builds_a_tree_for_the_current_session_only() {
        let tree = SessionTree::from_snapshot(&snapshot());
        let sessions = vec![session("session-a", "work", 1), session("session-b", "spare", 0)];

        let entries = build_entries(&sessions, "session-a", &tree);

        assert_eq!(
            labels(&entries),
            vec![
                "work",
                "  editor",
                "    vim",
                "    cargo watch",
                "  logs",
                "    journalctl",
                "spare",
            ]
        );
        assert!(matches!(
            entries.get(3),
            Some(ListEntry::Pane {
                is_focused: true,
                ..
            })
        ));
    }

    #[test]
    fn falls_back_to_identifiers_when_names_are_empty() {
        let tree = SessionTree::from_snapshot(&SessionSnapshot {
            tabs: vec![TabInfo {
                id: "tab-9".to_string(),
                title: String::new(),
                panes: vec![pane("pane-9", "")],
            }],
            ..SessionSnapshot::default()
        });
        let sessions = vec![session("session-z", "", 0)];

        let entries = build_entries(&sessions, "session-z", &tree);

        assert_eq!(
            labels(&entries),
            vec!["session-z", "  tab tab-9", "    pane pane-9"]
        );
    }

    #[test]
    fn filter_keeps_ancestors_and_descendants_of_a_match() {
        let tree = SessionTree::from_snapshot(&snapshot());
        let sessions = vec![session("session-a", "work", 1), session("session-b", "spare", 0)];
        let entries = build_entries(&sessions, "session-a", &tree);

        let filtered = filter_entries(&entries, "journal");

        assert_eq!(labels(&filtered), vec!["work", "  logs", "    journalctl"]);
    }

    #[test]
    fn filter_on_a_parent_keeps_its_subtree() {
        let tree = SessionTree::from_snapshot(&snapshot());
        let sessions = vec![session("session-a", "work", 1)];
        let entries = build_entries(&sessions, "session-a", &tree);

        let filtered = filter_entries(&entries, "editor");

        assert_eq!(labels(&filtered), vec!["work", "  editor", "    vim", "    cargo watch"]);
    }

    #[test]
    fn filter_is_case_insensitive_and_empty_query_keeps_everything() {
        let tree = SessionTree::from_snapshot(&snapshot());
        let sessions = vec![session("session-a", "work", 1)];
        let entries = build_entries(&sessions, "session-a", &tree);

        assert_eq!(filter_entries(&entries, "   ").len(), entries.len());
        assert_eq!(
            labels(&filter_entries(&entries, "VIM")),
            vec!["work", "  editor", "    vim"]
        );
        assert!(filter_entries(&entries, "nothing-matches").is_empty());
    }

    #[test]
    fn confirming_a_row_targets_the_right_mux_object() {
        let tree = SessionTree::from_snapshot(&snapshot());
        let sessions = vec![session("session-a", "work", 1), session("session-b", "spare", 0)];
        let entries = build_entries(&sessions, "session-a", &tree);

        assert_eq!(
            entries.first().and_then(ListEntry::request),
            Some(SidebarRequest::ActivateSession("session-a".to_string()))
        );
        assert_eq!(
            entries.get(1).and_then(ListEntry::request),
            Some(SidebarRequest::FocusPane("pane-1".to_string())),
            "confirming a tab focuses its first pane"
        );
        assert_eq!(
            entries.get(3).and_then(ListEntry::request),
            Some(SidebarRequest::FocusPane("pane-2".to_string()))
        );
        assert_eq!(
            entries.last().and_then(ListEntry::request),
            Some(SidebarRequest::ActivateSession("session-b".to_string()))
        );
    }

    #[test]
    fn tab_without_panes_has_nothing_to_confirm() {
        let entry = ListEntry::Tab {
            tab_id: "tab-empty".into(),
            title: "empty".into(),
            first_pane_id: None,
        };
        assert_eq!(entry.request(), None);
    }

    #[test]
    fn pane_lifecycle_events_update_the_tree() {
        let mut tree = SessionTree::from_snapshot(&snapshot());

        assert!(tree.apply_event(&Event::PaneAdded(mux_protocol::PaneAdded {
            pane_id: "pane-4".to_string(),
            tab_id: "tab-2".to_string(),
        })));
        assert!(
            !tree.apply_event(&Event::PaneAdded(mux_protocol::PaneAdded {
                pane_id: "pane-4".to_string(),
                tab_id: "tab-2".to_string(),
            })),
            "PaneAdded is at-least-once, so a repeat must not duplicate the row"
        );

        assert!(
            tree.apply_event(&Event::PaneTitleChanged(mux_protocol::PaneTitleChanged {
                pane_id: "pane-4".to_string(),
                title: "htop".to_string(),
            }))
        );
        assert!(tree.apply_event(&Event::PaneZoomed(mux_protocol::PaneZoomed {
            pane_id: "pane-4".to_string(),
            zoomed: true,
        })));

        let entries = build_entries(&[session("s", "s", 1)], "s", &tree);
        assert!(labels(&entries).contains(&"    htop".to_string()));

        assert!(tree.apply_event(&Event::PaneRemoved(mux_protocol::PaneRemoved {
            pane_id: "pane-4".to_string(),
            exit_code: 0,
        })));
        assert!(!tree.contains_pane("pane-4"));
    }

    #[test]
    fn removing_the_last_pane_drops_its_tab() {
        let mut tree = SessionTree::from_snapshot(&snapshot());

        assert!(tree.apply_event(&Event::PaneRemoved(mux_protocol::PaneRemoved {
            pane_id: "pane-3".to_string(),
            exit_code: 0,
        })));

        assert!(!tree.tabs.iter().any(|tab| tab.id == "tab-2"));
    }

    #[test]
    fn layout_changes_prune_zombie_panes() {
        let mut tree = SessionTree::from_snapshot(&snapshot());
        let layout = mux_protocol::LayoutTree {
            root: Some(mux_protocol::LayoutNode {
                id: "root".to_string(),
                node: Some(mux_protocol::layout_node::Node::Pane(
                    mux_protocol::PaneLeaf {
                        pane_id: "pane-1".to_string(),
                    },
                )),
            }),
        };

        assert!(
            tree.apply_event(&Event::SessionLayoutChanged(
                mux_protocol::SessionLayoutChanged {
                    layout: Some(layout),
                }
            ))
        );

        assert!(tree.contains_pane("pane-1"));
        assert!(!tree.contains_pane("pane-2"));
        assert!(!tree.contains_pane("pane-3"));
    }

    #[test]
    fn bells_are_tracked_until_the_pane_is_focused() {
        let mut tree = SessionTree::from_snapshot(&snapshot());

        assert!(tree.apply_event(&Event::PaneBell(mux_protocol::PaneBell {
            pane_id: "pane-1".to_string(),
        })));
        assert!(tree.bells.contains("pane-1"));
        assert!(
            !tree.apply_event(&Event::PaneBell(mux_protocol::PaneBell {
                pane_id: "unknown".to_string(),
            })),
            "a bell for a pane this session does not own must be ignored"
        );

        assert!(tree.apply_event(&Event::PaneFocused(mux_protocol::PaneFocused {
            pane_id: "pane-1".to_string(),
        })));
        assert!(tree.bells.is_empty());
    }

    #[test]
    fn high_frequency_events_do_not_rebuild_the_tree() {
        let mut tree = SessionTree::from_snapshot(&snapshot());
        assert!(!tree.apply_event(&Event::PaneDirty(mux_protocol::PaneDirty {
            pane_id: "pane-1".to_string(),
        })));
    }

    #[test]
    fn selection_is_clamped_to_the_visible_rows() {
        use SelectionMove::{First, Last, Next, Previous};

        for movement in [Next, Previous, First, Last] {
            assert_eq!(move_selection(None, 0, movement), None);
            assert_eq!(move_selection(Some(3), 0, movement), None);
        }

        assert_eq!(move_selection(None, 3, Next), Some(0));
        assert_eq!(move_selection(None, 3, Previous), Some(2));
        assert_eq!(move_selection(Some(0), 3, Next), Some(1));
        assert_eq!(
            move_selection(Some(2), 3, Next),
            Some(2),
            "stepping past the end must clamp rather than wrap"
        );
        assert_eq!(move_selection(Some(0), 3, Previous), Some(0));
        assert_eq!(move_selection(Some(1), 3, First), Some(0));
        assert_eq!(move_selection(Some(1), 3, Last), Some(2));
    }
}

#[cfg(all(test, unix))]
mod live_tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use mux_protocol::{PaneInfo, TabInfo};
    use std::cell::RefCell;
    use std::os::unix::net::UnixStream;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// A domain whose peer never answers: the sidebar's `list_sessions` pull
    /// stays pending, so these tests exercise the view against a session list
    /// they set explicitly.
    fn test_domain() -> (Arc<MuxDomain>, UnixStream) {
        let (client, server) = UnixStream::pair().expect("create a mux socket pair");
        client
            .set_nonblocking(true)
            .expect("make the mux client nonblocking");
        let domain = MuxDomain::connect_with_blocking_stream(client)
            .map(Arc::new)
            .expect("connect the test mux domain");
        (domain, server)
    }

    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            tabs: vec![TabInfo {
                id: "tab-1".to_string(),
                title: "editor".to_string(),
                panes: vec![
                    PaneInfo {
                        id: "pane-1".to_string(),
                        title: "vim".to_string(),
                        is_alive: true,
                        ..PaneInfo::default()
                    },
                    PaneInfo {
                        id: "pane-2".to_string(),
                        title: "cargo watch".to_string(),
                        is_alive: true,
                        ..PaneInfo::default()
                    },
                ],
            }],
            focused_pane_id: "pane-1".to_string(),
            session_id: "session-a".to_string(),
            ..SessionSnapshot::default()
        }
    }

    fn sessions() -> Vec<SessionInfo> {
        vec![
            SessionInfo {
                id: "session-a".to_string(),
                name: "work".to_string(),
                ..SessionInfo::default()
            },
            SessionInfo {
                id: "session-b".to_string(),
                name: "spare".to_string(),
                ..SessionInfo::default()
            },
        ]
    }

    struct Harness {
        sidebar: Entity<Sidebar>,
        requests: Rc<RefCell<Vec<SidebarRequest>>>,
        _domain: Arc<MuxDomain>,
        _peer: UnixStream,
    }

    fn harness(cx: &mut TestAppContext) -> (Harness, &mut VisualTestContext) {
        init_test(cx);
        let (domain, peer) = test_domain();
        let requests = Rc::new(RefCell::new(Vec::new()));
        let (sidebar, cx) = cx.add_window_view({
            let domain = domain.clone();
            let requests = requests.clone();
            move |window, cx| {
                let mut sidebar = Sidebar::new(
                    domain,
                    "session-a".to_string(),
                    Some(&snapshot()),
                    Rc::new(move |request, _window, _cx| {
                        requests.borrow_mut().push(request);
                    }),
                    window,
                    cx,
                );
                sidebar.sessions = sessions();
                sidebar.rebuild_entries(cx);
                sidebar
            }
        });
        (
            Harness {
                sidebar,
                requests,
                _domain: domain,
                _peer: peer,
            },
            cx,
        )
    }

    #[gpui::test]
    async fn selecting_and_confirming_a_pane_row_asks_to_focus_it(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);

        harness.sidebar.update_in(cx, |sidebar, window, cx| {
            assert_eq!(sidebar.entries.len(), 5, "session, tab, two panes, session");
            assert_eq!(sidebar.selected_index, None);

            sidebar.select_first(&SelectFirst, window, cx);
            assert_eq!(sidebar.selected_index, Some(0));
            sidebar.select_next(&SelectNext, window, cx);
            sidebar.select_next(&SelectNext, window, cx);
            sidebar.select_next(&SelectNext, window, cx);
            assert_eq!(sidebar.selected_index, Some(3));
            sidebar.confirm(&Confirm, window, cx);
        });

        assert_eq!(
            harness.requests.borrow().as_slice(),
            [SidebarRequest::FocusPane("pane-2".to_string())]
        );
    }

    #[gpui::test]
    async fn confirming_another_session_row_asks_to_activate_it(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);

        harness.sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.select_last(&SelectLast, window, cx);
            assert_eq!(sidebar.selected_index, Some(4));
            sidebar.confirm(&Confirm, window, cx);
        });

        assert_eq!(
            harness.requests.borrow().as_slice(),
            [SidebarRequest::ActivateSession("session-b".to_string())]
        );
    }

    #[gpui::test]
    async fn cancel_clears_the_selection_and_confirm_then_does_nothing(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);

        harness.sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.select_first(&SelectFirst, window, cx);
            sidebar.cancel(&Cancel, window, cx);
            assert_eq!(sidebar.selected_index, None);
            sidebar.confirm(&Confirm, window, cx);
        });

        assert!(harness.requests.borrow().is_empty());
    }

    #[gpui::test]
    async fn editing_the_filter_rebuilds_and_reclamps_the_list(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);

        harness.sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.select_last(&SelectLast, window, cx);
            assert_eq!(sidebar.selected_index, Some(4));
            sidebar
                .filter_editor
                .update(cx, |editor, cx| editor.set_text("cargo", window, cx));
        });
        cx.run_until_parked();

        harness.sidebar.read_with(cx, |sidebar, _| {
            let labels: Vec<String> = sidebar
                .entries
                .iter()
                .map(|entry| entry.label().to_string())
                .collect();
            assert_eq!(labels, vec!["work", "editor", "cargo watch"]);
            assert_eq!(
                sidebar.selected_index,
                Some(2),
                "a selection past the filtered end must be clamped back in range"
            );
        });
    }

    #[gpui::test]
    async fn a_pane_bell_raises_the_sidebar_notification_flag(cx: &mut TestAppContext) {
        let (harness, cx) = harness(cx);

        harness.sidebar.update(cx, |sidebar, cx| {
            assert!(!sidebar.has_notifications(cx));
            sidebar.apply_event(
                &Event::PaneBell(mux_protocol::PaneBell {
                    pane_id: "pane-1".to_string(),
                }),
                cx,
            );
            assert!(sidebar.has_notifications(cx));
        });
    }
}
