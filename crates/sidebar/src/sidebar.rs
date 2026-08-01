use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyContext, ListState,
    Pixels, Render, SharedString, Window, list, prelude::*, px,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use workspace::{
    MultiWorkspace, Sidebar as WorkspaceSidebar, SidebarEvent, SidebarSide, ToggleWorkspaceSidebar,
    Workspace,
};

// ============================================================
// Constants
// ============================================================

const DEFAULT_WIDTH: Pixels = px(300.0);
const MIN_WIDTH: Pixels = px(200.0);
const MAX_WIDTH: Pixels = px(800.0);

// ============================================================
// Serialization
// ============================================================

#[derive(Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum SerializedSidebarView {
    #[default]
    ThreadList,
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedSidebar {
    #[serde(default)]
    width: Option<f32>,
    #[serde(default)]
    active_view: SerializedSidebarView,
}

// ============================================================
// Sidebar View
// ============================================================

#[derive(Default, Clone, Copy, Debug)]
pub(crate) enum SidebarView {
    #[default]
    ThreadList,
}

// ============================================================
// Sidebar Contents
// ============================================================

#[derive(Default)]
struct SidebarContents {
    entries: Vec<ListEntry>,
    has_open_projects: bool,
    notified_threads: Vec<usize>,
    notified_terminals: Vec<usize>,
}

#[derive(Clone)]
enum ListEntry {
    Empty,
}

// ============================================================
// Sidebar struct
// ============================================================

/// The sidebar panel for the agents workspace.
/// After gutting agent/collab code, this is a minimal dock/panel shell.
pub struct Sidebar {
    multi_workspace: gpui::WeakEntity<MultiWorkspace>,
    width: Pixels,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    list_state: ListState,
    contents: SidebarContents,
    selection: Option<usize>,
    view: SidebarView,
}

// ============================================================
// impl Sidebar (construction + helpers)
// ============================================================

impl Sidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus_in(&focus_handle, window, Self::on_focus_in)
            .detach();

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search…", window, cx);
            editor
        });

        Self {
            multi_workspace: multi_workspace.downgrade(),
            width: DEFAULT_WIDTH,
            focus_handle,
            filter_editor,
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
            contents: SidebarContents::default(),
            selection: None,
            view: SidebarView::default(),
        }
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
    }

    fn on_focus_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection = None;
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selection {
            self.selection = Some(ix + 1);
        }
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.selection {
            self.selection = Some(ix.saturating_sub(1));
        }
        cx.notify();
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection = Some(0);
        cx.notify();
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection = None;
        cx.notify();
    }

    fn focus_sidebar_filter(
        &mut self,
        _: &zed_actions::agents_sidebar::FocusSidebarFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.filter_editor.focus_handle(cx);
        window.focus(&focus, cx);
    }

    fn render_sidebar_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let filter_editor = self.filter_editor.clone();

        h_flex()
            .id("sidebar-header")
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .child(div().min_w_0().flex_1().child(filter_editor))
    }

    fn render_empty_state(&self, cx: &App) -> impl IntoElement {
        v_flex()
            .id("sidebar-empty")
            .w_full()
            .h_full()
            .justify_center()
            .items_center()
            .p_4()
            .child(
                Label::new("No threads available")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_sidebar_bottom_bar(&self, _cx: &App) -> impl IntoElement {
        div().id("sidebar-bottom").h_0()
    }

    fn dispatch_context(&self, _window: &mut Window, _cx: &App) -> KeyContext {
        let mut key_context = KeyContext::default();
        key_context.add("WorkspaceSidebar");
        key_context.add("AgentSidebar");
        key_context
    }
}

// ============================================================
// impl WorkspaceSidebar (a.k.a. workspace::Sidebar trait)
// ============================================================

impl WorkspaceSidebar for Sidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(DEFAULT_WIDTH).clamp(MIN_WIDTH, MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        false
    }

    fn side(&self, _cx: &App) -> SidebarSide {
        SidebarSide::Left
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        let serialized = SerializedSidebar {
            width: Some(f32::from(self.width)),
            active_view: SerializedSidebarView::ThreadList,
        };
        serde_json::to_string(&serialized).ok()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(serialized) = serde_json::from_str::<SerializedSidebar>(state).ok() {
            if let Some(width) = serialized.width {
                self.width = px(width).clamp(MIN_WIDTH, MAX_WIDTH);
            }
        }
        cx.notify();
    }
}

// ============================================================
// impl EventEmitter
// ============================================================

impl gpui::EventEmitter<workspace::SidebarEvent> for Sidebar {}

// ============================================================
// impl Focusable
// ============================================================

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ============================================================
// impl Render
// ============================================================

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let side = self.side(cx);

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
            .on_action(cx.listener(Self::focus_sidebar_filter))
            .h_full()
            .w(self.width)
            .bg(bg)
            .when(side == SidebarSide::Left, |el| el.border_r_1())
            .when(side == SidebarSide::Right, |el| el.border_l_1())
            .border_color(color.border)
            .child(self.render_sidebar_header(window, cx))
            .child(self.render_empty_state(cx))
            .child(self.render_sidebar_bottom_bar(cx))
    }
}

// ============================================================
// dump_workspace_info (dev action, kept for z3rm compatibility)
// ============================================================

gpui::actions!(dev, [DumpWorkspaceInfo]);

pub fn dump_workspace_info(
    _workspace: &mut Workspace,
    _: &DumpWorkspaceInfo,
    _window: &mut gpui::Window,
    _cx: &mut gpui::Context<Workspace>,
) {
    // Stub: workspace info dump removed after gutting agent/collab code.
}
