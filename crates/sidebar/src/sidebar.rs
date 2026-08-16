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

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyContext,
    ListAlignment, ListState, Pixels, Render, Role, SharedString, Subscription, Task, Window, list,
    prelude::*, px,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use mux::MuxDomain;
use mux_protocol::{SessionInfo, SessionSnapshot, notification::Event};
use serde::{Deserialize, Serialize};
use ui::{ListItem, Tooltip, prelude::*};
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
    OpenFile {
        session_id: String,
        path: String,
    },
    /// §4 Review a file that shadow snapshot has recorded versions for.
    ReviewFile {
        session_id: String,
        path: String,
    },
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
            Event::SessionLayoutChanged(changed) => {
                if let Some(snapshot) = changed.snapshot.as_ref() {
                    let mut next = Self::from_snapshot(snapshot);
                    next.bells = self
                        .bells
                        .iter()
                        .filter(|pane_id| next.contains_pane(pane_id))
                        .cloned()
                        .collect();
                    if *self == next {
                        false
                    } else {
                        *self = next;
                        true
                    }
                } else {
                    changed
                        .layout
                        .as_ref()
                        .is_some_and(|layout| {
                            self.retain_layout_panes(&LayoutTree::from_proto(layout))
                        })
                }
            }
            _ => false,
        }
    }
}

// ============================================================
// Lazy session file tree
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarMode {
    Sessions,
    Files,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileNode {
    path: String,
    name: String,
    is_dir: bool,
    size: u64,
    is_modified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectoryLoad {
    Unloaded,
    Loading,
    Loaded(Vec<FileNode>),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryNode {
    expanded: bool,
    load: DirectoryLoad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileTree {
    session_id: String,
    root_path: String,
    directories: HashMap<String, DirectoryNode>,
}

impl FileTree {
    fn new(session_id: impl Into<String>, root_path: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let root_path = root_path.into();
        let mut directories = HashMap::new();
        directories.insert(
            root_path.clone(),
            DirectoryNode {
                expanded: true,
                load: DirectoryLoad::Unloaded,
            },
        );
        Self {
            session_id,
            root_path,
            directories,
        }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn root_path(&self) -> &str {
        &self.root_path
    }

    fn directory(&self, path: &str) -> Option<&DirectoryNode> {
        self.directories.get(path)
    }

    fn rebind(&mut self, session_id: &str, root_path: &str) -> bool {
        if self.session_id == session_id && self.root_path == root_path {
            return false;
        }
        *self = Self::new(session_id, root_path);
        true
    }

    fn start_loading(&mut self, path: &str) -> Option<String> {
        let directory = self.directories.get_mut(path)?;
        if !matches!(
            &directory.load,
            DirectoryLoad::Unloaded | DirectoryLoad::Error(_)
        ) {
            return None;
        }
        directory.expanded = true;
        directory.load = DirectoryLoad::Loading;
        Some(path.to_string())
    }

    fn toggle_directory(&mut self, path: &str) -> Option<String> {
        let directory = self.directories.get_mut(path)?;
        if matches!(&directory.load, DirectoryLoad::Loaded(_)) {
            directory.expanded = !directory.expanded;
            return None;
        }
        if matches!(
            &directory.load,
            DirectoryLoad::Unloaded | DirectoryLoad::Error(_)
        ) {
            directory.expanded = true;
            directory.load = DirectoryLoad::Loading;
            return Some(path.to_string());
        }
        None
    }

    fn complete_loading(&mut self, path: &str, mut entries: Vec<mux_protocol::DirEntry>) -> bool {
        if !matches!(
            self.directories.get(path).map(|directory| &directory.load),
            Some(DirectoryLoad::Loading)
        ) {
            return false;
        }

        sort_directory_entries(&mut entries);
        let nodes: Vec<FileNode> = entries
            .into_iter()
            .map(|entry| FileNode {
                path: join_remote_path(path, &entry.name),
                name: entry.name,
                is_dir: entry.is_dir,
                size: entry.size,
                is_modified: entry.is_modified,
            })
            .collect();
        for node in &nodes {
            if node.is_dir {
                self.directories
                    .entry(node.path.clone())
                    .or_insert(DirectoryNode {
                        expanded: false,
                        load: DirectoryLoad::Unloaded,
                    });
            }
        }
        let Some(directory) = self.directories.get_mut(path) else {
            return false;
        };
        directory.load = DirectoryLoad::Loaded(nodes);
        true
    }

    fn fail_loading(&mut self, path: &str, error: String) -> bool {
        let Some(directory) = self.directories.get_mut(path) else {
            return false;
        };
        if !matches!(&directory.load, DirectoryLoad::Loading) {
            return false;
        }
        directory.load = DirectoryLoad::Error(error);
        true
    }
}

fn join_remote_path(parent: &str, name: &str) -> String {
    if parent.is_empty() || parent == "." {
        return name.to_string();
    }
    let separator = if parent.contains('\\') && !parent.contains('/') {
        '\\'
    } else {
        '/'
    };
    if parent.chars().all(|character| character == separator) {
        return format!("{parent}{name}");
    }
    format!(
        "{}{separator}{name}",
        parent.trim_end_matches(separator)
    )
}

fn sort_directory_entries(entries: &mut [mux_protocol::DirEntry]) {
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.cmp(&right.name))
    });
}

// ============================================================
// List entries
// ============================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryStatus {
    Unloaded,
    Loading,
    Loaded,
    Error(SharedString),
}

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
    Directory {
        path: SharedString,
        name: SharedString,
        depth: usize,
        expanded: bool,
        status: DirectoryStatus,
    },
    File {
        session_id: SharedString,
        path: SharedString,
        name: SharedString,
        depth: usize,
        size: u64,
        is_modified: bool,
    },
}

