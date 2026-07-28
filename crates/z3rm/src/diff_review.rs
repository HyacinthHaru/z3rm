//! # Diff Review — CLI agent file modification review (Plan 18, §16.6)
//!
//! Side-by-side diff view for reviewing changes CLI agents make to files.
//! Left pane: previous version (shadow snapshot). Right pane: current content.
//! Accept = keep current. Decline = revert via shadow_snapshot (§4.8).
//!
//! §16.6 Entry points:
//! - file tree sidebar click (handled in project_panel)
//! - command palette `file::openDiff`
//! - terminal output path detection (handled in terminal_view)

use gpui::{
    AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Task, Window, div, px,
};
use std::any::TypeId;
use std::path::PathBuf;
use std::sync::Arc;
use ui::prelude::*;
use workspace::{
    ToolbarItemLocation,
    item::{Item, ItemEvent, TabContentParams},
};

/// §16.6 DiffReview — holds previous + current content for side-by-side display.
pub struct DiffReview {
    /// File path being reviewed
    pub file_path: PathBuf,
    /// Previous content (from shadow snapshot or git HEAD)
    pub previous_content: SharedString,
    /// Current content (from disk)
    pub current_content: SharedString,
    /// Whether the diff has been resolved (accept/decline)
    pub resolved: bool,
    /// Tab title for the workspace item ("Diff: <file_name>")
    title: SharedString,
    /// Focus handle
    focus_handle: FocusHandle,
    restore_target: Option<RestoreTarget>,
    decline_pending: bool,
    decline_error: Option<SharedString>,
}

pub struct RestoreTarget {
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    version_id: u64,
}

impl RestoreTarget {
    pub fn new(domain: Arc<mux::MuxDomain>, session_id: String, version_id: u64) -> Self {
        Self {
            domain,
            session_id,
            version_id,
        }
    }
}

/// §16.6 Events emitted by DiffReview
#[derive(Clone, Debug)]
pub enum DiffReviewEvent {
    /// User accepted the change (file stays at current)
    Accepted,
    /// User declined the change (file reverted to previous)
    Declined,
}

