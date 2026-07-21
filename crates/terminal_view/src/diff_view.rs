use std::any::TypeId;

use editor::{Editor, EditorEvent, EditorMode, MultiBuffer};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Window, div,
};
use language::Capability;
use ui::{Color, Icon, IconName, Label, LabelSize, prelude::*};
use workspace::{
    item::{HighlightedText, Item, ItemEvent, TabContentParams, TabTooltipContent},
};

/// §16.6 DiffView — read-only editor for unified diff content.
///
/// Wraps the retained editor crate to display unified diff text (as produced
/// by `git diff` or the shadow-snapshot version tree) with full tree-sitter
/// syntax highlighting, line numbers, search, and folding — but no editing,
/// LSP, or completions. This is the "diff review" surface for CLI agent
/// workflows described in §16.6.
///
/// Unlike `FileViewer`, the content is supplied directly as a string rather
/// than read from the filesystem, because the diff may come from an in-memory
/// comparison (e.g. shadow snapshot delta replay) or from a remote mux_server
/// `read_file` RPC rather than a local path.
pub struct DiffView {
    editor: Entity<Editor>,
    title: String,
    focus_handle: FocusHandle,
    _editor_subscription: Subscription,
}

impl DiffView {
    /// Creates a read-only DiffView showing `diff_content` under `title`.
    ///
    /// The diff text is placed in a `Buffer` with `Capability::ReadOnly`,
    /// wrapped in a `MultiBuffer`, and rendered by an `Editor` forced into
    /// `read_only = true`. The title is used for tab labeling.
    pub fn new(
        diff_content: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Build a local read-only buffer. No language registry is wired here:
        // diff content has no dedicated tree-sitter grammar, so we rely on the
        // editor's plain-text rendering. A future enhancement can detect a
        // "diff" language if one is registered.
        let buffer = cx.new(|cx| {
            let mut buf = language::Buffer::local(diff_content, cx);
            buf.set_capability(Capability::ReadOnly, cx);
            buf
        });

        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let editor = cx.new(|cx| {
            let mut editor = Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
            // Enforce read-only at the editor level as well as the buffer level.
            // The retained editor crate's read_only mode is the source of truth
            // for disabling edit action handlers (§2.1 editor pruning).
            editor.set_read_only(true);
            editor.set_searchable(true);
            editor
        });

        let focus_handle = editor.read(cx).focus_handle(cx);

        // Propagate editor lifecycle events as workspace ItemEvents so the
        // tab title / dirty indicator update correctly.
        let editor_subscription =
            cx.subscribe(&editor, |_, _, event, cx| match event {
                EditorEvent::TitleChanged | EditorEvent::DirtyChanged => {
                    cx.emit(ItemEvent::UpdateTab);
                }
                EditorEvent::Edited { .. } => {
                    cx.emit(ItemEvent::Edit);
                }
                _ => {}
            });

        DiffView {
            editor,
            title,
            focus_handle,
            _editor_subscription: editor_subscription,
        }
    }

    /// Returns the inner editor entity.
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    /// Returns the display title for this diff view.
    pub fn title(&self) -> &str {
        &self.title
    }
}

impl EventEmitter<ItemEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Render the editor full-size, matching the FileViewer pattern (§16.6).
        div().size_full().child(self.editor.clone())
    }
}

impl Item for DiffView {
    type Event = ItemEvent;

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        h_flex()
            .gap_1()
            .child(Icon::new(IconName::Diff).color(Color::Muted))
            .child(
                Label::new(self.title.as_str())
                    .color(params.text_color())
                    .size(LabelSize::Small),
            )
            .into_any()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.as_str().into()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.title.as_str().into())
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        self.tab_tooltip_text(cx).map(TabTooltipContent::Text)
    }

    fn to_item_events(event: &ItemEvent, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn is_dirty(&self, _: &App) -> bool {
        // DiffView is a read-only display surface; it is never dirty.
        false
    }

    fn has_deleted_file(&self, _: &App) -> bool {
        false
    }

    fn has_conflict(&self, _: &App) -> bool {
        false
    }

    fn can_save(&self, _: &App) -> bool {
        false
    }

    fn can_split(&self) -> bool {
        // Diff review is a single-pane display; splitting is not meaningful.
        false
    }

    fn capability(&self, _: &App) -> Capability {
        Capability::ReadOnly
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if TypeId::of::<Self>() == type_id {
            Some(self_handle.clone().into())
        } else if TypeId::of::<Editor>() == type_id {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(
        &self,
        _handle: &Entity<Self>,
        _cx: &App,
    ) -> Option<Box<dyn workspace::searchable::SearchableItemHandle>> {
        // Delegate searchability to the inner editor so in-diff search works.
        Some(Box::new(self.editor.clone()))
    }

    fn breadcrumb_location(&self, _: &App) -> workspace::ToolbarItemLocation {
        workspace::ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(
        &self,
        _cx: &App,
    ) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        Some((
            vec![HighlightedText {
                text: self.title.as_str().into(),
                highlights: Vec::new(),
            }],
            None,
        ))
    }
}
