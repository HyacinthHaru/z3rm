//! §16.6 OpenDiff command palette entry for reviewing a file against its
//! previous shadow-snapshot version.
//!
//! The action lists the files that accumulated shadow versions in the attached
//! session and lets the user pick one. Asking the user to browse for a file
//! instead would put the burden the wrong way round: the point of §4 is to
//! report what changed, not to make the user already know it.

use crate::diff_review::{DiffReview, RestoreTarget};
use anyhow::Context as _;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity,
    prelude::*,
};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ItemHandle, ModalView, Workspace};

/// Register the OpenDiff action: list changed files, pick one, open DiffReview.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &workspace::OpenDiff, cx: &mut App| {
        cx.spawn(async move |cx| {
            let (domain, session_id) = match mux_session(cx) {
                Ok(session) => session,
                Err(error) => {
                    report_to_workspace(
                        cx,
                        format!("Diff review needs an attached session: {error:#}"),
                    );
                    return;
                }
            };

            let changed = match domain.list_changed_files(&session_id).await {
                Ok(changed) => changed.files,
                Err(error) => {
                    report_to_workspace(cx, format!("Could not list changed files: {error:#}"));
                    return;
                }
            };
            if changed.is_empty() {
                report_to_workspace(
                    cx,
                    "No shadow versions recorded in this session yet.".to_string(),
                );
                return;
            }

            let files: Vec<ChangedFile> = changed
                .into_iter()
                .map(|file| ChangedFile {
                    path: file.path.into(),
                    version_count: file.version_count,
                })
                .collect();

            cx.update(|cx| {
                in_active_workspace(cx, move |workspace, window, cx| {
                    let handle = workspace.downgrade();
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, move |window, cx| {
                            ChangedFileSelector::new(files, domain, session_id, handle, window, cx)
                        });
                    });
                });
            });
        })
        .detach();
    });
}

/// One entry in the changed-file list.
struct ChangedFile {
    path: std::path::PathBuf,
    version_count: u64,
}

pub struct ChangedFileSelector {
    picker: Entity<Picker<ChangedFileSelectorDelegate>>,
}

impl ModalView for ChangedFileSelector {}

impl EventEmitter<DismissEvent> for ChangedFileSelector {}

impl Focusable for ChangedFileSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ChangedFileSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl ChangedFileSelector {
    fn new(
        files: Vec<ChangedFile>,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = ChangedFileSelectorDelegate::new(
            files,
            domain,
            session_id,
            workspace,
            cx.entity().downgrade(),
        );
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

pub struct ChangedFileSelectorDelegate {
    selector: WeakEntity<ChangedFileSelector>,
    workspace: WeakEntity<Workspace>,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    files: Vec<ChangedFile>,
    selected_index: usize,
    matches: Vec<StringMatch>,
}

impl ChangedFileSelectorDelegate {
    fn new(
        files: Vec<ChangedFile>,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        workspace: WeakEntity<Workspace>,
        selector: WeakEntity<ChangedFileSelector>,
    ) -> Self {
        // The server already sorted by newest SeqNo, so the initial (unfiltered)
        // match list has to preserve that order rather than re-score it.
        let matches = files
            .iter()
            .enumerate()
            .map(|(index, file)| StringMatch {
                candidate_id: index,
                score: 0.0,
                positions: Vec::new(),
                string: file.path.to_string_lossy().into_owned(),
            })
            .collect();

        Self {
            selector,
            workspace,
            domain,
            session_id,
            files,
            selected_index: 0,
            matches,
        }
    }
}

fn unfiltered_matches(candidates: Vec<StringMatchCandidate>) -> Vec<StringMatch> {
    candidates
        .into_iter()
        .map(|candidate| StringMatch {
            candidate_id: candidate.id,
            string: candidate.string,
            positions: Vec::new(),
            score: 0.0,
        })
        .collect()
}

fn version_count_label(version_count: u64) -> String {
    let noun = if version_count == 1 {
        "version"
    } else {
        "versions"
    };
    format!("{version_count} {noun}")
}

impl PickerDelegate for ChangedFileSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "changed file selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a changed file to review...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background_executor = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| StringMatchCandidate::new(index, &file.path.to_string_lossy()))
            .collect();

        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                unfiltered_matches(candidates)
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background_executor,
                )
                .await
            };

            this.update(cx, |this, _cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.matches.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let selected = self
            .matches
            .get(self.selected_index)
            .and_then(|selected| self.files.get(selected.candidate_id));
        let Some(file) = selected else {
            self.dismissed(window, cx);
            return;
        };

        let path = file.path.clone();
        let domain = self.domain.clone();
        let session_id = self.session_id.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_this, cx| {
            open_diff_review(path, domain, session_id, workspace, cx).await;
        })
        .detach();

        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let path_match = self.matches.get(ix)?;
        let file = self.files.get(path_match.candidate_id)?;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    path_match.string.clone(),
                    path_match.positions.clone(),
                ))
                .end_slot(Label::new(version_count_label(file.version_count)).color(Color::Muted)),
        )
    }
}