impl DiffReview {
    /// §16.6 Create a new diff review view from file path.
    /// Fetches previous (shadow snapshot) and current content.
    pub fn new(
        file_path: PathBuf,
        previous: String,
        current: String,
        restore_target: Option<RestoreTarget>,
        cx: &mut Context<Self>,
    ) -> Self {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let title = SharedString::from(format!("Diff: {}", file_name));
        Self {
            file_path,
            previous_content: previous.into(),
            current_content: current.into(),
            resolved: false,
            title,
            restore_target,
            decline_pending: false,
            decline_error: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// §16.6 Load both versions from disk + shadow snapshot.
    /// Caller supplies `previous_content` (real shadow-snapshot version when
    /// the Decline/ListVersions RPC is available, otherwise on-disk current
    /// content as a fallback — see `open_diff::init`). The current content is
    /// read from disk inside the spawned task. Returns a DiffReview entity
    /// ready to be added to a workspace pane via `add_item`.
    pub fn load(
        file_path: PathBuf,
        previous_content: String,
        restore_target: Option<RestoreTarget>,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let path = file_path.clone();
        let prev = previous_content.clone();
        cx.spawn(async move |cx| {
            let current = smol::unblock({
                let path = path.clone();
                move || std::fs::read_to_string(&path)
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
            let entity =
                cx.new(|cx| DiffReview::new(path.clone(), prev, current, restore_target, cx));
            Ok(entity)
        })
    }

    /// §16.6 Accept the current version (dismiss diff, file stays).
    pub fn accept(&mut self, cx: &mut Context<Self>) {
        self.resolved = true;
        cx.emit(DiffReviewEvent::Accepted);
        cx.notify();
    }

    pub fn can_decline(&self) -> bool {
        self.restore_target.is_some() && !self.resolved
    }

    pub fn decline(&mut self, cx: &mut Context<Self>) {
        if self.decline_pending || !self.can_decline() {
            return;
        }
        let Some(restore_target) = self.restore_target.as_ref() else {
            return;
        };
        self.decline_pending = true;
        self.decline_error = None;
        let domain = restore_target.domain.clone();
        let session_id = restore_target.session_id.clone();
        let version_id = restore_target.version_id;
        let path = self.file_path.to_string_lossy().into_owned();
        cx.spawn(async move |this, cx| {
            let result = domain
                .decline_file_version(&session_id, &path, version_id)
                .await;
            this.update(cx, |this, cx| {
                this.decline_pending = false;
                match result {
                    Ok(response) if response.restored => {
                        this.resolved = true;
                        cx.emit(DiffReviewEvent::Declined);
                    }
                    Ok(_) => {
                        this.decline_error = Some("shadow restore was not confirmed".into());
                    }
                    Err(error) => {
                        this.decline_error = Some(format!("Decline failed: {error:#}").into());
                    }
                }
                cx.notify();
            })
        })
        .detach();
        cx.notify();
    }

    /// §16.6 Compute line-level diff for display.
    pub fn line_diff(&self) -> Vec<DiffLine> {
        let prev_lines: Vec<&str> = self.previous_content.lines().collect();
        let curr_lines: Vec<&str> = self.current_content.lines().collect();
        let max_len = prev_lines.len().max(curr_lines.len());
        let mut result = Vec::with_capacity(max_len);
        for i in 0..max_len {
            match (prev_lines.get(i), curr_lines.get(i)) {
                (Some(prev), Some(curr)) => {
                    if prev == curr {
                        result.push(DiffLine::Unchanged((*prev).to_string()));
                    } else {
                        result.push(DiffLine::Modified {
                            old: (*prev).to_string(),
                            new: (*curr).to_string(),
                        });
                    }
                }
                (Some(prev), None) => result.push(DiffLine::Removed((*prev).to_string())),
                (None, Some(curr)) => result.push(DiffLine::Added((*curr).to_string())),
                (None, None) => {}
            }
        }
        result
    }
}

/// §16.6 A single line in the diff view.
#[derive(Debug, Clone)]
pub enum DiffLine {
    /// Line present in both versions
    Unchanged(String),
    /// Line present only in current (new)
    Added(String),
    /// Line present only in previous (deleted)
    Removed(String),
    /// Line modified between versions
    Modified { old: String, new: String },
}

impl Focusable for DiffReview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DiffReviewEvent> for DiffReview {}

impl EventEmitter<ItemEvent> for DiffReview {}

impl Render for DiffReview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let bg = colors.editor_background;
        let fg = colors.text;

        let diff_lines = self.line_diff();
        let file_name = self
            .file_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.file_path.to_string_lossy().into_owned());

        // Header
        let header = div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .child(SharedString::from(format!("Diff: {}", file_name)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .id("accept-btn")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(colors.border)
                            .text_color(fg)
                            .child("Accept (a)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.accept(cx);
                            })),
                    )
                    .child(if self.can_decline() {
                        div()
                            .id("decline-btn")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(colors.border)
                            .text_color(fg)
                            .child("Decline (d)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.decline(cx);
                            }))
                    } else {
                        div()
                            .id("decline-unavailable")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(colors.border)
                            .text_color(colors.text_muted)
                            .child("No snapshot")
                    }),
            );

        // Diff body
        let mut body = div().flex().flex_col().py_1().px_2().size_full();
        for (i, line) in diff_lines.iter().enumerate() {
            let (text, color) = match line {
                DiffLine::Unchanged(t) => (t.as_str(), fg),
                DiffLine::Added(t) => (t.as_str(), colors.editor_foreground),
                DiffLine::Removed(t) => (t.as_str(), colors.editor_foreground),
                DiffLine::Modified { new, .. } => (new.as_str(), colors.editor_foreground),
            };
            let bg_color = match line {
                DiffLine::Added(_) => gpui::rgb(0x2d5a1e),
                DiffLine::Removed(_) => gpui::rgb(0x5a1e1e),
                DiffLine::Modified { .. } => gpui::rgb(0x5a4a1e),
                DiffLine::Unchanged(_) => gpui::rgb(0x000000),
            };
            let prefix = match line {
                DiffLine::Unchanged(_) => "  ",
                DiffLine::Added(_) => "+ ",
                DiffLine::Removed(_) => "- ",
                DiffLine::Modified { .. } => "~ ",
            };
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .bg(bg_color)
                    .child(
                        div()
                            .w(px(40.0))
                            .text_color(gpui::rgb(0x888888))
                            .child(SharedString::from(format!("{}", i + 1))),
                    )
                    .child(
                        div()
                            .text_color(color)
                            .child(SharedString::from(format!("{}{}", prefix, text))),
                    ),
            );
        }

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(bg)
            .text_color(fg)
            .font_family("monospace")
            .text_size(px(13.0))
            .child(header)
            .child(body)
    }
}

impl Item for DiffReview {
    type Event = ItemEvent;

    fn tab_content(
        &self,
        params: TabContentParams,
        _window: &Window,
        _cx: &App,
    ) -> gpui::AnyElement {
        gpui::div()
            .child(gpui::SharedString::from(self.title.as_ref()))
            .into_any()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn is_dirty(&self, _: &App) -> bool {
        false
    }

    fn capability(&self, _: &App) -> language::Capability {
        language::Capability::ReadOnly
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if TypeId::of::<Self>() == type_id {
            Some(self_handle.clone().into())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(
        &self,
        _cx: &App,
    ) -> Option<(Vec<workspace::item::HighlightedText>, Option<gpui::Font>)> {
        use workspace::item::HighlightedText;
        Some((
            vec![HighlightedText {
                text: self.title.clone(),
                highlights: Vec::new(),
            }],
            None,
        ))
    }

    fn to_item_events(_event: &ItemEvent, f: &mut dyn std::ops::FnMut(ItemEvent)) {
        f(*_event)
    }

    fn can_split(&self) -> bool {
        false
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.title.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn review_without_snapshot_cannot_decline(cx: &mut gpui::TestAppContext) {
        let review = cx.new(|cx| {
            DiffReview::new(
                PathBuf::from("file.txt"),
                "same".to_string(),
                "same".to_string(),
                None,
                cx,
            )
        });

        assert!(!cx.read(|cx| review.read(cx).can_decline()));
        review.update(cx, |review, cx| review.decline(cx));
        assert!(!cx.read(|cx| review.read(cx).decline_pending));
        assert!(!cx.read(|cx| review.read(cx).resolved));
    }
}
