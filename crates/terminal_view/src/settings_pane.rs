//! SettingsPane — workspace item for viewing z3rm settings JSON.
//!
//! §2.1 spec: "settings_ui, settings_profile_selector — retained (settings pane as workspace item)"
//! Wraps a read-only Editor displaying the current settings content.

use std::sync::Arc;

use editor::{Editor, EditorEvent, EditorMode, MultiBuffer};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, Window, div,
};
use language::{Buffer, Capability, LanguageRegistry};
use ui::prelude::*;
use workspace::{
    item::{Item, ItemEvent, TabTooltipContent},
    ItemHandle, ToolbarItemLocation,
};

/// A settings viewer backed by the retained editor crate.
/// Displays the current settings JSON with syntax highlighting.
pub struct SettingsPane {
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    _editor_subscription: Subscription,
}

impl SettingsPane {
    /// Creates a new SettingsPane displaying the given settings JSON content.
    pub fn new(
        settings_content: String,
        languages: Option<Arc<LanguageRegistry>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let languages_for_buffer = languages.clone();

        let buffer = cx.new(|cx| {
            let mut buf = Buffer::local(settings_content, cx);
            buf.set_capability(Capability::ReadOnly, cx);
            if let Some(langs) = languages_for_buffer {
                buf.set_language_registry(langs);
            }
            buf
        });

        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let editor = cx.new(|cx| {
            let mut editor = Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
            editor.set_read_only(true);
            editor
        });

        let focus_handle = editor.focus_handle(cx);
        let editor_subscription = cx.observe(&editor, move |_, cx| {
            // Read-only pane: no meaningful state change to propagate
            let _ = cx;
        });

        cx.new(move |_cx| Self {
            editor,
            focus_handle,
            _editor_subscription: editor_subscription,
        })
    }
}

impl Focusable for SettingsPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for SettingsPane {}

impl Render for SettingsPane {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

impl Item for SettingsPane {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Settings".into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some("Settings".into())
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        self.tab_tooltip_text(cx).map(TabTooltipContent::Text)
    }

    fn is_dirty(&self, _cx: &App) -> bool {
        false
    }

    fn can_save(&self, _cx: &App) -> bool {
        false
    }

    fn can_split(&self) -> bool {
        false
    }

    fn breadcrumb_location(&self, _cx: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::Hidden
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
}