impl ListEntry {
    fn indent_level(&self) -> usize {
        match self {
            ListEntry::Session { .. } => 0,
            ListEntry::Tab { .. } => 1,
            ListEntry::Pane { .. } => 2,
            ListEntry::Directory { depth, .. } | ListEntry::File { depth, .. } => *depth,
        }
    }

    /// Whether this row can be collapsed, and if so its current state. Rows
    /// that never collapse stay `None` so they are not announced as
    /// collapsible.
    fn expanded(&self) -> Option<bool> {
        match self {
            ListEntry::Directory { expanded, .. } => Some(*expanded),
            _ => None,
        }
    }

    /// The name assistive technology announces for this row.
    ///
    /// Screen readers get no icon, so the row kind is spoken first; the
    /// trailing detail mirrors the secondary label shown next to the name.
    fn accessible_name(&self) -> String {
        let kind = match self {
            ListEntry::Session { .. } => "Session",
            ListEntry::Tab { .. } => "Tab",
            ListEntry::Pane { .. } => "Pane",
            ListEntry::Directory { .. } => "Folder",
            ListEntry::File { .. } => "File",
        };

        let mut detail = Vec::new();
        match self {
            ListEntry::Session {
                attached_clients,
                is_current,
                ..
            } => {
                detail.push(format!("{attached_clients} attached"));
                if *is_current {
                    detail.push("current".to_string());
                }
            }
            ListEntry::Pane {
                is_alive,
                zoomed,
                has_bell,
                is_focused,
                ..
            } => {
                if *is_focused {
                    detail.push("focused".to_string());
                }
                if *zoomed {
                    detail.push("zoomed".to_string());
                }
                if *has_bell {
                    detail.push("bell".to_string());
                }
                if !*is_alive {
                    detail.push("exited".to_string());
                }
            }
            ListEntry::File {
                size, is_modified, ..
            } => {
                detail.push(format_file_size(*size).to_string());
                if *is_modified {
                    detail.push("modified".to_string());
                }
            }
            ListEntry::Directory { status, .. } => match status {
                DirectoryStatus::Loading => detail.push("loading".to_string()),
                DirectoryStatus::Error(error) => detail.push(error.to_string()),
                DirectoryStatus::Unloaded | DirectoryStatus::Loaded => {}
            },
            ListEntry::Tab { .. } => {}
        }

        let name = format!("{kind} {}", self.label());
        if detail.is_empty() {
            name
        } else {
            format!("{name}, {}", detail.join(", "))
        }
    }

    fn label(&self) -> &SharedString {
        match self {
            ListEntry::Session { name, .. } => name,
            ListEntry::Tab { title, .. } => title,
            ListEntry::Pane { title, .. } => title,
            ListEntry::Directory { name, .. } | ListEntry::File { name, .. } => name,
        }
    }

    fn element_id(&self) -> SharedString {
        match self {
            ListEntry::Session { session_id, .. } => format!("session-{session_id}").into(),
            ListEntry::Tab { tab_id, .. } => format!("tab-{tab_id}").into(),
            ListEntry::Pane { pane_id, .. } => format!("pane-{pane_id}").into(),
            ListEntry::Directory { path, .. } => format!("directory-{path}").into(),
            ListEntry::File { path, .. } => format!("file-{path}").into(),
        }
    }
    fn is_session(&self) -> bool {
        matches!(self, ListEntry::Session { .. })
    }

    fn is_pane(&self) -> bool {
        matches!(self, ListEntry::Pane { .. })
    }

    fn request(&self) -> Option<SidebarRequest> {
        match self {
            ListEntry::Session { session_id, .. } => {
                Some(SidebarRequest::ActivateSession(session_id.to_string()))
            }
            ListEntry::Tab { first_pane_id, .. } => first_pane_id
                .as_ref()
                .map(|pane_id| SidebarRequest::FocusPane(pane_id.to_string())),
            ListEntry::Pane { pane_id, .. } => Some(SidebarRequest::FocusPane(pane_id.to_string())),
            ListEntry::File {
                session_id, path, ..
            } => Some(SidebarRequest::OpenFile {
                session_id: session_id.to_string(),
                path: path.to_string(),
            }),
            ListEntry::Directory { .. } => None,
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

fn build_file_entries(tree: &FileTree) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    append_directory_entries(
        tree,
        tree.root_path(),
        SharedString::from(tree.root_path().to_string()),
        0,
        &mut entries,
    );
    entries
}

fn append_directory_entries(
    tree: &FileTree,
    path: &str,
    name: SharedString,
    depth: usize,
    entries: &mut Vec<ListEntry>,
) {
    let Some(directory) = tree.directory(path) else {
        return;
    };
    let status = match &directory.load {
        DirectoryLoad::Unloaded => DirectoryStatus::Unloaded,
        DirectoryLoad::Loading => DirectoryStatus::Loading,
        DirectoryLoad::Loaded(_) => DirectoryStatus::Loaded,
        DirectoryLoad::Error(error) => DirectoryStatus::Error(error.clone().into()),
    };
    entries.push(ListEntry::Directory {
        path: path.to_string().into(),
        name,
        depth,
        expanded: directory.expanded,
        status,
    });
    if !directory.expanded {
        return;
    }
    let DirectoryLoad::Loaded(children) = &directory.load else {
        return;
    };
    for child in children {
        if child.is_dir {
            append_directory_entries(
                tree,
                &child.path,
                child.name.clone().into(),
                depth + 1,
                entries,
            );
        } else {
            entries.push(ListEntry::File {
                session_id: tree.session_id().to_string().into(),
                path: child.path.clone().into(),
                name: child.name.clone().into(),
                depth: depth + 1,
                size: child.size,
                is_modified: child.is_modified,
            });
        }
    }
}

fn format_file_size(size: u64) -> SharedString {
    format!("{size} B").into()
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
fn move_selection(current: Option<usize>, length: usize, movement: SelectionMove) -> Option<usize> {
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
    mode: SidebarMode,
    file_tree: Option<FileTree>,
    entries: Vec<ListEntry>,
    selected_index: Option<usize>,
    filter_editor: Entity<Editor>,
    list_state: ListState,
    width: Pixels,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _refresh_task: Option<Task<()>>,
    _notification_task: Option<Task<()>>,
    _directory_tasks: HashMap<String, Task<()>>,
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
            editor.set_placeholder_text("Filter…", window, cx);
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
            mode: SidebarMode::Sessions,
            file_tree: None,
            entries: Vec::new(),
            selected_index: None,
            filter_editor,
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            width: DEFAULT_WIDTH,
            focus_handle,
            _subscriptions: subscriptions,
            _refresh_task: None,
            _notification_task: None,
            _directory_tasks: HashMap::new(),
        };
        this.rebuild_entries(cx);
        this.refresh_sessions(cx);
        this.start_notification_listener(cx);
        this
    }

    pub fn rebind_session(
        &mut self,
        session_id: String,
        snapshot: Option<&SessionSnapshot>,
        cx: &mut Context<Self>,
    ) {
        let session_changed = self.session_id != session_id;
        self.session_id = session_id;
        self.tree = snapshot.map(SessionTree::from_snapshot).unwrap_or_default();
        if session_changed {
            self.file_tree = None;
            self._directory_tasks.clear();
        }
        self.rebuild_entries(cx);
        self.refresh_sessions(cx);
    }

    fn set_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.selected_index = None;
        let root_to_load = (mode == SidebarMode::Files)
            .then(|| self.start_root_load())
            .flatten();
        self.rebuild_entries(cx);
        if let Some(path) = root_to_load {
            self.spawn_directory_load(path, cx);
        }
    }

