//! §16.6 OpenDiff command palette entry for reviewing a file against its
//! previous shadow-snapshot version.
//!
//! The action lists the files that accumulated shadow versions in the attached
//! session and lets the user pick one. Asking the user to browse for a file
//! instead would put the burden the wrong way round: the point of §4 is to
//! report what changed, not to make the user already know it.

use crate::diff_review::{DiffReview, ReviewQueue};
use anyhow::Context as _;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Task, WeakEntity, Window, prelude::*,
};
use picker::{Picker, PickerDelegate};
use std::path::PathBuf;
use std::sync::Arc;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

pub fn init(cx: &mut App) {
    cx.on_action(|_: &workspace::OpenDiff, cx: &mut App| {
        open_changed_file_selector(cx);
    });
    cx.on_action(
        |_: &crate::diff_review::OpenChangedFilesReview, cx: &mut App| {
            cx.spawn(async move |cx| {
                let (domain, session_id) = match mux_session(cx) {
                    Ok(session) => session,
                    Err(error) => {
                        report_to_workspace(
                            cx,
                            format!("Changed Files review needs an attached session: {error:#}"),
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
                    report_to_workspace(cx, "No changed files".to_string());
                    return;
                }
                let first_path = PathBuf::from(
                    changed
                        .first()
                        .map(|file| file.path.as_str())
                        .unwrap_or_default(),
                );
                cx.update(|cx| {
                    in_active_workspace(cx, move |workspace, window, cx| {
                        let handle = workspace.downgrade();
                        let domain = domain.clone();
                        let session_id = session_id.clone();
                        let changed = changed.clone();
                        workspace.update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, move |window, cx| {
                                OpenChangedFilesReviewModal::new(
                                    first_path, changed, domain, session_id, handle, window, cx,
                                )
                            });
                        });
                    });
                });
            })
            .detach();
        },
    );
    cx.on_action(
        |_: &crate::diff_review::OpenChangedFileReview, cx: &mut App| {
            let (path, workspace) = match active_workspace_file(cx) {
                Ok(active) => active,
                Err(error) => {
                    report_to_app(cx, format!("Could not review active file: {error:#}"));
                    return;
                }
            };
            cx.spawn(async move |cx| {
                let (domain, session_id) = match mux_session(cx) {
                    Ok(session) => session,
                    Err(error) => {
                        report_to_workspace(cx, format!("Could not review active file: {error:#}"));
                        return;
                    }
                };
                let response = match domain
                    .get_file_review_state(&session_id, path.to_string_lossy().as_ref())
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        report_to_workspace(cx, format!("Could not review active file: {error:#}"));
                        return;
                    }
                };
                let task =
                    cx.update(|cx| DiffReview::load_mux(path, response, domain, session_id, cx));
                let entity = match task.await {
                    Ok(entity) => entity,
                    Err(error) => {
                        report_to_workspace(cx, format!("Could not create review: {error:#}"));
                        return;
                    }
                };
                let shown = cx.update(|cx| {
                    let mut added = false;
                    for window_handle in cx.windows() {
                        let entity = entity.clone();
                        let workspace = workspace.clone();
                        let result = window_handle.update(cx, |_root, window, cx| {
                            let Some(workspace) = workspace.upgrade() else {
                                return false;
                            };
                            workspace.update(cx, |workspace, cx| {
                                let pane = workspace.active_pane().clone();
                                workspace.add_item(
                                    pane,
                                    Box::new(entity),
                                    None,
                                    true,
                                    true,
                                    window,
                                    cx,
                                );
                            });
                            true
                        });
                        if matches!(result, Ok(true)) {
                            added = true;
                            break;
                        }
                    }
                    added
                });
                if !shown {
                    tracing::warn!("OpenDiff: workspace closed before review opened");
                }
            })
            .detach();
        },
    );
}

fn open_changed_file_selector(cx: &mut App) {
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
}

/// Modal entry point for the continuous Changed Files review workflow.
pub struct OpenChangedFilesReviewModal {
    queue: ReviewQueue,
    selected_index: usize,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
}

impl ModalView for OpenChangedFilesReviewModal {}
impl EventEmitter<DismissEvent> for OpenChangedFilesReviewModal {}

impl Focusable for OpenChangedFilesReviewModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl OpenChangedFilesReviewModal {
    fn new(
        first_path: std::path::PathBuf,
        files: Vec<mux_protocol::ChangedFile>,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected_index = files
            .iter()
            .position(|file| PathBuf::from(&file.path) == first_path)
            .unwrap_or(0);
        Self {
            queue: ReviewQueue::from_changed_files(files),
            selected_index,
            domain,
            session_id,
            workspace,
            focus_handle: cx.focus_handle(),
        }
    }

    fn select_previous(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.queue.previous(self.selected_index) {
            self.selected_index = index;
            cx.notify();
        }
    }

    fn select_next(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.queue.next(self.selected_index) {
            self.selected_index = index;
            cx.notify();
        }
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.queue.entry(self.selected_index) else {
            return;
        };
        let path = entry.path.clone();
        let queue = self.queue.clone();
        let index = self.selected_index;
        let domain = self.domain.clone();
        let session_id = self.session_id.clone();
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        cx.spawn_in(window, async move |_this, cx| {
            open_diff_review_with_queue(path, domain, session_id, workspace, queue, index, cx)
                .await;
        })
        .detach();
    }
}