/// Fetch the previous version, then open a DiffReview tab for `path`.
///
/// A file with a single recorded version has nothing to compare against, so it
/// opens as a read-only preview rather than failing outright.
async fn open_diff_review(
    path: std::path::PathBuf,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    let path_string = path.to_string_lossy().into_owned();
    let (previous, restore_target) =
        match fetch_previous_version(&domain, &session_id, &path_string).await {
            Ok((previous, version_id)) => (
                previous,
                Some(RestoreTarget::new(domain, session_id, version_id)),
            ),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "OpenDiff: shadow version unavailable; opening read-only preview"
                );
                let fallback = match smol::unblock({
                    let path = path.clone();
                    move || std::fs::read_to_string(&path)
                })
                .await
                {
                    Ok(text) => text,
                    Err(read_error) => {
                        report_error_in(
                            &workspace,
                            cx,
                            format!("Could not read {}: {read_error}", path.display()),
                        );
                        return;
                    }
                };
                (fallback, None)
            }
        };

    let task = workspace.update(cx, |_workspace, cx| {
        DiffReview::load(path.clone(), previous, restore_target, cx)
    });
    let task = match task {
        Ok(task) => task,
        Err(error) => {
            tracing::error!(error = %error, "OpenDiff: workspace went away");
            return;
        }
    };
    let entity = match task.await {
        Ok(entity) => entity,
        Err(error) => {
            report_error_in(
                &workspace,
                cx,
                format!("Could not open {}: {error:#}", path.display()),
            );
            return;
        }
    };

    workspace
        .update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            workspace.add_item(pane, Box::new(entity), None, true, true, window, cx);
        })
        .log_err();
}

/// Resolve the shadow version to diff against: the one before the newest.
async fn fetch_previous_version(
    domain: &Arc<mux::MuxDomain>,
    session_id: &str,
    path: &str,
) -> anyhow::Result<(String, u64)> {
    let versions_response = domain.list_file_versions(session_id, path).await?;
    let versions = versions_response.versions;
    let target = versions
        .len()
        .checked_sub(2)
        .and_then(|index| versions.get(index))
        .with_context(|| format!("need at least 2 versions to diff, found {}", versions.len()))?;
    let content_response = domain
        .get_file_version(session_id, path, target.version_id)
        .await?;
    let previous = String::from_utf8(content_response.content)
        .map_err(|error| anyhow::anyhow!("shadow version is not valid UTF-8: {error}"))?;
    Ok((previous, target.version_id))
}

/// The mux domain and the session the GUI is attached to.
fn mux_session(cx: &mut gpui::AsyncApp) -> anyhow::Result<(Arc<mux::MuxDomain>, String)> {
    let domain = cx
        .update(|cx| workspace::AppState::try_global(cx).and_then(|state| state.mux_domain.clone()))
        .context("mux domain not available")?;
    let session_id = domain
        .last_attached_session_id()
        .context("no session attached")?;
    Ok((domain, session_id))
}

/// Run `body` against the first window whose root hosts a workspace.
///
/// OpenDiff is registered as a global action, so no workspace is in scope when
/// it fires and the target has to be found by inspecting each window's root.
fn in_active_workspace(cx: &mut App, body: impl FnOnce(&Entity<Workspace>, &mut Window, &mut App)) {
    let mut body = Some(body);
    for window_handle in cx.windows() {
        let opened = window_handle.update(cx, |_root, window, cx| {
            let Some(Some(multi)) = window.root::<workspace::MultiWorkspace>() else {
                return false;
            };
            let Some(body) = body.take() else {
                return false;
            };
            let workspace = multi.read(cx).workspace().clone();
            body(&workspace, window, cx);
            true
        });
        if matches!(opened, Ok(true)) {
            return;
        }
    }
}

/// Surface a failure the user can act on. Falls back to the log when no
/// workspace is open — a silently dropped message is worse than a logged one.
fn report_to_workspace(cx: &mut gpui::AsyncApp, message: String) {
    let logged = message.clone();
    let shown = cx.update(|cx| {
        let mut shown = false;
        in_active_workspace(cx, |workspace, _window, cx| {
            workspace.update(cx, |workspace, cx| workspace.show_error(message, cx));
            shown = true;
        });
        shown
    });
    if !shown {
        tracing::warn!("OpenDiff: {logged}");
    }
}

fn report_error_in(
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
    message: String,
) {
    let logged = message.clone();
    if workspace
        .update(cx, |workspace, cx| workspace.show_error(message, cx))
        .is_err()
    {
        tracing::warn!("OpenDiff: {logged}");
    }
}

/// Compute a unified diff between the previous and current contents.
pub fn unified_diff(previous: &str, current: &str) -> String {
    language::unified_diff(previous, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfiltered_matches_preserve_candidate_ids() {
        let matches = unfiltered_matches(vec![
            StringMatchCandidate::new(7, "first"),
            StringMatchCandidate::new(42, "second"),
        ]);
        assert_eq!(
            matches
                .into_iter()
                .map(|candidate| candidate.candidate_id)
                .collect::<Vec<_>>(),
            vec![7, 42]
        );
    }

    #[test]
    fn version_count_uses_the_correct_noun() {
        assert_eq!(version_count_label(1), "1 version");
        assert_eq!(version_count_label(2), "2 versions");
    }

    #[test]
    fn unified_diff_marks_additions_and_removals() {
        let diff = unified_diff("a\nb\nc", "a\nx\nc");
        assert!(diff.lines().any(|line| line == "-b"), "{diff}");
        assert!(diff.lines().any(|line| line == "+x"), "{diff}");
    }

    #[test]
    fn unified_diff_does_not_treat_an_insertion_as_trailing_replacements() {
        let diff = unified_diff("a\nb\nc", "a\ninserted\nb\nc");
        assert!(diff.lines().any(|line| line == "+inserted"), "{diff}");
        assert!(
            !diff.lines().any(|line| line == "-b" || line == "-c"),
            "{diff}"
        );
    }
}
