//! # Diff Review — CLI agent file modification review (Plan 18, §16.6)
//!
//! Side-by-side diff view for reviewing changes CLI agents make to files.
//! Left pane: previous version (shadow snapshot). Right pane: current content.
//! Accept = keep current. Decline = revert via shadow_snapshot (§4.8).
//!
//! §16.6 Entry point: `workspace::OpenDiff`, from the command palette or its
//! keybinding. It lists the session's changed files and opens the picked one
//! here — see `open_diff`.

use gpui::{
    AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Task, Window, div, px,
};
use imara_diff::{Algorithm, diff, intern::InternedInput};
use std::any::TypeId;
use std::ops::Range;
use std::path::{Path, PathBuf};
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
    current_file_exists: bool,
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

fn read_current_content(path: &Path) -> std::io::Result<(String, bool)> {
    match std::fs::read_to_string(path) {
        Ok(current) => Ok((current, true)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((String::new(), false)),
        Err(error) => Err(error),
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
        Self::new_with_current_file_state(file_path, previous, current, true, restore_target, cx)
    }

    fn new_with_current_file_state(
        file_path: PathBuf,
        previous: String,
        current: String,
        current_file_exists: bool,
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
            current_file_exists,
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
                move || read_current_content(&path)
            })
            .await
            .map_err(|error| anyhow::anyhow!("failed to read {}: {}", path.display(), error))?;
            let (current, current_file_exists) = current;
            let entity = cx.new(|cx| {
                DiffReview::new_with_current_file_state(
                    path.clone(),
                    prev,
                    current,
                    current_file_exists,
                    restore_target,
                    cx,
                )
            });
            Ok(entity)
        })
    }

    pub fn is_deleted(&self) -> bool {
        !self.current_file_exists
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
                        this.current_content = this.previous_content.clone();
                        this.current_file_exists = true;
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
        let previous_content: &str = &self.previous_content;
        let current_content: &str = &self.current_content;
        let input = InternedInput::new(previous_content, current_content);
        let mut lines = Vec::new();
        let mut unchanged_start = 0usize;

        diff(
            Algorithm::Histogram,
            &input,
            |removed_range: Range<u32>, added_range: Range<u32>| {
                let removed_start = removed_range.start as usize;
                for token in input
                    .before
                    .get(unchanged_start..removed_start)
                    .unwrap_or_default()
                {
                    lines.push(DiffLine::Unchanged(input.interner[*token].to_string()));
                }

                let removed = input
                    .before
                    .get(removed_start..removed_range.end as usize)
                    .unwrap_or_default();
                let added = input
                    .after
                    .get(added_range.start as usize..added_range.end as usize)
                    .unwrap_or_default();

                // A real diff reports a hunk as "delete this run, insert that run". Pairing the
                // two runs positionally keeps a single-line edit rendered as one `Modified` row,
                // preserving the original side-by-side visual design.
                for (old_token, new_token) in removed.iter().zip(added.iter()) {
                    lines.push(DiffLine::Modified {
                        old: input.interner[*old_token].to_string(),
                        new: input.interner[*new_token].to_string(),
                    });
                }
                for token in removed.iter().skip(added.len()) {
                    lines.push(DiffLine::Removed(input.interner[*token].to_string()));
                }
                for token in added.iter().skip(removed.len()) {
                    lines.push(DiffLine::Added(input.interner[*token].to_string()));
                }

                unchanged_start = removed_range.end as usize;
            },
        );

        for token in input.before.get(unchanged_start..).unwrap_or_default() {
            lines.push(DiffLine::Unchanged(input.interner[*token].to_string()));
        }
        lines
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
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(SharedString::from(format!("Diff: {}", file_name)))
                    .when(self.is_deleted(), |this| {
                        this.child(Label::new("Deleted").color(Color::Error))
                    }),
            )
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
            // Same shape as the log viewer: without a role this focused root
            // yields no accessibility node at all. Named by the file so two
            // open reviews are told apart.
            .id("diff-review")
            .role(gpui::Role::Group)
            .aria_label(SharedString::from(format!(
                "Diff review: {}",
                self.file_path.display()
            )))
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

    /// A focused element with no role produces no accessibility node, so
    /// opening this view discarded focus and screen readers announced the whole
    /// window instead of the review.
    #[gpui::test]
    async fn the_review_is_announced_by_the_file_it_reviews(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, cx| {
            DiffReview::new(
                PathBuf::from("src/main.rs"),
                "one\n".to_string(),
                "two\n".to_string(),
                None,
                cx,
            )
        });
        cx.activate_a11y(window.into());

        let focus_handle = window
            .update(cx, |review, _, _| review.focus_handle.clone())
            .expect("the review window is still open");
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.focus(&focus_handle, cx);
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the review window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value =
            serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "diff review");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "diff review");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "diff review");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "diff review");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "diff review");

        assert_eq!(
            tree["frame"]["focus_without_node"].as_str(),
            None,
            "the review carries a role now, so its focus must reach the tree"
        );
        let focused = tree["gpui_focus"].as_str().expect("the review holds focus");
        assert_eq!(
            tree["nodes"][focused]["aria"]["label"].as_str(),
            Some("Diff review: src/main.rs"),
            "two open reviews are only told apart by the file they review"
        );
    }

    /// Renders a diff as compact tags so assertions read like a unified diff:
    /// `" x"` unchanged, `"+x"` added, `"-x"` removed, `"~old>new"` modified.
    fn summarize(cx: &mut gpui::TestAppContext, previous: &str, current: &str) -> Vec<String> {
        let review = cx.new(|cx| {
            DiffReview::new(
                PathBuf::from("file.txt"),
                previous.to_string(),
                current.to_string(),
                None,
                cx,
            )
        });
        cx.read(|cx| {
            review
                .read(cx)
                .line_diff()
                .iter()
                .map(|line| match line {
                    DiffLine::Unchanged(text) => format!(" {text}"),
                    DiffLine::Added(text) => format!("+{text}"),
                    DiffLine::Removed(text) => format!("-{text}"),
                    DiffLine::Modified { old, new } => format!("~{old}>{new}"),
                })
                .collect()
        })
    }

    #[gpui::test]
    fn insertion_in_the_middle_keeps_surrounding_lines_unchanged(cx: &mut gpui::TestAppContext) {
        let lines = summarize(
            cx,
            "alpha\nbeta\ngamma\ndelta\n",
            "alpha\nbeta\ninserted\ngamma\ndelta\n",
        );
        assert_eq!(
            lines,
            vec![" alpha", " beta", "+inserted", " gamma", " delta"]
        );
    }

    #[gpui::test]
    fn deletion_in_the_middle_keeps_surrounding_lines_unchanged(cx: &mut gpui::TestAppContext) {
        let lines = summarize(cx, "alpha\nbeta\ngamma\ndelta\n", "alpha\nbeta\ndelta\n");
        assert_eq!(lines, vec![" alpha", " beta", "-gamma", " delta"]);
    }

    #[gpui::test]
    fn single_line_edit_becomes_one_modified_line(cx: &mut gpui::TestAppContext) {
        let lines = summarize(cx, "alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(lines, vec![" alpha", "~beta>BETA", " gamma"]);
    }

    #[gpui::test]
    fn uneven_replacement_pairs_lines_then_reports_the_remainder(cx: &mut gpui::TestAppContext) {
        let lines = summarize(cx, "alpha\nbeta\ngamma\n", "alpha\nBETA\nextra\ngamma\n");
        assert_eq!(lines, vec![" alpha", "~beta>BETA", "+extra", " gamma"]);

        let lines = summarize(cx, "alpha\nbeta\nextra\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(lines, vec![" alpha", "~beta>BETA", "-extra", " gamma"]);
    }

    #[gpui::test]
    fn identical_content_is_entirely_unchanged(cx: &mut gpui::TestAppContext) {
        let content = "alpha\nbeta\ngamma\n";
        let lines = summarize(cx, content, content);
        assert_eq!(lines, vec![" alpha", " beta", " gamma"]);
    }

    #[gpui::test]
    fn empty_previous_content_is_all_additions(cx: &mut gpui::TestAppContext) {
        let lines = summarize(cx, "", "alpha\nbeta\n");
        assert_eq!(lines, vec!["+alpha", "+beta"]);
    }

    #[gpui::test]
    fn empty_current_content_is_all_removals(cx: &mut gpui::TestAppContext) {
        let lines = summarize(cx, "alpha\nbeta\n", "");
        assert_eq!(lines, vec!["-alpha", "-beta"]);
    }

    #[test]
    fn current_content_reader_distinguishes_deleted_and_empty_files() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let missing_path = directory.path().join("deleted.txt");
        assert_eq!(
            read_current_content(&missing_path).expect("read missing file state"),
            (String::new(), false)
        );

        let empty_path = directory.path().join("empty.txt");
        std::fs::write(&empty_path, "").expect("write empty file");
        assert_eq!(
            read_current_content(&empty_path).expect("read empty file state"),
            (String::new(), true)
        );
    }

    #[gpui::test]
    fn deleted_file_review_has_empty_current_side(cx: &mut gpui::TestAppContext) {
        let review = cx.new(|cx| {
            DiffReview::new_with_current_file_state(
                PathBuf::from("deleted.txt"),
                "alpha\nbeta\n".to_string(),
                String::new(),
                false,
                None,
                cx,
            )
        });
        cx.read(|cx| {
            let review = review.read(cx);
            assert!(review.is_deleted());
            assert_eq!(review.current_content.as_ref(), "");
            assert_eq!(
                review
                    .line_diff()
                    .iter()
                    .map(|line| match line {
                        DiffLine::Removed(text) => format!("-{text}"),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>(),
                vec!["-alpha", "-beta"]
            );
        });
    }

    #[gpui::test]
    fn both_contents_empty_produces_no_lines(cx: &mut gpui::TestAppContext) {
        let lines = summarize(cx, "", "");
        assert!(lines.is_empty(), "{lines:?}");
    }

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