impl Render for OpenChangedFilesReviewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let progress = self.queue.progress_label();
        let mut rows = v_flex().gap_1();
        for (index, entry) in self.queue.entries().iter().enumerate() {
            let selected = index == self.selected_index;
            let classification = entry.classification.label();
            let status = match entry.status {
                crate::diff_review::ReviewQueueStatus::Pending => "Pending",
                crate::diff_review::ReviewQueueStatus::Reviewed => "Reviewed",
                crate::diff_review::ReviewQueueStatus::NeedsRefresh => "Needs refresh",
                crate::diff_review::ReviewQueueStatus::Loading => "Loading",
                crate::diff_review::ReviewQueueStatus::Unavailable => "Unavailable",
            };
            let path = entry.path.display().to_string();
            rows = rows.child(
                div()
                    .id(("changed-file", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .when(selected, |this| this.bg(colors.element_selected))
                    .child(format!("{classification} · {status}: {path}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected_index = index;
                        this.open_selected(window, cx);
                    })),
            );
        }

        v_flex()
            .key_context("ChangedFilesReview")
            .track_focus(&self.focus_handle)
            .w(rems(48.))
            .p_3()
            .gap_2()
            .child(Label::new("Changed Files Review"))
            .child(Label::new(progress))
            .child(rows)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("previous-file")
                            .px_2()
                            .py_1()
                            .bg(colors.border)
                            .child("Previous")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_previous(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("next-file")
                            .px_2()
                            .py_1()
                            .bg(colors.border)
                            .child("Next")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_next(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("review-file")
                            .px_2()
                            .py_1()
                            .bg(colors.border)
                            .child("Review")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_selected(window, cx);
                            })),
                    ),
            )
            .on_action(
                cx.listener(|this, _: &crate::diff_review::PreviousFile, window, cx| {
                    this.select_previous(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::diff_review::NextFile, window, cx| {
                    this.select_next(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::diff_review::CloseReview, _window, cx| {
                    cx.emit(DismissEvent);
                }),
            )
    }
}

async fn open_diff_review_with_queue(
    path: std::path::PathBuf,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    workspace: WeakEntity<Workspace>,
    queue: ReviewQueue,
    queue_index: usize,
    cx: &mut AsyncWindowContext,
) {
    let path_string = path.to_string_lossy().into_owned();
    let review_state = match domain
        .get_file_review_state(&session_id, &path_string)
        .await
    {
        Ok(review_state) => review_state,
        Err(error) => {
            report_error_in(
                &workspace,
                cx,
                format!("Could not load {}: {error:#}", path.display()),
            );
            return;
        }
    };

    let task = workspace.update(cx, |_workspace, cx| {
        DiffReview::load_mux_with_queue(
            path.clone(),
            review_state,
            domain,
            session_id,
            queue,
            queue_index,
            cx,
        )
    });
    let task = match task {
        Ok(task) => task,
        Err(error) => {
            report_error_in(
                &workspace,
                cx,
                format!("Could not open {}: {error:#}", path.display()),
            );
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

/// Fetch an atomic server review state, then open a DiffReview tab for `path`.
pub(crate) async fn open_diff_review(
    path: std::path::PathBuf,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    let path_string = path.to_string_lossy().into_owned();
    let review_state = match domain
        .get_file_review_state(&session_id, &path_string)
        .await
    {
        Ok(review_state) => review_state,
        Err(error) => {
            report_error_in(
                &workspace,
                cx,
                format!("Could not load {}: {error:#}", path.display()),
            );
            return;
        }
    };

    let task = workspace.update(cx, |_workspace, cx| {
        DiffReview::load_mux(path.clone(), review_state, domain, session_id, cx)
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

/// The mux domain and session currently attached to the application.
pub(crate) fn mux_session(
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<(Arc<mux::MuxDomain>, String)> {
    let domain = cx
        .update(|cx| workspace::AppState::try_global(cx).and_then(|state| state.mux_domain.clone()))
        .context("mux domain not available")?;
    let session_id = domain
        .last_attached_session_id()
        .context("no session attached")?;
    Ok((domain, session_id))
}

pub(crate) fn active_workspace_file(
    cx: &mut App,
) -> anyhow::Result<(PathBuf, WeakEntity<Workspace>)> {
    let mut found = None;
    in_active_workspace(cx, |workspace, _window, cx| {
        let path = workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.project_path(cx))
            .and_then(|path| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .absolute_path(&path, cx)
            });
        if let Some(path) = path {
            found = Some((path, workspace.downgrade()));
        }
    });
    found.context("the active workspace item is not a file")
}

/// OpenDiff is registered as a global action, so no workspace is in scope when
/// it fires and the target has to be found by inspecting each window's root.

pub(crate) fn in_active_workspace(
    cx: &mut App,
    body: impl FnOnce(&Entity<Workspace>, &mut Window, &mut App),
) {
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
fn report_to_app(cx: &mut App, message: String) {
    let logged = message.clone();
    let mut shown = false;
    in_active_workspace(cx, |workspace, _window, cx| {
        workspace.update(cx, |workspace, cx| workspace.show_error(message, cx));
        shown = true;
    });
    if !shown {
        tracing::warn!("OpenDiff: {logged}");
    }
}

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
