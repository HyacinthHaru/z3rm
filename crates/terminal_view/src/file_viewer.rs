use std::{
    any::TypeId,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent, EditorMode, MultiBuffer};
use fs::Fs;
use gpui::{
    Action, AnyElement, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use language::Capability;
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{Color, Icon, IconName, Label, LabelSize, prelude::*};
use workspace::{
    SplitDirection, Workspace,
    item::{HighlightedText, Item, ItemEvent, TabContentParams, TabTooltipContent},
};

/// Opens a file in a read-only FileViewer pane.
///
/// Dispatchable from the command palette. The path is resolved relative to
/// the first worktree root when it is not absolute.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = file_viewer)]
pub struct OpenFile {
    /// The file path to open. Relative paths are resolved against the
    /// first worktree root.
    pub path: String,
}

/// A read-only file viewer backed by the retained editor crate.
///
/// Renders file content with tree-sitter syntax highlighting, line numbers,
/// search, and folding — but no editing, LSP, or completions (§16.6).
pub struct FileViewer {
    editor: Entity<Editor>,
    path: PathBuf,
    focus_handle: FocusHandle,
    _editor_subscription: Subscription,
}

impl FileViewer {
    /// Opens a file at `abs_path` in a new read-only FileViewer.
    ///
    /// Reads the file from the filesystem, creates a Buffer with
    /// `Capability::ReadOnly`, detects the language via the project's
    /// `LanguageRegistry` for tree-sitter syntax highlighting, wraps the
    /// buffer in a `MultiBuffer`, and constructs an `Editor` with
    /// `read_only = true`.
    pub fn open(
        abs_path: PathBuf,
        project: WeakEntity<project::Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let fs = <dyn Fs>::global(cx);
        let languages = project
            .read_with(cx, |project, _| project.languages().clone())
            .ok();

        window.spawn(cx, async move |cx| {
            let content = fs
                .load(&abs_path)
                .await
                .with_context(|| format!("failed to read file: {}", abs_path.display()))?;

            let path_for_language = abs_path.clone();
            let editor = cx.update(|window, cx| {
                let buffer = cx.new(|cx| {
                    let mut buf = language::Buffer::local(content, cx);
                    buf.set_capability(Capability::ReadOnly, cx);
                    if let Some(langs) = &languages {
                        buf.set_language_registry(langs.clone());
                    }
                    buf
                });

                // Load tree-sitter grammar asynchronously for syntax highlighting.
                if let Some(langs) = languages.clone() {
                    let buffer_clone = buffer.clone();
                    cx.spawn(async move |cx| {
                        if let Ok(language) =
                            langs.load_language_for_file_path(&path_for_language).await
                        {
                            buffer_clone.update(cx, |buf, cx| {
                                buf.set_language(Some(language), cx);
                            });
                        }
                    })
                    .detach();
                }

                let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

                cx.new(|cx| {
                    let mut editor =
                        Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
                    editor.set_read_only(true);
                    editor.set_searchable(true);
                    editor
                })
            })?;

            cx.update(|_, cx| {
                cx.new(|cx| {
                    let focus_handle = editor.read(cx).focus_handle(cx);
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
                    FileViewer {
                        editor,
                        path: abs_path,
                        focus_handle,
                        _editor_subscription: editor_subscription,
                    }
                })
            })
        })
    }

    /// Returns the absolute path of the file being viewed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the inner editor entity.
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

impl EventEmitter<ItemEvent> for FileViewer {}

impl Focusable for FileViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileViewer {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

impl Item for FileViewer {
    type Event = ItemEvent;

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        h_flex()
            .gap_1()
            .child(Icon::new(IconName::FileCode).color(Color::Muted))
            .child(
                Label::new(self.file_name())
                    .color(params.text_color())
                    .size(LabelSize::Small),
            )
            .into_any()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.file_name().into()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.path.display().to_string().into())
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        self.tab_tooltip_text(cx).map(TabTooltipContent::Text)
    }

    fn to_item_events(event: &ItemEvent, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn is_dirty(&self, _: &App) -> bool {
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
        Some(Box::new(self.editor.clone()))
    }

    fn breadcrumb_location(&self, _: &App) -> workspace::ToolbarItemLocation {
        workspace::ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(
        &self,
        _cx: &App,
    ) -> Option<(Vec<HighlightedText>, Option<gpui::Font>)> {
        let mut segments = Vec::new();
        if let Some(parent) = self.path.parent() {
            segments.push(HighlightedText {
                text: parent.display().to_string().into(),
                highlights: Vec::new(),
            });
        }
        segments.push(HighlightedText {
            text: self.file_name().into(),
            highlights: Vec::new(),
        });
        Some((segments, None))
    }
}

/// Opens a file in a FileViewer, split right from the active pane (§16.6).
///
/// This is the primary entry point used by terminal path detection and
/// the command palette.
pub fn open_file_in_viewer(
    workspace: &mut Workspace,
    abs_path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().downgrade();

    // §16.6: auto-split-right when the center pane has only terminals.
    let active_pane = workspace.active_pane().clone();
    let split_direction = SplitDirection::Right;
    let target_pane = workspace
        .find_pane_in_direction(split_direction, cx)
        .unwrap_or_else(|| {
            workspace.split_pane(active_pane.clone(), split_direction, window, cx)
        });

    let open_task = FileViewer::open(abs_path, project, window, cx);
    cx.spawn_in(window, async move |_workspace, cx| {
        match open_task.await {
            Ok(viewer) => {
                target_pane
                    .update_in(cx, |pane, window, cx| {
                        pane.add_item(Box::new(viewer), true, true, None, window, cx);
                    })
                    .ok();
            }
            Err(err) => {
                log::error!("failed to open file viewer: {err:#}");
            }
        }
    })
    .detach();
}