    fn bind_file_tree_to_active_session(&mut self) -> bool {
        let Some(cwd) = self
            .sessions
            .iter()
            .find(|session| session.id == self.session_id)
            .map(|session| session.cwd.clone())
        else {
            let changed = self.file_tree.take().is_some();
            if changed {
                self._directory_tasks.clear();
            }
            return changed;
        };

        let changed = match &mut self.file_tree {
            Some(tree) => tree.rebind(&self.session_id, &cwd),
            None => {
                self.file_tree = Some(FileTree::new(&self.session_id, cwd));
                true
            }
        };
        if changed {
            self._directory_tasks.clear();
        }
        changed
    }

    fn start_root_load(&mut self) -> Option<String> {
        let tree = self.file_tree.as_mut()?;
        let root_path = tree.root_path().to_string();
        tree.start_loading(&root_path)
    }

    fn spawn_directory_load(&mut self, path: String, cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        let session_id = self.session_id.clone();
        let task_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = domain
                .list_dir(&task_path)
                .await
                .map(|listing| listing.entries)
                .map_err(|error| error.to_string());
            if let Err(error) = this.update(cx, |this, cx| {
                if this.session_id != session_id {
                    return;
                }
                let Some(tree) = this.file_tree.as_mut() else {
                    return;
                };
                if tree.session_id() != session_id.as_str() {
                    return;
                }
                let changed = match result {
                    Ok(entries) => tree.complete_loading(&task_path, entries),
                    Err(error) => tree.fail_loading(&task_path, error),
                };
                if changed {
                    this.rebuild_entries(cx);
                }
            }) {
                tracing::debug!(?error, "sidebar dropped before directory listing arrived");
            }
        });
        self._directory_tasks.insert(path, task);
    }

    /// Pulls the authoritative session list (spec §3.3 push signal, pull data).
    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            match domain.list_sessions().await {
                Ok(sessions) => {
                    if let Err(error) = this.update(cx, |this, cx| {
                        this.sessions = sessions;
                        this.bind_file_tree_to_active_session();
                        let root_to_load = (this.mode == SidebarMode::Files)
                            .then(|| this.start_root_load())
                            .flatten();
                        this.rebuild_entries(cx);
                        if let Some(path) = root_to_load {
                            this.spawn_directory_load(path, cx);
                        }
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
        let entries = match self.mode {
            SidebarMode::Sessions => build_entries(&self.sessions, &self.session_id, &self.tree),
            SidebarMode::Files => self
                .file_tree
                .as_ref()
                .map(build_file_entries)
                .unwrap_or_default(),
        };
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
        let previous = move_selection(
            self.selected_index,
            self.entries.len(),
            SelectionMove::Previous,
        );
        self.select(previous, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        let first = move_selection(
            self.selected_index,
            self.entries.len(),
            SelectionMove::First,
        );
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
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        self.selected_index = Some(index);
        if let ListEntry::Directory { path, .. } = &entry {
            let path_to_load = self
                .file_tree
                .as_mut()
                .and_then(|tree| tree.toggle_directory(path));
            self.rebuild_entries(cx);
            if let Some(path) = path_to_load {
                self.spawn_directory_load(path, cx);
            }
            return;
        }
        let Some(request) = entry.request() else {
            return;
        };
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
            ListEntry::Directory {
                name,
                expanded,
                status,
                ..
            } => {
                let secondary = match status {
                    DirectoryStatus::Unloaded | DirectoryStatus::Loaded => None,
                    DirectoryStatus::Loading => Some(SharedString::from("loading…")),
                    DirectoryStatus::Error(error) => Some(error.clone()),
                };
                (
                    if *expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    },
                    name.clone(),
                    secondary,
                    false,
                )
            }
            ListEntry::File {
                name,
                size,
                is_modified,
                ..
            } => {
                let size = format_file_size(*size);
                (
                    IconName::File,
                    name.clone(),
                    Some(if *is_modified {
                        format!("{size} · modified").into()
                    } else {
                        size
                    }),
                    false,
                )
            }
        };

        let review_target = match &entry {
            ListEntry::File {
                session_id,
                path,
                is_modified: true,
                ..
            } => Some((session_id.clone(), path.clone())),
            _ => None,
        };

        let focused_pane = matches!(
            entry,
            ListEntry::Pane {
                is_focused: true,
                ..
            }
        );
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
            // The icon is what tells a sighted user whether a row is a session,
            // a pane or a file, so the kind has to be spoken as part of the
            // name; `indent_level` is only visual, hence the explicit level.
            .aria_role(Role::TreeItem)
            .aria_label(entry.accessible_name())
            .aria_level(entry.indent_level() + 1)
            .when_some(entry.expanded(), ListItem::aria_expanded)
            // Keyboard focus stays on the sidebar container while `Select*`
            // actions move `selected_index`, so without this the current row is
            // never reported.
            .when(selected, ListItem::aria_active_descendant)
            .start_slot(Icon::new(icon).size(IconSize::Small).color(if muted {
                Color::Disabled
            } else {
                Color::Muted
            }))
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
            // §4 A recorded file gets a second action: the row body opens the
            // read-only view, this opens the diff against its last version.
            .when_some(review_target, |element, (session_id, path)| {
                element.end_slot(
                    IconButton::new(("sidebar-review", index), IconName::Diff)
                        .aria_label("Review changes")
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Review changes"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let handler = this.request_handler.clone();
                            handler(
                                SidebarRequest::ReviewFile {
                                    session_id: session_id.to_string(),
                                    path: path.to_string(),
                                },
                                window,
                                cx,
                            );
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_entry(index, window, cx);
            }))
            .into_any_element()
    }

    /// The name announced for the row list.
    ///
    /// The same list renders either mode's rows, so a fixed name would say
    /// "sessions" while the panel shows files. It also must not repeat the mode
    /// buttons' labels, or "Sessions" would name three things in one panel.
    fn tree_label(&self) -> &'static str {
        match self.mode {
            SidebarMode::Sessions => "Sessions and panes",
            SidebarMode::Files => "Session files",
        }
    }

    fn render_header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sidebar-header")
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("sidebar-sessions-mode", "Sessions")
                            .label_size(LabelSize::Small)
                            .toggle_state(self.mode == SidebarMode::Sessions)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_mode(SidebarMode::Sessions, cx);
                            })),
                    )
                    .child(
                        Button::new("sidebar-files-mode", "Files")
                            .label_size(LabelSize::Small)
                            .toggle_state(self.mode == SidebarMode::Files)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_mode(SidebarMode::Files, cx);
                            })),
                    ),
            )
            .child(div().min_w_0().w_full().child(self.filter_editor.clone()))
    }

    fn render_empty_state(&self, _cx: &App) -> impl IntoElement {
        let message = match self.mode {
            SidebarMode::Sessions => "No mux sessions",
            SidebarMode::Files if self.file_tree.is_none() => "Loading session files…",
            SidebarMode::Files => "No matching files",
        };
        v_flex()
            .id("sidebar-empty")
            .w_full()
            .flex_1()
            .justify_center()
            .items_center()
            .p_4()
            .child(
                Label::new(message)
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

    /// The mux sidebar has no separate thread-switcher popup. Focusing its
    /// filter provides the same keyboard-first entry point while keeping the
    /// native controls available without the extension host.
    fn toggle_thread_switcher(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if select_last {
            self.select_last(&SelectLast, window, cx);
        }
        let focus_handle = self.filter_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
    }

    fn cycle_project(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let session_indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.is_session().then_some(index))
            .collect();
        let Some(current_position) = session_indices.iter().position(|index| {
            matches!(
                self.entries.get(*index),
                Some(ListEntry::Session { session_id, .. })
                    if session_id.as_ref() == self.session_id.as_str()
            )
        }) else {
            return;
        };
        let target_position = if forward {
            (current_position + 1) % session_indices.len()
        } else {
            (current_position + session_indices.len() - 1) % session_indices.len()
        };
        self.activate_entry(session_indices[target_position], window, cx);
    }

    fn cycle_thread(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let pane_indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.is_pane().then_some(index))
            .collect();
        let Some(current_position) = pane_indices.iter().position(|index| {
            matches!(
                self.entries.get(*index),
                Some(ListEntry::Pane { pane_id, .. })
                    if self.tree.focused_pane_id.as_deref() == Some(pane_id.as_ref())
            )
        }) else {
            return;
        };
        let target_position = if forward {
            (current_position + 1) % pane_indices.len()
        } else {
            (current_position + pane_indices.len() - 1) % pane_indices.len()
        };
        self.activate_entry(pane_indices[target_position], window, cx);
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
            // A focused element without a role never becomes an accessibility
            // node, so focus is dropped and the selected row's
            // `aria_active_descendant` — which needs a focused ancestor — is
            // discarded with it.
            .role(Role::Complementary)
            .aria_label("Session sidebar")
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
                    div()
                        .id("workspace-sidebar-tree")
                        .role(Role::Tree)
                        .aria_label(self.tree_label())
                        .flex_1()
                        .min_h_0()
                        .child(
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
        let sessions = vec![
            session("session-a", "work", 1),
            session("session-b", "spare", 0),
        ];

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

    /// Indentation and icons carry the hierarchy and the row kind visually.
    /// Neither reaches a screen reader, so the name has to say both, and the
    /// level has to be reported separately from the visual indent.
    #[test]
    fn rows_announce_their_kind_state_and_depth() {
        let tree = SessionTree::from_snapshot(&snapshot());
        let sessions = vec![
            session("session-a", "work", 1),
            session("session-b", "spare", 0),
        ];

        let entries = build_entries(&sessions, "session-a", &tree);
        let announced: Vec<(String, usize)> = entries
            .iter()
            .map(|entry| (entry.accessible_name(), entry.indent_level() + 1))
            .collect();

        assert_eq!(
            announced,
            vec![
                ("Session work, 1 attached, current".to_string(), 1),
                ("Tab editor".to_string(), 2),
                ("Pane vim".to_string(), 3),
                ("Pane cargo watch, focused".to_string(), 3),
                ("Tab logs".to_string(), 2),
                ("Pane journalctl".to_string(), 3),
                ("Session spare, 0 attached".to_string(), 1),
            ]
        );

        // Only collapsible rows report an expanded state; announcing a pane as
        // collapsed would be a lie.
        assert!(
            entries.iter().all(|entry| entry.expanded().is_none()),
            "sessions, tabs and panes are not collapsible"
        );
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
        let sessions = vec![
            session("session-a", "work", 1),
            session("session-b", "spare", 0),
        ];
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

        assert_eq!(
            labels(&filtered),
            vec!["work", "  editor", "    vim", "    cargo watch"]
        );
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
        let sessions = vec![
            session("session-a", "work", 1),
            session("session-b", "spare", 0),
        ];
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
        assert!(
            tree.apply_event(&Event::PaneZoomed(mux_protocol::PaneZoomed {
                pane_id: "pane-4".to_string(),
                zoomed: true,
            }))
        );

        let entries = build_entries(&[session("s", "s", 1)], "s", &tree);
        assert!(labels(&entries).contains(&"    htop".to_string()));

        assert!(
            tree.apply_event(&Event::PaneRemoved(mux_protocol::PaneRemoved {
                pane_id: "pane-4".to_string(),
                exit_code: 0,
            }))
        );
        assert!(!tree.contains_pane("pane-4"));
    }

    #[test]
    fn removing_the_last_pane_drops_its_tab() {
        let mut tree = SessionTree::from_snapshot(&snapshot());

        assert!(
            tree.apply_event(&Event::PaneRemoved(mux_protocol::PaneRemoved {
                pane_id: "pane-3".to_string(),
                exit_code: 0,
            }))
        );

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

        assert!(tree.apply_event(&Event::SessionLayoutChanged(
            mux_protocol::SessionLayoutChanged {
                layout: Some(layout),
                // §15.4 ordinary server layout notifications stay pure deltas.
                snapshot: None,
            }
        )));

        assert!(tree.contains_pane("pane-1"));
        assert!(!tree.contains_pane("pane-2"));
        assert!(!tree.contains_pane("pane-3"));
    }

    /// §15.4 A reconnect resync carries the authoritative snapshot; the tree
    /// must be replaced wholesale — stale panes/tabs pruned, focus, titles
    /// and zoom reconciled — instead of only pruning against the layout.
    #[test]
    fn snapshot_resync_replaces_the_tree_and_drops_zombies() {
        let mut tree = SessionTree::from_snapshot(&snapshot());
        // A pane created while the connection was down must not survive the
        // resync: the reconnect snapshot is authoritative (spec §15.4).
        assert!(tree.apply_event(&Event::PaneAdded(mux_protocol::PaneAdded {
            pane_id: "pane-9".to_string(),
            tab_id: "tab-9".to_string(),
        })));
        assert!(tree.contains_pane("pane-9"));

        // The reconnected snapshot: tab-2 is gone, focus moved to pane-1 and
        // pane-2 is zoomed with a refreshed title.
        let resynced = SessionSnapshot {
            tabs: vec![TabInfo {
                id: "tab-1".to_string(),
                title: "editor".to_string(),
                panes: vec![
                    PaneInfo {
                        id: "pane-1".to_string(),
                        title: "vim".to_string(),
                        is_alive: true,
                        ..Default::default()
                    },
                    PaneInfo {
                        id: "pane-2".to_string(),
                        title: "cargo watch".to_string(),
                        is_alive: true,
                        zoomed: true,
                        ..Default::default()
                    },
                ],
            }],
            focused_pane_id: "pane-1".to_string(),
            ..Default::default()
        };
        assert!(tree.apply_event(&Event::SessionLayoutChanged(
            mux_protocol::SessionLayoutChanged {
                layout: None,
                snapshot: Some(resynced.clone()),
            }
        )));

        assert!(!tree.contains_pane("pane-9"), "zombie pane must be pruned");
        assert!(
            !tree.tabs.iter().any(|tab| tab.id == "tab-9"),
            "zombie tab must be pruned"
        );
        assert!(
            !tree.contains_pane("pane-3"),
            "panes of a tab dropped on the server must go too"
        );
        assert!(tree.contains_pane("pane-1"));
        assert_eq!(tree.focused_pane_id.as_deref(), Some("pane-1"));
        let pane_2 = tree
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .find(|pane| pane.id == "pane-2")
            .expect("pane-2 must survive the resync");
        assert!(pane_2.zoomed, "zoom metadata must be reconciled");
        assert_eq!(pane_2.title, "cargo watch");

        // §15.4 at-least-once: a second identical resync is a no-op.
        assert!(!tree.apply_event(&Event::SessionLayoutChanged(
            mux_protocol::SessionLayoutChanged {
                layout: None,
                snapshot: Some(resynced),
            }
        )));
    }

    /// §15.4 Bell latches survive a resync for panes still present and are
    /// dropped for panes the snapshot no longer contains.
    #[test]
    fn snapshot_resync_keeps_bells_for_surviving_panes() {
        let mut tree = SessionTree::from_snapshot(&snapshot());
        assert!(tree.apply_event(&Event::PaneBell(mux_protocol::PaneBell {
            pane_id: "pane-1".to_string(),
        })));
        assert!(tree.apply_event(&Event::PaneBell(mux_protocol::PaneBell {
            pane_id: "pane-3".to_string(),
        })));

        let resynced = SessionSnapshot {
            tabs: vec![TabInfo {
                id: "tab-1".to_string(),
                title: "editor".to_string(),
                panes: vec![
                    PaneInfo {
                        id: "pane-1".to_string(),
                        title: "vim".to_string(),
                        is_alive: true,
                        ..Default::default()
                    },
                    PaneInfo {
                        id: "pane-2".to_string(),
                        title: "cargo watch".to_string(),
                        is_alive: true,
                        ..Default::default()
                    },
                ],
            }],
            focused_pane_id: "pane-2".to_string(),
            ..Default::default()
        };
        assert!(tree.apply_event(&Event::SessionLayoutChanged(
            mux_protocol::SessionLayoutChanged {
                layout: None,
                snapshot: Some(resynced),
            }
        )));

        assert!(tree.bells.contains("pane-1"));
        assert!(!tree.bells.contains("pane-3"));
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

        assert!(
            tree.apply_event(&Event::PaneFocused(mux_protocol::PaneFocused {
                pane_id: "pane-1".to_string(),
            }))
        );
        assert!(tree.bells.is_empty());
    }

    #[test]
    fn high_frequency_events_do_not_rebuild_the_tree() {
        let mut tree = SessionTree::from_snapshot(&snapshot());
        assert!(
            !tree.apply_event(&Event::PaneDirty(mux_protocol::PaneDirty {
                pane_id: "pane-1".to_string(),
            }))
        );
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

    fn directory_entry(
        name: &str,
        is_dir: bool,
        size: u64,
        is_modified: bool,
    ) -> mux_protocol::DirEntry {
        mux_protocol::DirEntry {
            name: name.to_string(),
            is_dir,
            size,
            is_modified,
        }
    }

    #[test]
    fn joins_remote_paths_without_using_client_path_rules() {
        assert_eq!(join_remote_path("/workspace", "src"), "/workspace/src");
        assert_eq!(join_remote_path("/workspace/", "src"), "/workspace/src");
        assert_eq!(join_remote_path("/", "src"), "/src");
        assert_eq!(join_remote_path(".", "src"), "src");
        assert_eq!(join_remote_path(r"C:\workspace", "src"), r"C:\workspace\src");
        assert_eq!(join_remote_path(r"C:\workspace\", "src"), r"C:\workspace\src");
    }

    #[test]
    fn directory_loading_is_lazy_and_has_explicit_transitions() {
        let mut tree = FileTree::new("session-a", "/workspace");

        assert!(matches!(
            tree.directory("/workspace").map(|directory| &directory.load),
            Some(DirectoryLoad::Unloaded)
        ));
        assert_eq!(
            tree.start_loading("/workspace"),
            Some("/workspace".to_string())
        );
        assert!(matches!(
            tree.directory("/workspace").map(|directory| &directory.load),
            Some(DirectoryLoad::Loading)
        ));
        assert_eq!(tree.start_loading("/workspace"), None);

        assert!(tree.complete_loading(
            "/workspace",
            vec![directory_entry("src", true, 0, false)]
        ));
        assert!(matches!(
            tree.directory("/workspace").map(|directory| &directory.load),
            Some(DirectoryLoad::Loaded(_))
        ));
        assert_eq!(tree.toggle_directory("/workspace"), None);
        assert_eq!(
            tree.directory("/workspace")
                .map(|directory| directory.expanded),
            Some(false)
        );
        assert_eq!(
            tree.toggle_directory("/workspace"),
            None,
            "expanding loaded directories must not issue another RPC"
        );
        assert_eq!(
            tree.directory("/workspace")
                .map(|directory| directory.expanded),
            Some(true)
        );
        assert!(matches!(
            tree.directory("/workspace/src")
                .map(|directory| &directory.load),
            Some(DirectoryLoad::Unloaded)
        ));
        assert_eq!(
            tree.start_loading("/workspace/src"),
            Some("/workspace/src".to_string())
        );
        assert!(tree.fail_loading("/workspace/src", "permission denied".to_string()));
        assert!(matches!(
            tree.directory("/workspace/src")
                .map(|directory| &directory.load),
            Some(DirectoryLoad::Error(error)) if error == "permission denied"
        ));
        assert_eq!(
            tree.start_loading("/workspace/src"),
            Some("/workspace/src".to_string()),
            "activating an errored directory retries it"
        );
    }

    #[test]
    fn directory_rows_are_directories_first_then_deterministic_by_name() {
        let mut entries = vec![
            directory_entry("zeta.txt", false, 4, false),
            directory_entry("beta", true, 0, false),
            directory_entry("Alpha.txt", false, 3, true),
            directory_entry("alpha", true, 0, false),
            directory_entry("alpha.txt", false, 5, false),
        ];

        sort_directory_entries(&mut entries);

        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.is_dir, entry.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (true, "alpha"),
                (true, "beta"),
                (false, "Alpha.txt"),
                (false, "alpha.txt"),
                (false, "zeta.txt"),
            ]
        );
    }

    #[test]
    fn file_rows_keep_remote_identity_size_and_modified_state() {
        let mut tree = FileTree::new("session-a", "/workspace");
        assert_eq!(
            tree.start_loading("/workspace"),
            Some("/workspace".to_string())
        );
        assert!(tree.complete_loading(
            "/workspace",
            vec![directory_entry("notes.txt", false, 42, true)]
        ));

        let rows = build_file_entries(&tree);
        let file = rows.get(1).expect("loaded file row");
        assert!(matches!(
            file,
            ListEntry::File {
                size: 42,
                is_modified: true,
                ..
            }
        ));
        assert_eq!(
            file.request(),
            Some(SidebarRequest::OpenFile {
                session_id: "session-a".to_string(),
                path: "/workspace/notes.txt".to_string(),
            })
        );
    }

    #[test]
    fn rebinding_resets_all_lazily_loaded_directory_state() {
        let mut tree = FileTree::new("session-a", "/workspace-a");
        assert_eq!(
            tree.start_loading("/workspace-a"),
            Some("/workspace-a".to_string())
        );
        assert!(tree.complete_loading(
            "/workspace-a",
            vec![directory_entry("src", true, 0, false)]
        ));
        assert_eq!(
            tree.start_loading("/workspace-a/src"),
            Some("/workspace-a/src".to_string())
        );

        assert!(!tree.rebind("session-a", "/workspace-a"));
        assert!(tree.rebind("session-b", "/workspace-b"));

        assert_eq!(tree.session_id(), "session-b");
        assert_eq!(tree.root_path(), "/workspace-b");
        assert_eq!(tree.directories.len(), 1);
        assert!(matches!(
            tree.directory("/workspace-b")
                .map(|directory| &directory.load),
            Some(DirectoryLoad::Unloaded)
        ));
        assert!(tree.directory("/workspace-a").is_none());
    }
}

#[cfg(all(test, unix))]
mod live_tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use mux_protocol::{PaneInfo, TabInfo};
    use std::cell::RefCell;
    use std::io::{self, Read, Write};
    use std::sync::{Condvar, Mutex};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// The live tests only exercise the sidebar's local projection and request
    /// handling. An immediately closed transport keeps `list_sessions` from
    /// overwriting the fixture's explicit session list, without leaving a mux
    /// I/O worker alive until GPUI tears down the test context.
    struct ClosedStream {
        stopped: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Read for ClosedStream {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for ClosedStream {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test stream closed",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for ClosedStream {
        fn drop(&mut self) {
            let (lock, wake) = &*self.stopped;
            *lock.lock().expect("closed-stream state poisoned") = true;
            wake.notify_one();
        }
    }

    fn test_domain() -> Arc<MuxDomain> {
        let stopped = Arc::new((Mutex::new(false), Condvar::new()));
        let domain = MuxDomain::connect_with_blocking_stream(ClosedStream {
            stopped: stopped.clone(),
        })
        .map(Arc::new)
        .expect("connect the test mux domain");

        let (lock, wake) = &*stopped;
        let stopped = lock.lock().expect("closed-stream state poisoned");
        let _stopped = wake
            .wait_while(stopped, |stopped| !*stopped)
            .expect("closed-stream state poisoned");
        domain
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
    }

    fn harness(cx: &mut TestAppContext) -> (Harness, &mut VisualTestContext) {
        init_test(cx);
        let domain = test_domain();
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

    /// Read the a11y semantics the sidebar actually renders, keyed by role.
    ///
    /// The unit tests above check the strings the sidebar *computes*; this
    /// reads what lands in the tree AccessKit would hand to a screen reader,
    /// which is the only place the `Role::Tree` -> `Role::TreeItem` parenting
    /// can be observed at all.
    fn a11y_tree(cx: &mut TestAppContext, mode: SidebarMode) -> serde_json::Value {
        init_test(cx);
        let domain = test_domain();
        let window = cx.add_window(move |window, cx| {
            let mut sidebar = Sidebar::new(
                domain,
                "session-a".to_string(),
                Some(&snapshot()),
                Rc::new(|_request, _window, _cx| {}),
                window,
                cx,
            );
            sidebar.sessions = sessions();
            sidebar.mode = mode;
            sidebar.selected_index = Some(3);
            sidebar.rebuild_entries(cx);
            sidebar
        });

        // Per window rather than the process-wide environment variable, which
        // would switch accessibility on for the other tests in this binary.
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |view, window, cx| {
                // `aria_active_descendant` is honored only while the container
                // actually holds focus, so an unfocused dump would silently
                // skip the property this asserts on.
                if let Ok(sidebar) = view.downcast::<Sidebar>() {
                    let handle = sidebar.read(cx).focus_handle.clone();
                    window.focus(&handle, cx);
                }
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the sidebar window is still open")
            .expect("activation makes the debug tree available");
        serde_json::from_str(&json).expect("the dump is valid JSON")
    }

    /// Roles and names are only useful if they are actually reachable from the
    /// tree root: a `TreeItem` parented by something other than a `Tree` is not
    /// a tree row as far as assistive technology is concerned.
    #[gpui::test]
    async fn the_rendered_tree_parents_named_rows_under_a_tree_role(cx: &mut TestAppContext) {
        let tree = a11y_tree(cx, SidebarMode::Sessions);
        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

        let node_role = |node: &serde_json::Value| {
            node["aria"]["role"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };

        let (tree_id, tree_node) = nodes
            .iter()
            .find(|(_, node)| node_role(node) == "Tree")
            .expect("the session list must be reported as a tree");
        assert_eq!(
            tree_node["aria"]["label"].as_str(),
            Some("Sessions and panes"),
            "an unnamed tree is announced as just \"tree\""
        );

        // Walk down from the Tree node rather than scanning the whole dump, so
        // rows that render outside it would not count.
        let mut rows = Vec::new();
        let mut pending = vec![tree_id.clone()];
        while let Some(id) = pending.pop() {
            let Some(node) = nodes.get(&id) else { continue };
            if node_role(node) == "TreeItem" {
                rows.push(node.clone());
            }
            if let Some(children) = node["children"].as_array() {
                pending.extend(
                    children
                        .iter()
                        .filter_map(|child| child.as_str().map(str::to_string)),
                );
            }
        }

        let mut announced: Vec<(String, u64, bool)> = rows
            .iter()
            .map(|row| {
                (
                    row["aria"]["label"].as_str().unwrap_or_default().to_string(),
                    row["aria"]["level"].as_u64().unwrap_or_default(),
                    row["aria"]["selected"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        announced.sort();

        assert_eq!(
            announced,
            vec![
                // Selection and mux focus are different things, and the dump
                // keeps them apart: the selected row is the one the sidebar's
                // cursor is on, the focused row is the live pane.
                ("Pane cargo watch".to_string(), 3, true),
                ("Pane vim, focused".to_string(), 3, false),
                ("Session spare, 0 attached".to_string(), 1, false),
                ("Session work, 0 attached, current".to_string(), 1, false),
                ("Tab editor".to_string(), 2, false),
            ],
            "every row under the tree must carry a name, a level, and its selection state"
        );

        // Selection lives on the container, so the current row has to be
        // reported as the active descendant or nothing announces it.
        let active = tree["active_descendant_focus"]
            .as_str()
            .expect("the focused sidebar must report an active descendant");
        assert_eq!(
            nodes[active]["aria"]["label"].as_str(),
            Some("Pane cargo watch"),
            "the active descendant must be the row the sidebar's cursor is on"
        );
    }

    /// Typing in the filter and arrowing through the results is the sidebar's
    /// keyboard-first flow, and it is the one case where the highlight cannot
    /// be announced: focus is in the filter, which is not an ancestor of the
    /// rows, so reporting a row as focused would misstate where the keyboard
    /// is. The claim is dropped — this pins that it is dropped *visibly*.
    #[gpui::test]
    async fn filtering_leaves_the_highlighted_row_unannounced(cx: &mut TestAppContext) {
        init_test(cx);
        let domain = test_domain();
        let window = cx.add_window(move |window, cx| {
            let mut sidebar = Sidebar::new(
                domain,
                "session-a".to_string(),
                Some(&snapshot()),
                Rc::new(|_request, _window, _cx| {}),
                window,
                cx,
            );
            sidebar.sessions = sessions();
            sidebar.selected_index = Some(3);
            sidebar.rebuild_entries(cx);
            sidebar
        });
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |view, window, cx| {
                if let Ok(sidebar) = view.downcast::<Sidebar>() {
                    let filter = sidebar.read(cx).filter_editor.focus_handle(cx);
                    window.focus(&filter, cx);
                }
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the sidebar window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        assert_eq!(
            tree["active_descendant_focus"].as_str(),
            None,
            "a row cannot be reported as focused while the filter holds the keyboard"
        );
        assert_eq!(
            tree["frame"]["active_descendant_without_focus"].as_bool(),
            Some(true),
            "the dropped claim has to be visible in the dump rather than silent"
        );
    }

    /// A node with an interactive role and no name is announced as a bare
    /// "button" or "tree item" with nothing to tell it apart. Checked across
    /// the whole rendered panel rather than per element, so a row or control
    /// added later cannot quietly skip it.
    #[gpui::test]
    async fn every_interactive_node_in_the_panel_has_a_name(cx: &mut TestAppContext) {
        let tree = a11y_tree(cx, SidebarMode::Sessions);
        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "sidebar panel");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "sidebar panel");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "sidebar panel");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "sidebar panel");

        // A screen reader derives "item 2 of 5" and the arrow-key conventions
        // from containment, so a row outside its tree loses all of it.
        let mut parent_of = std::collections::HashMap::new();
        for (id, node) in nodes {
            for child in node["children"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|child| child.as_str())
            {
                parent_of.insert(child.to_string(), id.clone());
            }
        }
        let orphaned: Vec<&str> = nodes
            .iter()
            .filter(|(_, node)| node["aria"]["role"] == "TreeItem")
            .filter(|(id, _)| {
                let mut ancestor = parent_of.get(*id);
                while let Some(current) = ancestor {
                    if nodes[current]["aria"]["role"] == "Tree" {
                        return false;
                    }
                    ancestor = parent_of.get(current);
                }
                true
            })
            .map(|(id, _)| id.as_str())
            .collect();
        assert!(
            orphaned.is_empty(),
            "these rows have no Tree ancestor: {orphaned:?}"
        );
    }

    /// The filter is an `Editor`, and an editor with no element id can carry no
    /// accessibility node at all — so a visible search box contributed nothing
    /// to the tree.
    #[gpui::test]
    async fn the_filter_input_is_exposed_as_a_named_text_input(cx: &mut TestAppContext) {
        let tree = a11y_tree(cx, SidebarMode::Sessions);
        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

        let input = nodes
            .values()
            .find(|node| node["aria"]["role"] == "TextInput")
            .expect("the sidebar filter must be reported as a text input");
        assert!(
            input["aria"]["placeholder"].as_str().is_some_and(|p| !p.is_empty()),
            "the filter's placeholder is the only name it has"
        );
        assert!(
            input["aria"]["text_selection"].is_object(),
            "a text input must expose a caret so typing can be followed"
        );
        assert!(
            input["children"]
                .as_array()
                .is_some_and(|children| !children.is_empty()),
            "the input's content must be readable as text runs"
        );
    }

    /// The tree renders both modes' rows, so a fixed name would announce
    /// "Sessions and panes" over a list of files, and would collide with the
    /// mode buttons that are already called "Sessions" and "Files".
    #[gpui::test]
    async fn the_tree_name_follows_the_sidebar_mode(cx: &mut TestAppContext) {
        init_test(cx);
        let domain = test_domain();
        let window = cx.add_window(move |window, cx| {
            Sidebar::new(
                domain,
                "session-a".to_string(),
                Some(&snapshot()),
                Rc::new(|_request, _window, _cx| {}),
                window,
                cx,
            )
        });

        window
            .update(cx, |sidebar, _, cx| {
                assert_eq!(sidebar.tree_label(), "Sessions and panes");
                sidebar.set_mode(SidebarMode::Files, cx);
                assert_eq!(sidebar.tree_label(), "Session files");
            })
            .expect("the sidebar window is still open");
    }

    /// A dropped focus is invisible in a dump — `gpui_focus` is simply null —
    /// so the frame has to say when focus was discarded and why.
    #[gpui::test]
    async fn the_dump_explains_a_focus_that_produced_no_node(cx: &mut TestAppContext) {
        let tree = a11y_tree(cx, SidebarMode::Sessions);
        assert_eq!(
            tree["frame"]["focus_without_node"].as_str(),
            None,
            "the sidebar has a role, so its focus must reach the tree"
        );
        assert!(
            tree["gpui_focus"].as_str().is_some(),
            "a focused container with a role must be reported as focused"
        );
    }
}
