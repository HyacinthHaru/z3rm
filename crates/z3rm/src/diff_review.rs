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
    Action, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Task, WeakEntity, Window, div, px,
};
use imara_diff::{Algorithm, diff, intern::InternedInput};
use std::any::TypeId;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use ui::prelude::*;
use workspace::{
    ToolbarItemLocation, Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

gpui::actions!(
    change_review,
    [
        OpenChangedFilesReview,
        OpenChangedFileReview,
        PreviousFile,
        NextFile,
        PreviousVersion,
        NextVersion,
        CompareCurrent,
        AcceptCurrent,
        RestoreSelectedVersion,
        RefreshCurrent,
        CloseReview,
    ]
);

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
    /// Queue state is present only for the review-all workflow.
    queue: Option<ReviewQueue>,
    queue_index: usize,
    queue_error: Option<SharedString>,
    queue_sync_error: Option<SharedString>,
    review_model: Option<ReviewStateModel>,
    review_source: Option<MuxReviewSource>,
    from_state: ReviewContentState,
    to_state: ReviewContentState,
    restore_confirmation: Option<RestoreConfirmation>,
    review_loading: bool,
    _queue_refresh_task: Option<Task<()>>,
}

#[derive(Clone)]
struct MuxReviewSource {
    domain: Arc<mux::MuxDomain>,
    session_id: String,
}

#[derive(Clone)]
struct LoadedReviewContent {
    state: ReviewContentState,
    content: Option<SharedString>,
}

pub struct RestoreTarget {
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    version_id: u64,
    expected_latest_seq_no: u64,
    expected_current_exists: bool,
    expected_current_sha256: Vec<u8>,
}

impl RestoreTarget {
    pub fn new(
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        version_id: u64,
        expected_latest_seq_no: u64,
        expected_current_exists: bool,
        expected_current_sha256: Vec<u8>,
    ) -> Self {
        Self {
            domain,
            session_id,
            version_id,
            expected_latest_seq_no,
            expected_current_exists,
            expected_current_sha256,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewContentState {
    Text,
    Empty,
    Binary,
    TooLarge,
    Deleted,
    Unavailable(String),
}

impl ReviewContentState {
    pub fn from_proto(value: i32) -> Self {
        match value {
            value if value == mux_protocol::FileContentState::Text as i32 => Self::Text,
            value if value == mux_protocol::FileContentState::Empty as i32 => Self::Empty,
            value if value == mux_protocol::FileContentState::Binary as i32 => Self::Binary,
            value if value == mux_protocol::FileContentState::TooLarge as i32 => Self::TooLarge,
            value if value == mux_protocol::FileContentState::Deleted as i32 => Self::Deleted,
            _ => Self::Unavailable("server returned an unknown content state".to_string()),
        }
    }

    pub fn is_text_comparable(&self) -> bool {
        matches!(self, Self::Text | Self::Empty)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Empty => "Empty",
            Self::Binary => "Binary",
            Self::TooLarge => "Too large",
            Self::Deleted => "Deleted",
            Self::Unavailable(_) => "Unavailable",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewClassification {
    Added,
    Modified,
    Deleted,
    Binary,
    TooLarge,
    Unavailable,
}

/// Classify a queue row from the listing alone.
///
/// The listing carries existence and the oldest trigger, which is enough to
/// separate added / modified / deleted without one review-state RPC per file.
/// Binary and oversized content still needs the file itself, so those rows
/// read as `Modified` until they are opened and `mark_refreshed` corrects them.
fn queue_row_classification(file: &mux_protocol::ChangedFile) -> ReviewClassification {
    if !file.current_exists {
        ReviewClassification::Deleted
    } else if file.first_trigger.eq_ignore_ascii_case("create") {
        ReviewClassification::Added
    } else {
        ReviewClassification::Modified
    }
}

impl ReviewClassification {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Modified => "Modified",
            Self::Deleted => "Deleted",
            Self::Binary => "Binary",
            Self::TooLarge => "Too large",
            Self::Unavailable => "Unavailable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewUnavailableState {
    NoHistory,
    Binary,
    TooLarge,
    Deleted,
    Evicted,
    Error(String),
}

impl ReviewUnavailableState {
    fn label(&self) -> String {
        match self {
            Self::NoHistory => "no historical version is available".to_string(),
            Self::Binary => "the selected version is binary".to_string(),
            Self::TooLarge => "the selected version is too large to compare".to_string(),
            Self::Deleted => "the selected version is a deletion".to_string(),
            Self::Evicted => "the selected version is no longer retained".to_string(),
            Self::Error(error) => error.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewEndpoint {
    Current,
    Historical(u64),
    Unavailable {
        version_id: Option<u64>,
        state: ReviewUnavailableState,
    },
}

impl ReviewEndpoint {
    pub fn label(&self) -> String {
        match self {
            Self::Current => "Current".to_string(),
            Self::Historical(version_id) => format!("Version {version_id}"),
            Self::Unavailable { version_id, state } => match version_id {
                Some(version_id) => {
                    format!("Version {version_id} unavailable ({})", state.label())
                }
                None => format!("Unavailable ({})", state.label()),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewEndpointRole {
    From,
    To,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewTimelineVersion {
    pub version_id: u64,
    pub seq_no: u64,
    pub trigger: String,
}

impl ReviewTimelineVersion {
    pub fn trigger_label(&self) -> String {
        match self.trigger.to_ascii_lowercase().as_str() {
            "create" => "Created".to_string(),
            "write" => "Written".to_string(),
            "close" => "Closed".to_string(),
            "debounce" => "Debounced".to_string(),
            "decline" | "restore" => "Restored".to_string(),
            "delete" => "Deleted".to_string(),
            trigger if trigger.is_empty() => "Unknown".to_string(),
            trigger => {
                let mut chars = trigger.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => "Unknown".to_string(),
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReviewTimeline {
    versions: Vec<ReviewTimelineVersion>,
    from: ReviewEndpoint,
    to: ReviewEndpoint,
    active_endpoint: ReviewEndpointRole,
}

impl ReviewTimeline {
    pub fn from_response(response: &mux_protocol::GetFileReviewStateResponse) -> Self {
        let mut versions = response
            .versions
            .iter()
            .map(|version| ReviewTimelineVersion {
                version_id: version.version_id,
                seq_no: version.seq_no,
                trigger: version.trigger.clone(),
            })
            .collect::<Vec<_>>();
        versions.sort_by_key(|version| (version.seq_no, version.version_id));
        let from = versions
            .iter()
            .rev()
            .nth(1)
            .or_else(|| versions.last())
            .map(|version| ReviewEndpoint::Historical(version.version_id))
            .unwrap_or(ReviewEndpoint::Unavailable {
                version_id: None,
                state: ReviewUnavailableState::NoHistory,
            });
        Self {
            versions,
            from,
            to: ReviewEndpoint::Current,
            active_endpoint: ReviewEndpointRole::From,
        }
    }

    pub fn versions(&self) -> &[ReviewTimelineVersion] {
        &self.versions
    }

    pub fn from(&self) -> &ReviewEndpoint {
        &self.from
    }

    pub fn to(&self) -> &ReviewEndpoint {
        &self.to
    }

    pub fn active_endpoint(&self) -> ReviewEndpointRole {
        self.active_endpoint
    }

    pub fn set_active_endpoint(&mut self, role: ReviewEndpointRole) {
        self.active_endpoint = role;
    }

    pub fn select_from(&mut self, version_id: u64) {
        self.from = self.endpoint_for_version(version_id);
        self.active_endpoint = ReviewEndpointRole::From;
    }

    pub fn select_to(&mut self, version_id: u64) {
        self.to = self.endpoint_for_version(version_id);
        self.active_endpoint = ReviewEndpointRole::To;
    }

    pub fn compare_to_current(&mut self) {
        self.to = ReviewEndpoint::Current;
        self.active_endpoint = ReviewEndpointRole::To;
    }

    pub fn mark_unavailable(
        &mut self,
        role: ReviewEndpointRole,
        version_id: Option<u64>,
        state: ReviewUnavailableState,
    ) {
        let endpoint = ReviewEndpoint::Unavailable { version_id, state };
        match role {
            ReviewEndpointRole::From => self.from = endpoint,
            ReviewEndpointRole::To => self.to = endpoint,
        }
    }

    pub fn previous(&mut self) -> ReviewEndpoint {
        self.move_active(-1);
        self.active_endpoint_value().clone()
    }

    pub fn next(&mut self) -> ReviewEndpoint {
        self.move_active(1);
        self.active_endpoint_value().clone()
    }

    fn active_endpoint_value(&self) -> &ReviewEndpoint {
        match self.active_endpoint {
            ReviewEndpointRole::From => &self.from,
            ReviewEndpointRole::To => &self.to,
        }
    }

    fn endpoint_for_version(&self, version_id: u64) -> ReviewEndpoint {
        if self
            .versions
            .iter()
            .any(|version| version.version_id == version_id)
        {
            ReviewEndpoint::Historical(version_id)
        } else {
            ReviewEndpoint::Unavailable {
                version_id: Some(version_id),
                state: ReviewUnavailableState::Evicted,
            }
        }
    }

    fn move_active(&mut self, delta: isize) {
        if self.versions.is_empty() {
            return;
        }
        let role = self.active_endpoint;
        let current_index = match self.active_endpoint_value() {
            ReviewEndpoint::Current => self.versions.len(),
            ReviewEndpoint::Historical(version_id) => self
                .versions
                .iter()
                .position(|version| version.version_id == *version_id)
                .unwrap_or(0),
            ReviewEndpoint::Unavailable { version_id, .. } => version_id
                .and_then(|version_id| {
                    self.versions
                        .iter()
                        .position(|version| version.version_id == version_id)
                })
                .unwrap_or(0),
        };
        let max_index = match role {
            ReviewEndpointRole::From => self.versions.len().saturating_sub(1),
            ReviewEndpointRole::To => self.versions.len(),
        };
        let next_index = if delta < 0 {
            current_index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current_index.saturating_add(delta as usize).min(max_index)
        };
        let endpoint = if role == ReviewEndpointRole::To && next_index == self.versions.len() {
            ReviewEndpoint::Current
        } else {
            ReviewEndpoint::Historical(self.versions[next_index].version_id)
        };
        match role {
            ReviewEndpointRole::From => self.from = endpoint,
            ReviewEndpointRole::To => self.to = endpoint,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReviewStateModel {
    timeline: ReviewTimeline,
    current_content: Option<String>,
    current_state: ReviewContentState,
    latest_seq_no: u64,
    current_exists: bool,
    current_sha256: Vec<u8>,
    error: Option<String>,
}

impl ReviewStateModel {
    pub fn from_response(response: &mux_protocol::GetFileReviewStateResponse) -> Self {
        let current_state = ReviewContentState::from_proto(response.current_state);
        let current_content = if current_state.is_text_comparable() {
            String::from_utf8(response.current_content.clone()).ok()
        } else {
            None
        };
        Self {
            timeline: ReviewTimeline::from_response(response),
            current_content,
            current_state,
            latest_seq_no: response.latest_seq_no,
            current_exists: response.current_exists,
            current_sha256: response.current_sha256.clone(),
            error: None,
        }
    }

    pub fn timeline(&self) -> &ReviewTimeline {
        &self.timeline
    }

    pub fn timeline_mut(&mut self) -> &mut ReviewTimeline {
        &mut self.timeline
    }

    pub fn select_from(&mut self, version_id: u64) {
        self.timeline.select_from(version_id);
    }

    pub fn current_content(&self) -> Option<&str> {
        self.current_content.as_deref()
    }

    pub fn current_state(&self) -> &ReviewContentState {
        &self.current_state
    }

    pub fn latest_seq_no(&self) -> u64 {
        self.latest_seq_no
    }

    pub fn current_exists(&self) -> bool {
        self.current_exists
    }

    pub fn current_sha256(&self) -> &[u8] {
        &self.current_sha256
    }

    pub fn classification(&self) -> ReviewClassification {
        match self.current_state {
            ReviewContentState::Deleted => ReviewClassification::Deleted,
            ReviewContentState::Binary => ReviewClassification::Binary,
            ReviewContentState::TooLarge => ReviewClassification::TooLarge,
            ReviewContentState::Unavailable(_) => ReviewClassification::Unavailable,
            ReviewContentState::Text | ReviewContentState::Empty => {
                let first_is_create = self
                    .timeline
                    .versions()
                    .first()
                    .is_some_and(|version| version.trigger.eq_ignore_ascii_case("create"));
                if first_is_create && self.current_exists {
                    ReviewClassification::Added
                } else {
                    ReviewClassification::Modified
                }
            }
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn retain_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }
    pub fn refresh_from(&mut self, response: &mux_protocol::GetFileReviewStateResponse) -> bool {
        let changed = self.latest_seq_no != response.latest_seq_no
            || self.current_exists != response.current_exists
            || self.current_sha256 != response.current_sha256;
        let old_from = self.timeline.from().clone();
        let old_to = self.timeline.to().clone();
        let old_active = self.timeline.active_endpoint();
        *self = Self::from_response(response);
        if let ReviewEndpoint::Historical(version_id) = old_from {
            self.timeline.select_from(version_id);
        }
        match old_to {
            ReviewEndpoint::Current => self.timeline.compare_to_current(),
            ReviewEndpoint::Historical(version_id) => self.timeline.select_to(version_id),
            ReviewEndpoint::Unavailable { version_id, state } => {
                self.timeline
                    .mark_unavailable(ReviewEndpointRole::To, version_id, state)
            }
        }
        // Last: `select_from` / `select_to` / `compare_to_current` each move
        // the active endpoint as a side effect, so restoring it any earlier
        // leaves the user's browsing endpoint pointing at the wrong side and
        // the version arrows nudging the endpoint they were not using.
        self.timeline.set_active_endpoint(old_active);
        changed
    }

    pub fn replace_response(&mut self, response: &mux_protocol::GetFileReviewStateResponse) {
        *self = Self::from_response(response);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreConfirmation {
    path: PathBuf,
    version_id: u64,
    seq_no: u64,
    target_state: ReviewContentState,
}

impl RestoreConfirmation {
    pub fn new(
        path: PathBuf,
        version_id: u64,
        seq_no: u64,
        target_state: ReviewContentState,
    ) -> Self {
        Self {
            path,
            version_id,
            seq_no,
            target_state,
        }
    }

    pub fn text(&self) -> String {
        let operation = if self.target_state == ReviewContentState::Deleted {
            "the current file will be removed"
        } else {
            "Current text bytes will be replaced"
        };
        format!(
            "Restore {} to version {} (SeqNo {})? {}; all history remains.",
            self.path.display(),
            self.version_id,
            self.seq_no,
            operation
        )
    }

    pub fn version_id(&self) -> u64 {
        self.version_id
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
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        let title = SharedString::from(format!("Diff: {file_name}"));
        Self {
            file_path,
            previous_content: previous.into(),
            current_content: current.into(),
            current_file_exists,
            resolved: false,
            title,
            focus_handle: cx.focus_handle(),
            restore_target,
            decline_pending: false,
            decline_error: None,
            queue: None,
            queue_index: 0,
            queue_error: None,
            queue_sync_error: None,
            review_model: None,
            review_source: None,
            from_state: ReviewContentState::Text,
            to_state: ReviewContentState::Text,
            restore_confirmation: None,
            review_loading: false,
            _queue_refresh_task: None,
        }
    }

    pub fn new_mux(
        file_path: PathBuf,
        response: mux_protocol::GetFileReviewStateResponse,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = ReviewStateModel::from_response(&response);
        let current = model.current_content().unwrap_or_default().to_string();
        let current_state = model.current_state().clone();
        let from = model.timeline().from().clone();
        let from_state = match from {
            ReviewEndpoint::Unavailable { ref state, .. } => {
                ReviewContentState::Unavailable(state.label())
            }
            _ => ReviewContentState::Unavailable("historical content is loading".to_string()),
        };
        let version_id = match from {
            ReviewEndpoint::Historical(version_id) => Some(version_id),
            _ => None,
        };
        let restore_target = version_id.map(|version_id| {
            RestoreTarget::new(
                domain.clone(),
                session_id.clone(),
                version_id,
                model.latest_seq_no(),
                model.current_exists(),
                model.current_sha256().to_vec(),
            )
        });
        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        Self {
            file_path,
            previous_content: SharedString::default(),
            current_content: current.into(),
            current_file_exists: model.current_exists(),
            resolved: false,
            title: SharedString::from(format!("Diff: {file_name}")),
            focus_handle: cx.focus_handle(),
            restore_target,
            decline_pending: false,
            decline_error: None,
            queue: None,
            queue_index: 0,
            queue_error: None,
            queue_sync_error: None,
            review_model: Some(model),
            review_source: Some(MuxReviewSource { domain, session_id }),
            from_state,
            to_state: current_state,
            restore_confirmation: None,
            review_loading: false,
            _queue_refresh_task: None,
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
    pub fn load_with_queue(
        file_path: PathBuf,
        previous_content: String,
        restore_target: Option<RestoreTarget>,
        queue: ReviewQueue,
        queue_index: usize,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let task = Self::load(file_path, previous_content, restore_target, cx);
        cx.spawn(async move |cx| {
            let entity = task.await?;
            entity.update(cx, |review, cx| {
                review.queue = Some(queue);
                review.queue_index = queue_index;
                cx.notify();
            });
            Ok(entity)
        })
    }

    pub fn is_deleted(&self) -> bool {
        !self.current_file_exists
    }

    pub fn classify(
        &self,
        response: &mux_protocol::GetFileReviewStateResponse,
    ) -> ReviewClassification {
        ReviewStateModel::from_response(response).classification()
    }

    fn mark_current_queue_item_needs_refresh(&mut self) {
        if let Some(queue) = self.queue.as_mut()
            && let Some(entry) = queue.entries.get_mut(self.queue_index)
        {
            entry.status = ReviewQueueStatus::NeedsRefresh;
        }
    }

    /// §16.6 Accept the current version after an atomic freshness check.
    pub fn accept(&mut self, cx: &mut Context<Self>) {
        if self.decline_pending || self.review_loading || self.resolved {
            return;
        }
        let Some(source) = self.review_source.clone() else {
            self.resolved = true;
            cx.emit(DiffReviewEvent::Accepted);
            cx.notify();
            return;
        };
        let path = self.file_path.to_string_lossy().into_owned();
        let queue_index = self.queue_index;
        self.review_loading = true;
        cx.spawn(async move |this, cx| {
            let state = source
                .domain
                .get_file_review_state(&source.session_id, &path)
                .await;
            let changed = source.domain.list_changed_files(&source.session_id).await;
            this.update(cx, |this, cx| {
                this.review_loading = false;
                match (state, changed) {
                    (Ok(state), Ok(changed)) => {
                        let fresh = this
                            .review_model
                            .as_ref()
                            .map(|model| {
                                model.latest_seq_no() == state.latest_seq_no
                                    && model.current_exists() == state.current_exists
                                    && model.current_sha256() == state.current_sha256
                            })
                            .unwrap_or(false);
                        if fresh {
                            let classification = this.classify(&state);
                            if let Some(queue) = this.queue.as_mut() {
                                queue.refresh_changed_files(&changed.files);
                                queue.mark_reviewed_with_classification(
                                    queue_index,
                                    state.latest_seq_no,
                                    state.current_exists,
                                    state.current_sha256.clone(),
                                    classification,
                                );
                            }
                            this.resolved = true;
                            cx.emit(DiffReviewEvent::Accepted);
                            this.advance_after_success(cx);
                        } else {
                            if let Some(queue) = this.queue.as_mut() {
                                queue.refresh_changed_files(&changed.files);
                            }
                            this.mark_current_queue_item_needs_refresh();
                            this.queue_error = Some(
                                "File changed while it was being reviewed; refresh required."
                                    .into(),
                            );
                        }
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        this.mark_current_queue_item_needs_refresh();
                        this.queue_error = Some(format!("Accept failed: {error:#}").into());
                    }
                }
                cx.notify();
            })
        })
        .detach();
        cx.notify();
    }
    pub fn load_mux(
        file_path: PathBuf,
        response: mux_protocol::GetFileReviewStateResponse,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let path = file_path.clone();
        let path_string = path.to_string_lossy().into_owned();
        let initial_from = ReviewTimeline::from_response(&response).from().clone();
        cx.spawn(async move |cx| {
            let initial_content = match initial_from {
                ReviewEndpoint::Historical(version_id) => domain
                    .get_file_version(&session_id, &path_string, version_id)
                    .await
                    .map(Self::loaded_review_content)
                    .unwrap_or_else(|error| LoadedReviewContent {
                        state: ReviewContentState::Unavailable(format!(
                            "historical version {version_id} unavailable: {error:#}"
                        )),
                        content: None,
                    }),
                ReviewEndpoint::Unavailable { state, .. } => LoadedReviewContent {
                    state: ReviewContentState::Unavailable(state.label()),
                    content: None,
                },
                ReviewEndpoint::Current => LoadedReviewContent {
                    state: ReviewContentState::from_proto(response.current_state),
                    content: String::from_utf8(response.current_content.clone())
                        .ok()
                        .map(Into::into),
                },
            };
            let entity = cx.new(|cx| {
                let mut review = Self::new_mux(path.clone(), response, domain, session_id, cx);
                review.set_loaded_endpoint(ReviewEndpointRole::From, initial_content);
                review
            });
            Ok(entity)
        })
    }
    pub fn load_mux_with_queue(
        file_path: PathBuf,
        response: mux_protocol::GetFileReviewStateResponse,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        queue: ReviewQueue,
        queue_index: usize,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let task = Self::load_mux(file_path, response.clone(), domain, session_id, cx);
        cx.spawn(async move |cx| {
            let entity = task.await?;
            entity.update(cx, |review, cx| {
                let classification = review.classify(&response);
                review.queue = Some(queue);
                review.queue_index = queue_index;
                if let Some(queue) = review.queue.as_mut() {
                    if let Some(entry) = queue.entries.get_mut(queue_index) {
                        entry.classification = classification;
                        entry.current_exists = Some(response.current_exists);
                        entry.current_sha256 = Some(response.current_sha256.clone());
                        entry.latest_seq_no = response.latest_seq_no;
                    }
                }
                review.start_queue_refresh(cx);
                cx.notify();
            });
            Ok(entity)
        })
    }

    fn start_queue_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.review_source.clone() else {
            return;
        };
        self._queue_refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let result = source.domain.list_changed_files(&source.session_id).await;
                if this
                    .update(cx, |this, cx| {
                        match result {
                            Ok(changed_files) => {
                                if let Some(queue) = this.queue.as_mut() {
                                    queue.refresh_changed_files(&changed_files.files);
                                }
                                this.queue_sync_error = None;
                            }
                            Err(error) => {
                                this.queue_sync_error =
                                    Some(format!("Queue refresh failed: {error:#}").into());
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn loaded_review_content(
        response: mux_protocol::GetFileVersionResponse,
    ) -> LoadedReviewContent {
        let state = ReviewContentState::from_proto(response.state);
        let content = if response.content_available && state.is_text_comparable() {
            match String::from_utf8(response.content) {
                Ok(content) => Some(content.into()),
                Err(_) => {
                    return LoadedReviewContent {
                        state: ReviewContentState::Binary,
                        content: None,
                    };
                }
            }
        } else {
            None
        };
        LoadedReviewContent { state, content }
    }

    fn set_loaded_endpoint(&mut self, role: ReviewEndpointRole, loaded: LoadedReviewContent) {
        match role {
            ReviewEndpointRole::From => {
                self.from_state = loaded.state.clone();
                self.previous_content = loaded.content.unwrap_or_default();
            }
            ReviewEndpointRole::To => {
                self.to_state = loaded.state.clone();
                self.current_content = loaded.content.unwrap_or_default();
            }
        }
        if let ReviewContentState::Unavailable(error) = loaded.state {
            let version_id =
                match role {
                    ReviewEndpointRole::From => match self.review_model.as_ref().and_then(|model| {
                        match model.timeline().from() {
                            ReviewEndpoint::Historical(version_id) => Some(*version_id),
                            _ => None,
                        }
                    }) {
                        Some(version_id) => Some(version_id),
                        None => None,
                    },
                    ReviewEndpointRole::To => match self.review_model.as_ref().and_then(|model| {
                        match model.timeline().to() {
                            ReviewEndpoint::Historical(version_id) => Some(*version_id),
                            _ => None,
                        }
                    }) {
                        Some(version_id) => Some(version_id),
                        None => None,
                    },
                };
            if let Some(model) = self.review_model.as_mut() {
                model.timeline_mut().mark_unavailable(
                    role,
                    version_id,
                    ReviewUnavailableState::Error(error),
                );
            }
        }
    }

    pub fn timeline(&self) -> Option<&ReviewTimeline> {
        self.review_model.as_ref().map(ReviewStateModel::timeline)
    }

    pub fn comparison_header(&self) -> Option<String> {
        self.timeline().map(|timeline| {
            format!(
                "From: {}  To: {}",
                timeline.from().label(),
                timeline.to().label()
            )
        })
    }

    pub fn comparison_states(&self) -> Option<(&ReviewContentState, &ReviewContentState)> {
        self.review_model
            .as_ref()
            .map(|_| (&self.from_state, &self.to_state))
    }

    pub fn restore_confirmation(&self) -> Option<&RestoreConfirmation> {
        self.restore_confirmation.as_ref()
    }

    pub fn restore_confirmation_text(&self) -> Option<String> {
        self.restore_confirmation
            .as_ref()
            .map(RestoreConfirmation::text)
    }

    pub fn can_restore_selected_version(&self) -> bool {
        matches!(
            self.timeline().map(ReviewTimeline::from),
            Some(ReviewEndpoint::Historical(_))
        ) && !self.resolved
            && !self.decline_pending
            && !self.review_loading
    }

    pub fn new_with_mux(
        file_path: PathBuf,
        response: mux_protocol::GetFileReviewStateResponse,
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        _workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_mux(file_path, response, domain, session_id, cx)
    }

    fn restore_target_for_current_selection(&self) -> Option<RestoreTarget> {
        let source = self.review_source.as_ref()?;
        let model = self.review_model.as_ref()?;
        let version_id = self.historical_version_id(ReviewEndpointRole::From)?;
        Some(RestoreTarget::new(
            source.domain.clone(),
            source.session_id.clone(),
            version_id,
            model.latest_seq_no(),
            model.current_exists(),
            model.current_sha256().to_vec(),
        ))
    }

    fn historical_version_id(&self, role: ReviewEndpointRole) -> Option<u64> {
        self.timeline().and_then(|timeline| {
            let endpoint = match role {
                ReviewEndpointRole::From => timeline.from(),
                ReviewEndpointRole::To => timeline.to(),
            };
            match endpoint {
                ReviewEndpoint::Historical(version_id) => Some(*version_id),
                _ => None,
            }
        })
    }

    fn set_current_endpoint(&mut self) {
        if let Some(model) = self.review_model.as_ref() {
            self.to_state = model.current_state().clone();
            self.current_content = model.current_content().unwrap_or_default().into();
        }
    }

    fn load_active_endpoint(&mut self, role: ReviewEndpointRole, cx: &mut Context<Self>) {
        let Some(source) = self.review_source.clone() else {
            return;
        };
        let endpoint = self.timeline().map(|timeline| match role {
            ReviewEndpointRole::From => timeline.from().clone(),
            ReviewEndpointRole::To => timeline.to().clone(),
        });
        let Some(endpoint) = endpoint else {
            return;
        };
        if matches!(endpoint, ReviewEndpoint::Current) {
            self.set_current_endpoint();
            cx.notify();
            return;
        }
        let ReviewEndpoint::Historical(version_id) = endpoint else {
            cx.notify();
            return;
        };
        match role {
            ReviewEndpointRole::From => {
                self.from_state =
                    ReviewContentState::Unavailable("historical content is loading".to_string());
                self.previous_content = SharedString::default();
            }
            ReviewEndpointRole::To => {
                self.to_state =
                    ReviewContentState::Unavailable("historical content is loading".to_string());
                self.current_content = SharedString::default();
            }
        }
        let path = self.file_path.to_string_lossy().into_owned();
        self.review_loading = true;
        cx.spawn(async move |this, cx| {
            let loaded = source
                .domain
                .get_file_version(&source.session_id, &path, version_id)
                .await
                .map(Self::loaded_review_content)
                .unwrap_or_else(|error| LoadedReviewContent {
                    state: ReviewContentState::Unavailable(format!(
                        "historical version {version_id} unavailable: {error:#}"
                    )),
                    content: None,
                });
            this.update(cx, |this, cx| {
                this.review_loading = false;
                this.set_loaded_endpoint(role, loaded);
                cx.notify();
            })
        })
        .detach();
    }

    pub fn previous_version(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.review_model.as_mut() else {
            return;
        };
        let role = model.timeline().active_endpoint();
        model.timeline_mut().previous();
        self.load_active_endpoint(role, cx);
    }

    pub fn next_version(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.review_model.as_mut() else {
            return;
        };
        let role = model.timeline().active_endpoint();
        model.timeline_mut().next();
        self.load_active_endpoint(role, cx);
    }

    pub fn select_from_version(&mut self, version_id: u64, cx: &mut Context<Self>) {
        let Some(model) = self.review_model.as_mut() else {
            return;
        };
        model.timeline_mut().select_from(version_id);
        self.load_active_endpoint(ReviewEndpointRole::From, cx);
    }

    pub fn select_to_version(&mut self, version_id: u64, cx: &mut Context<Self>) {
        let Some(model) = self.review_model.as_mut() else {
            return;
        };
        model.timeline_mut().select_to(version_id);
        self.load_active_endpoint(ReviewEndpointRole::To, cx);
    }

    pub fn compare_to_current(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.review_model.as_mut() else {
            return;
        };
        model.timeline_mut().compare_to_current();
        self.load_active_endpoint(ReviewEndpointRole::To, cx);
    }

    pub fn restore_selected_version(&mut self, cx: &mut Context<Self>) {
        if !self.can_restore_selected_version() {
            return;
        }
        if self.restore_confirmation.is_none() {
            let Some(version_id) = self.historical_version_id(ReviewEndpointRole::From) else {
                return;
            };
            let Some(model) = self.review_model.as_ref() else {
                return;
            };
            let seq_no = model
                .timeline()
                .versions()
                .iter()
                .find(|version| version.version_id == version_id)
                .map(|version| version.seq_no)
                .unwrap_or_default();
            self.restore_confirmation = Some(RestoreConfirmation::new(
                self.file_path.clone(),
                version_id,
                seq_no,
                self.from_state.clone(),
            ));
            cx.notify();
            return;
        }
        self.confirm_restore(cx);
    }

    pub fn cancel_restore(&mut self, cx: &mut Context<Self>) {
        self.restore_confirmation = None;
        cx.notify();
    }

    fn confirm_restore(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.review_source.clone() else {
            return;
        };
        let Some(model) = self.review_model.as_ref() else {
            return;
        };
        let Some(version_id) = self.historical_version_id(ReviewEndpointRole::From) else {
            return;
        };
        self.decline_pending = true;
        self.decline_error = None;
        let path = self.file_path.to_string_lossy().into_owned();
        let expected_latest_seq_no = model.latest_seq_no();
        let expected_current_exists = model.current_exists();
        let expected_current_sha256 = model.current_sha256().to_vec();
        cx.spawn(async move |this, cx| {
            let result = source
                .domain
                .decline_file_version(
                    &source.session_id,
                    &path,
                    version_id,
                    expected_latest_seq_no,
                    expected_current_exists,
                    expected_current_sha256,
                )
                .await;
            let refreshed = if let Ok(response) = &result
                && response.restored
            {
                let review_state = source
                    .domain
                    .get_file_review_state(&source.session_id, &path)
                    .await;
                let changed_files = source.domain.list_changed_files(&source.session_id).await;
                Some((review_state, changed_files))
            } else {
                None
            };
            this.update(cx, |this, cx| {
                this.decline_pending = false;
                match (result, refreshed) {
                    (Ok(response), Some((Ok(review_state), Ok(changed_files))))
                        if response.restored =>
                    {
                        this.apply_review_state(&review_state);
                        let classification = this
                            .review_model
                            .as_ref()
                            .map(ReviewStateModel::classification)
                            .unwrap_or(ReviewClassification::Unavailable);
                        if let Some(queue) = this.queue.as_mut() {
                            queue.refresh_changed_files(&changed_files.files);
                            queue.mark_reviewed_with_classification(
                                this.queue_index,
                                review_state.latest_seq_no,
                                review_state.current_exists,
                                review_state.current_sha256.clone(),
                                classification,
                            );
                        }
                        this.restore_confirmation = None;
                        // Deliberately not `resolved`: a restore appends a new
                        // version rather than closing the file out, and §4 asks
                        // that the user can keep comparing and undo it. Marking
                        // it resolved hid the Restore control until the review
                        // was reopened. The queue entry is already marked
                        // reviewed above, so the queue still advances.
                        cx.emit(DiffReviewEvent::Declined);
                        this.advance_after_success(cx);
                    }
                    (Ok(_), Some((Err(error), _))) | (Ok(_), Some((_, Err(error)))) => {
                        // The confirmation described an operation that is over;
                        // leaving it up next to the failure reads as if it were
                        // still awaiting a second press.
                        this.restore_confirmation = None;
                        this.mark_current_queue_item_needs_refresh();
                        this.decline_error =
                            Some(format!("Restore succeeded but refresh failed: {error:#}").into());
                    }
                    (Ok(_), _) => {
                        this.restore_confirmation = None;
                        this.mark_current_queue_item_needs_refresh();
                        this.decline_error =
                            Some("shadow restore was not confirmed".to_string().into());
                    }
                    (Err(error), _) => {
                        this.restore_confirmation = None;
                        this.mark_current_queue_item_needs_refresh();
                        this.decline_error = Some(format!("Restore failed: {error:#}").into());
                        if let Some(model) = this.review_model.as_mut() {
                            model.retain_error(format!("Restore failed: {error:#}"));
                        }
                    }
                }
                cx.notify();
            })
        })
        .detach();
        cx.notify();
    }

    pub fn refresh_current(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.review_source.clone() else {
            return;
        };
        if self.review_loading || self.decline_pending {
            return;
        }
        let path = self.file_path.to_string_lossy().into_owned();
        self.review_loading = true;
        cx.spawn(async move |this, cx| {
            let result = source
                .domain
                .get_file_review_state(&source.session_id, &path)
                .await;
            let changed_files = source.domain.list_changed_files(&source.session_id).await;
            this.update(cx, |this, cx| {
                this.review_loading = false;
                if let Ok(changed_files) = &changed_files
                    && let Some(queue) = this.queue.as_mut()
                {
                    queue.refresh_changed_files(&changed_files.files);
                }
                match result {
                    Ok(review_state) => {
                        this.queue_error = changed_files
                            .err()
                            .map(|error| format!("Queue refresh failed: {error:#}").into());
                        this.apply_review_state(&review_state);
                        this.refresh_queue_entry(this.queue_index, &review_state);
                        let role = this
                            .review_model
                            .as_ref()
                            .map(|model| model.timeline().active_endpoint())
                            .unwrap_or(ReviewEndpointRole::From);
                        this.load_active_endpoint(role, cx);
                    }
                    Err(error) => {
                        this.queue_error = Some(format!("Refresh failed: {error:#}").into());
                        if let Some(model) = this.review_model.as_mut() {
                            model.retain_error(format!("Refresh failed: {error:#}"));
                        }
                    }
                }
                cx.notify();
            })
        })
        .detach();
        cx.notify();
    }

    fn apply_review_state(&mut self, response: &mux_protocol::GetFileReviewStateResponse) {
        let old_to_is_current = self
            .review_model
            .as_ref()
            .map(|model| matches!(model.timeline().to(), ReviewEndpoint::Current))
            .unwrap_or(true);
        if let Some(model) = self.review_model.as_mut() {
            model.refresh_from(response);
        } else {
            self.review_model = Some(ReviewStateModel::from_response(response));
        }
        if let Some(model) = self.review_model.as_ref() {
            self.current_file_exists = model.current_exists();
            if old_to_is_current {
                self.current_content = model.current_content().unwrap_or_default().into();
                self.to_state = model.current_state().clone();
            }
            self.restore_target = self.restore_target_for_current_selection();
        }
    }

    fn replace_review_state(&mut self, response: &mux_protocol::GetFileReviewStateResponse) {
        self.review_model = Some(ReviewStateModel::from_response(response));
        if let Some(model) = self.review_model.as_ref() {
            self.current_file_exists = model.current_exists();
            self.current_content = model.current_content().unwrap_or_default().into();
            self.to_state = model.current_state().clone();
            self.previous_content = SharedString::default();
            self.from_state =
                ReviewContentState::Unavailable("historical content is loading".to_string());
            self.restore_target = self.restore_target_for_current_selection();
        }
    }

    fn refresh_queue_entry(
        &mut self,
        index: usize,
        response: &mux_protocol::GetFileReviewStateResponse,
    ) {
        let classification = self.classify(response);
        if let Some(queue) = self.queue.as_mut() {
            queue.mark_refreshed(
                index,
                response.latest_seq_no,
                response.current_exists,
                response.current_sha256.clone(),
                response.versions.len() as u64,
                classification,
            );
        }
    }

    fn advance_after_success(&mut self, cx: &mut Context<Self>) {
        let next = self
            .queue
            .as_ref()
            .and_then(|queue| queue.next_unreviewed(self.queue_index));
        if let Some(index) = next {
            self.navigate_queue_to(index, cx);
        }
    }

    pub fn navigate_previous_file(&mut self, cx: &mut Context<Self>) {
        let next = self
            .queue
            .as_ref()
            .and_then(|queue| queue.previous(self.queue_index));
        if let Some(index) = next {
            self.navigate_queue_to(index, cx);
        }
    }

    pub fn navigate_next_file(&mut self, cx: &mut Context<Self>) {
        let next = self
            .queue
            .as_ref()
            .and_then(|queue| queue.next(self.queue_index));
        if let Some(index) = next {
            self.navigate_queue_to(index, cx);
        }
    }

    fn navigate_queue_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.review_loading || self.decline_pending {
            return;
        }
        let Some(source) = self.review_source.clone() else {
            return;
        };
        let Some(path) = self
            .queue
            .as_ref()
            .and_then(|queue| queue.entry(index))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        self.review_loading = true;
        if let Some(queue) = self.queue.as_mut() {
            queue.mark_loading(index);
        }
        let path_string = path.to_string_lossy().into_owned();
        cx.spawn(async move |this, cx| {
            let result = source
                .domain
                .get_file_review_state(&source.session_id, &path_string)
                .await;
            let changed_files = source.domain.list_changed_files(&source.session_id).await;
            this.update(cx, |this, cx| {
                this.review_loading = false;
                if let Ok(changed_files) = &changed_files
                    && let Some(queue) = this.queue.as_mut()
                {
                    queue.refresh_changed_files(&changed_files.files);
                }
                match result {
                    Ok(response) => {
                        this.file_path = path.clone();
                        this.title = format!(
                            "Diff: {}",
                            path.file_name()
                                .map(|name| name.to_string_lossy())
                                .unwrap_or_else(|| path.to_string_lossy())
                        )
                        .into();
                        this.queue_index = index;
                        this.resolved = false;
                        this.decline_error = None;
                        this.restore_confirmation = None;
                        this.queue_error = changed_files
                            .err()
                            .map(|error| format!("Queue refresh failed: {error:#}").into());
                        this.replace_review_state(&response);
                        this.refresh_queue_entry(index, &response);
                        let role = this
                            .review_model
                            .as_ref()
                            .map(|model| model.timeline().active_endpoint())
                            .unwrap_or(ReviewEndpointRole::From);
                        this.load_active_endpoint(role, cx);
                    }
                    Err(error) => {
                        if let Some(queue) = this.queue.as_mut() {
                            queue.mark_unavailable(index);
                        }
                        this.queue_error =
                            Some(format!("Could not open {}: {error:#}", path.display()).into());
                    }
                }
                cx.notify();
            })
        })
        .detach();
        cx.notify();
    }

    pub fn comparison_is_available(&self) -> bool {
        self.from_state.is_text_comparable() && self.to_state.is_text_comparable()
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
        let expected_latest_seq_no = restore_target.expected_latest_seq_no;
        let expected_current_exists = restore_target.expected_current_exists;
        let expected_current_sha256 = restore_target.expected_current_sha256.clone();
        let path = self.file_path.to_string_lossy().into_owned();
        cx.spawn(async move |this, cx| {
            let result = domain
                .decline_file_version(
                    &session_id,
                    &path,
                    version_id,
                    expected_latest_seq_no,
                    expected_current_exists,
                    expected_current_sha256,
                )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let bg = colors.editor_background;
        let fg = colors.text;
        let is_mux_review = self.review_model.is_some();
        let diff_lines = if self.comparison_is_available() {
            self.line_diff()
        } else {
            Vec::new()
        };
        let file_name = self
            .file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.file_path.to_string_lossy().into_owned());

        let mut header = div()
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
                    .child(SharedString::from(format!("Diff: {file_name}")))
                    .when(self.is_deleted(), |this| {
                        this.child(Label::new("Deleted").color(Color::Error))
                    }),
            );

        let mut header_actions = div().flex().flex_row().gap_2();
        if is_mux_review {
            header_actions = header_actions
                .child(
                    div()
                        .id("accept-btn")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(colors.border)
                        .text_color(fg)
                        .child("Accept")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(AcceptCurrent.boxed_clone(), cx);
                        }),
                )
                .when(self.can_restore_selected_version(), |this| {
                    this.child(
                        div()
                            .id("restore-btn")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(colors.border)
                            .text_color(fg)
                            .child("Restore")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(RestoreSelectedVersion.boxed_clone(), cx);
                            }),
                    )
                })
                .child(
                    div()
                        .id("refresh-btn")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(colors.border)
                        .text_color(fg)
                        .child("Refresh")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(RefreshCurrent.boxed_clone(), cx);
                        }),
                );
        } else {
            header_actions = header_actions
                .child(
                    div()
                        .id("accept-btn")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(colors.border)
                        .text_color(fg)
                        .child("Accept (a)")
                        .on_click(cx.listener(|this, _, _, cx| this.accept(cx))),
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
                        .on_click(cx.listener(|this, _, _, cx| this.decline(cx)))
                } else {
                    div()
                        .id("decline-unavailable")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(colors.border)
                        .text_color(colors.text_muted)
                        .child("No snapshot")
                });
        }
        header = header.child(header_actions);

        let mut timeline = v_flex().gap_1().p_2();
        if let Some(review_model) = self.review_model.as_ref() {
            let current_from = review_model.timeline().from().clone();
            let current_to = review_model.timeline().to().clone();
            for version in review_model.timeline().versions().iter().cloned() {
                let version_id = version.version_id;
                let from_selected = current_from == ReviewEndpoint::Historical(version_id);
                let to_selected = current_to == ReviewEndpoint::Historical(version_id);
                let from_label = format!(
                    "{}  SeqNo {}  {}",
                    version.trigger_label(),
                    version.seq_no,
                    version_id
                );
                let to_version_id = version.version_id;
                timeline = timeline.child(
                    h_flex()
                        .gap_1()
                        .child(
                            div()
                                .id(("timeline-from", version_id))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .when(from_selected, |this| this.bg(colors.element_selected))
                                .child(format!("From: {from_label}"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_from_version(version_id, cx);
                                })),
                        )
                        .child(
                            div()
                                .id(("timeline-to", to_version_id))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .when(to_selected, |this| this.bg(colors.element_selected))
                                .child("Compare to")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_to_version(to_version_id, cx);
                                })),
                        ),
                );
            }
            timeline = timeline
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .id("compare-current")
                                .px_2()
                                .py_1()
                                .bg(colors.border)
                                .child("Compare to Current")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.compare_to_current(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("previous-version")
                                .px_2()
                                .py_1()
                                .bg(colors.border)
                                .child("Previous version")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(PreviousVersion.boxed_clone(), cx);
                                }),
                        )
                        .child(
                            div()
                                .id("next-version")
                                .px_2()
                                .py_1()
                                .bg(colors.border)
                                .child("Next version")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(NextVersion.boxed_clone(), cx);
                                }),
                        ),
                )
                .child(
                    Label::new(
                        self.comparison_header()
                            .unwrap_or_else(|| "From: unavailable  To: unavailable".to_string()),
                    )
                    .color(Color::Custom(fg)),
                );
            if let Some(text) = self.restore_confirmation_text() {
                timeline = timeline.child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(text).color(Color::Warning))
                        .child(
                            div()
                                .id("confirm-restore")
                                .px_2()
                                .py_1()
                                .bg(colors.border)
                                .child("Confirm restore")
                                .on_click(|_, window, cx| {
                                    window
                                        .dispatch_action(RestoreSelectedVersion.boxed_clone(), cx);
                                }),
                        )
                        .child(
                            div()
                                .id("cancel-restore")
                                .px_2()
                                .py_1()
                                .bg(colors.border)
                                .child("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_restore(cx))),
                        ),
                );
            }
            if let Some(error) = self
                .decline_error
                .as_ref()
                .or(self.queue_error.as_ref())
                .or(self.queue_sync_error.as_ref())
            {
                timeline = timeline.child(Label::new(error.clone()).color(Color::Error));
            }
            if let Some(error) = self.review_model.as_ref().and_then(|model| model.error()) {
                timeline = timeline.child(Label::new(error.to_string()).color(Color::Error));
            }
            if self.review_loading {
                timeline = timeline.child(Label::new("Loading review state…").color(Color::Muted));
            }
            if !self.comparison_is_available() {
                let states = self
                    .comparison_states()
                    .map(|(from, to)| format!("{} → {}", from.label(), to.label()))
                    .unwrap_or_else(|| "Unavailable".to_string());
                timeline = timeline.child(Label::new(format!("Comparison unavailable: {states}")));
            }
        }

        let mut body = div().flex().flex_col().py_1().px_2().size_full();
        for (index, line) in diff_lines.iter().enumerate() {
            let (text, color) = match line {
                DiffLine::Unchanged(text) => (text.as_str(), fg),
                DiffLine::Added(text) => (text.as_str(), colors.editor_foreground),
                DiffLine::Removed(text) => (text.as_str(), colors.editor_foreground),
                DiffLine::Modified { new, .. } => (new.as_str(), colors.editor_foreground),
            };
            let background = match line {
                DiffLine::Added(_) => gpui::rgb(0x2d5a1e),
                DiffLine::Removed(_) => gpui::rgb(0x5a1e1e),
                DiffLine::Modified { .. } => gpui::rgb(0x5a4a1e),
                DiffLine::Unchanged(_) => gpui::rgb(0x000000),
            };
            let prefix = match line {
                DiffLine::Unchanged(_) => " ",
                DiffLine::Added(_) => "+",
                DiffLine::Removed(_) => "-",
                DiffLine::Modified { .. } => "~",
            };
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .bg(background)
                    .child(
                        div()
                            .w(px(40.0))
                            .text_color(gpui::rgb(0x888888))
                            .child(SharedString::from(format!("{}", index + 1))),
                    )
                    .child(
                        div()
                            .text_color(color)
                            .child(SharedString::from(format!("{prefix}{text}"))),
                    ),
            );
        }

        let queue_rail = self.queue.as_ref().map(|queue| {
            let mut rows = v_flex()
                .id("review-queue-scroll")
                .overflow_y_scroll()
                .gap_1()
                .flex_1();
            for (index, entry) in queue.entries().iter().enumerate() {
                let selected = index == self.queue_index;
                let status = match entry.status {
                    ReviewQueueStatus::Pending => "Pending",
                    ReviewQueueStatus::Reviewed => "Reviewed",
                    ReviewQueueStatus::NeedsRefresh => "Needs refresh",
                    ReviewQueueStatus::Loading => "Loading",
                    ReviewQueueStatus::Unavailable => "Unavailable",
                };
                let label = format!(
                    "{} · {status}\n{}",
                    entry.classification.label(),
                    entry.path.display()
                );
                rows = rows.child(
                    div()
                        .id(("review-queue-entry", index))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .when(selected, |this| this.bg(colors.element_selected))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_queue_to(index, cx);
                        })),
                );
            }
            v_flex()
                .w(rems(28.))
                .h_full()
                .p_2()
                .gap_2()
                .border_r_1()
                .border_color(colors.border)
                .child(Label::new("Changed Files"))
                .child(Label::new(queue.progress_label()).color(Color::Muted))
                .child(rows)
        });

        let mut root = div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(bg)
            .text_color(fg)
            .font_family("monospace")
            .text_size(px(13.0));
        if is_mux_review {
            root = root
                .key_context("ChangedFilesReview")
                .on_action(cx.listener(|this, _: &PreviousVersion, _, cx| {
                    this.previous_version(cx);
                }))
                .on_action(cx.listener(|this, _: &NextVersion, _, cx| {
                    this.next_version(cx);
                }))
                .on_action(cx.listener(|this, _: &PreviousFile, _, cx| {
                    this.navigate_previous_file(cx);
                }))
                .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                    this.navigate_next_file(cx);
                }))
                .on_action(cx.listener(|this, _: &CompareCurrent, _, cx| {
                    this.compare_to_current(cx);
                }))
                .on_action(cx.listener(|this, _: &RestoreSelectedVersion, _, cx| {
                    this.restore_selected_version(cx);
                }))
                .on_action(cx.listener(|this, _: &RefreshCurrent, _, cx| {
                    this.refresh_current(cx);
                }))
                .on_action(cx.listener(|this, _: &AcceptCurrent, _, cx| {
                    this.accept(cx);
                }))
                .on_action(cx.listener(|_, _: &CloseReview, _, cx| {
                    cx.emit(ItemEvent::CloseItem);
                }));
        }
        let content = div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(timeline)
            .child(body);
        if let Some(queue_rail) = queue_rail {
            root.child(
                div()
                    .flex()
                    .flex_row()
                    .size_full()
                    .child(queue_rail)
                    .child(content),
            )
        } else {
            root.child(content)
        }
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

/// Process-local review status. This intentionally is not persisted with
/// Shadow Snapshot history: accepting a file only resolves this open review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewQueueStatus {
    Pending,
    Reviewed,
    NeedsRefresh,
    Loading,
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct ReviewQueueEntry {
    pub path: PathBuf,
    pub latest_seq_no: u64,
    pub version_count: u64,
    pub classification: ReviewClassification,
    pub status: ReviewQueueStatus,
    pub current_exists: Option<bool>,
    pub current_sha256: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct ReviewQueue {
    entries: Vec<ReviewQueueEntry>,
}

impl ReviewQueue {
    pub fn from_changed_files(files: Vec<mux_protocol::ChangedFile>) -> Self {
        Self {
            entries: files
                .into_iter()
                .map(|file| ReviewQueueEntry {
                    classification: queue_row_classification(&file),
                    path: PathBuf::from(file.path),
                    latest_seq_no: file.latest_seq_no,
                    version_count: file.version_count,
                    status: ReviewQueueStatus::Pending,
                    current_exists: None,
                    current_sha256: None,
                })
                .collect(),
        }
    }

    pub fn entries(&self) -> &[ReviewQueueEntry] {
        &self.entries
    }

    pub fn entry(&self, index: usize) -> Option<&ReviewQueueEntry> {
        self.entries.get(index)
    }

    pub fn status(&self, index: usize) -> Option<ReviewQueueStatus> {
        self.entry(index).map(|entry| entry.status)
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Show that `index` is being fetched, without forgetting that it was
    /// already reviewed.
    ///
    /// `mark_refreshed` decides "still reviewed" by looking at the current
    /// status, so overwriting a `Reviewed` entry here made that check
    /// impossible and every revisit demoted the entry back to Pending.
    pub fn mark_loading(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index)
            && entry.status != ReviewQueueStatus::Reviewed
        {
            entry.status = ReviewQueueStatus::Loading;
        }
    }

    pub fn mark_unavailable(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.status = ReviewQueueStatus::Unavailable;
        }
    }
    pub fn mark_reviewed(
        &mut self,
        index: usize,
        latest_seq_no: u64,
        current_exists: bool,
        current_sha256: Vec<u8>,
    ) {
        self.mark_reviewed_with_classification(
            index,
            latest_seq_no,
            current_exists,
            current_sha256,
            ReviewClassification::Modified,
        );
    }

    pub fn mark_refreshed(
        &mut self,
        index: usize,
        latest_seq_no: u64,
        current_exists: bool,
        current_sha256: Vec<u8>,
        version_count: u64,
        classification: ReviewClassification,
    ) {
        if let Some(entry) = self.entries.get_mut(index) {
            let was_reviewed = entry.status == ReviewQueueStatus::Reviewed
                && entry.latest_seq_no == latest_seq_no
                && entry.current_exists == Some(current_exists)
                && entry.current_sha256.as_deref() == Some(current_sha256.as_slice());
            entry.latest_seq_no = latest_seq_no;
            entry.version_count = version_count;
            entry.current_exists = Some(current_exists);
            entry.current_sha256 = Some(current_sha256);
            entry.classification = classification;
            entry.status = if was_reviewed {
                ReviewQueueStatus::Reviewed
            } else {
                ReviewQueueStatus::Pending
            };
        }
    }

    pub fn mark_reviewed_with_classification(
        &mut self,
        index: usize,
        latest_seq_no: u64,
        current_exists: bool,
        current_sha256: Vec<u8>,
        classification: ReviewClassification,
    ) {
        if let Some(entry) = self.entries.get_mut(index) {
            if entry.latest_seq_no == latest_seq_no {
                entry.current_exists = Some(current_exists);
                entry.current_sha256 = Some(current_sha256);
                entry.classification = classification;
                entry.status = ReviewQueueStatus::Reviewed;
            } else {
                entry.status = ReviewQueueStatus::NeedsRefresh;
            }
        }
    }

    pub fn refresh_changed_files(&mut self, files: &[mux_protocol::ChangedFile]) {
        for file in files {
            let path = Path::new(&file.path);
            if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
                if entry.latest_seq_no != file.latest_seq_no {
                    entry.latest_seq_no = file.latest_seq_no;
                    entry.version_count = file.version_count;
                    entry.current_exists = None;
                    entry.current_sha256 = None;
                    // Fall back to what the listing knows rather than blanking
                    // the row: a new version does not make the file unreadable.
                    entry.classification = queue_row_classification(file);
                    entry.status = ReviewQueueStatus::NeedsRefresh;
                } else {
                    entry.version_count = file.version_count;
                }
            } else {
                self.entries.push(ReviewQueueEntry {
                    classification: queue_row_classification(file),
                    path: path.to_path_buf(),
                    latest_seq_no: file.latest_seq_no,
                    version_count: file.version_count,
                    status: ReviewQueueStatus::Pending,
                    current_exists: None,
                    current_sha256: None,
                });
            }
        }
    }

    pub fn next_unreviewed(&self, current_index: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .skip(current_index.saturating_add(1))
            .find_map(|(index, entry)| {
                // An entry that could not be loaded is not reviewable, so
                // advancing onto it would park the queue there forever.
                (!matches!(
                    entry.status,
                    ReviewQueueStatus::Reviewed | ReviewQueueStatus::Unavailable
                ))
                .then_some(index)
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .take(current_index)
                    .find_map(|(index, entry)| {
                        (entry.status != ReviewQueueStatus::Reviewed).then_some(index)
                    })
            })
    }

    pub fn previous(&self, current_index: usize) -> Option<usize> {
        current_index
            .checked_sub(1)
            .or_else(|| self.entries.len().checked_sub(1))
            .filter(|index| *index < self.entries.len())
    }

    pub fn next(&self, current_index: usize) -> Option<usize> {
        let next = current_index.saturating_add(1);
        (next < self.entries.len()).then_some(next)
    }

    pub fn progress(&self) -> (usize, usize) {
        (
            self.entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.status,
                        ReviewQueueStatus::Reviewed | ReviewQueueStatus::Unavailable
                    )
                })
                .count(),
            self.entries.len(),
        )
    }

    pub fn progress_label(&self) -> String {
        let (reviewed, total) = self.progress();
        if total == 0 {
            "No changed files".to_string()
        } else if reviewed == total {
            format!("All changed files reviewed ({reviewed}/{total})")
        } else {
            format!("Changed files reviewed ({reviewed}/{total})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[test]
    fn review_queue_marks_changed_pending_and_reviewed_entries_needs_refresh() {
        let mut queue = ReviewQueue::from_changed_files(vec![
            changed_file("/tmp/alpha", 4, 2),
            changed_file("/tmp/beta", 3, 1),
        ]);
        queue.mark_reviewed(0, 4, true, vec![1; 32]);

        queue.refresh_changed_files(&[
            changed_file("/tmp/alpha", 5, 3),
            changed_file("/tmp/beta", 3, 1),
            changed_file("/tmp/gamma", 6, 1),
        ]);

        assert_eq!(queue.status(0), Some(ReviewQueueStatus::NeedsRefresh));
        assert_eq!(queue.status(1), Some(ReviewQueueStatus::Pending));
        assert_eq!(queue.status(2), Some(ReviewQueueStatus::Pending));
        assert_eq!(
            queue.paths(),
            vec![
                PathBuf::from("/tmp/alpha"),
                PathBuf::from("/tmp/beta"),
                PathBuf::from("/tmp/gamma"),
            ]
        );
        assert_eq!(queue.progress(), (0, 3));
    }

    #[test]
    fn review_queue_preserves_historical_count_after_accept() {
        let mut queue = ReviewQueue::from_changed_files(vec![changed_file("/tmp/alpha", 7, 4)]);
        queue.mark_reviewed(0, 7, true, vec![2; 32]);
        queue.refresh_changed_files(&[changed_file("/tmp/alpha", 7, 4)]);

        assert_eq!(queue.entry(0).map(|entry| entry.version_count), Some(4));
        assert_eq!(queue.status(0), Some(ReviewQueueStatus::Reviewed));
        assert_eq!(queue.progress(), (1, 1));
    }

    #[test]
    fn review_queue_advances_only_to_unreviewed_items() {
        let mut queue = ReviewQueue::from_changed_files(vec![
            changed_file("/tmp/alpha", 1, 1),
            changed_file("/tmp/beta", 2, 1),
            changed_file("/tmp/gamma", 3, 1),
        ]);
        queue.mark_reviewed(0, 1, true, vec![3; 32]);
        assert_eq!(queue.next_unreviewed(0), Some(1));
        queue.mark_reviewed(1, 2, true, vec![4; 32]);
        assert_eq!(queue.next_unreviewed(1), Some(2));
        queue.mark_reviewed(2, 3, true, vec![5; 32]);
        assert_eq!(queue.next_unreviewed(2), None);
    }

    #[test]
    fn queue_refresh_classifies_reopened_entries_without_marking_reviewed() {
        let mut queue = ReviewQueue::from_changed_files(vec![changed_file("/tmp/alpha", 7, 4)]);
        queue.mark_reviewed_with_classification(
            0,
            7,
            true,
            vec![2; 32],
            ReviewClassification::Modified,
        );

        queue.mark_refreshed(0, 8, false, vec![3; 32], 5, ReviewClassification::Deleted);

        let entry = queue.entry(0).expect("queue entry");
        assert_eq!(entry.classification, ReviewClassification::Deleted);
        assert_eq!(entry.status, ReviewQueueStatus::Pending);
        assert_eq!(entry.version_count, 5);
        assert_eq!(queue.progress(), (0, 1));
    }

    /// Revisiting an accepted file must not undo the acceptance. The
    /// navigation path marks the row loading before it refreshes it, and the
    /// refresh decides "still reviewed" from the current status — so a
    /// `Loading` overwrite silently demoted the row and walked the progress
    /// counter backwards.
    #[test]
    fn revisiting_a_reviewed_file_keeps_it_reviewed() {
        let mut queue = ReviewQueue::from_changed_files(vec![
            changed_file("/tmp/alpha", 7, 4),
            changed_file("/tmp/beta", 9, 2),
        ]);
        queue.mark_reviewed_with_classification(
            0,
            7,
            true,
            vec![2; 32],
            ReviewClassification::Modified,
        );
        assert_eq!(queue.progress(), (1, 2));

        // Exactly what `navigate_queue_to` does when the user goes back.
        queue.mark_loading(0);
        queue.refresh_changed_files(&[changed_file("/tmp/alpha", 7, 4)]);
        queue.mark_refreshed(0, 7, true, vec![2; 32], 4, ReviewClassification::Modified);

        assert_eq!(queue.status(0), Some(ReviewQueueStatus::Reviewed));
        assert_eq!(queue.progress(), (1, 2));
        assert_eq!(queue.next_unreviewed(0), Some(1));
    }

    /// The listing carries existence and the oldest trigger, so a queue row
    /// says what kind of change it is before the file is opened. Every row
    /// reading "Unavailable" told the user nothing.
    #[test]
    fn queue_rows_classify_from_the_listing_alone() {
        let mut added = changed_file("/tmp/added", 1, 1);
        added.first_trigger = "create".to_string();
        let mut deleted = changed_file("/tmp/deleted", 2, 2);
        deleted.current_exists = false;
        let modified = changed_file("/tmp/modified", 3, 2);

        let queue = ReviewQueue::from_changed_files(vec![added, deleted, modified]);
        let kinds = queue
            .entries()
            .iter()
            .map(|entry| entry.classification)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                ReviewClassification::Added,
                ReviewClassification::Deleted,
                ReviewClassification::Modified,
            ]
        );
    }

    /// A new version means "look again", not "this file became unreadable".
    #[test]
    fn a_new_version_keeps_the_row_classified() {
        let mut queue = ReviewQueue::from_changed_files(vec![changed_file("/tmp/alpha", 7, 4)]);
        let mut deleted = changed_file("/tmp/alpha", 8, 5);
        deleted.current_exists = false;

        queue.refresh_changed_files(&[deleted]);

        let entry = queue.entry(0).expect("queue entry");
        assert_eq!(entry.status, ReviewQueueStatus::NeedsRefresh);
        assert_eq!(entry.classification, ReviewClassification::Deleted);
    }

    /// A file that could not be loaded is not reviewable, so the queue has to
    /// step past it. Treating it as outstanding parked auto-advance on that
    /// row and kept the queue from ever reporting completion.
    #[test]
    fn an_unloadable_entry_does_not_stall_the_queue() {
        let mut queue = ReviewQueue::from_changed_files(vec![
            changed_file("/tmp/alpha", 7, 4),
            changed_file("/tmp/beta", 9, 2),
        ]);
        queue.mark_unavailable(1);
        queue.mark_reviewed_with_classification(
            0,
            7,
            true,
            vec![2; 32],
            ReviewClassification::Modified,
        );

        assert_eq!(queue.next_unreviewed(0), None);
        assert_eq!(queue.progress(), (2, 2));
        assert_eq!(queue.progress_label(), "All changed files reviewed (2/2)");
    }

    #[test]
    fn review_queue_empty_and_complete_states_are_distinct() {
        let empty = ReviewQueue::default();
        assert_eq!(empty.progress_label(), "No changed files");

        let mut complete = ReviewQueue::from_changed_files(vec![changed_file("/tmp/alpha", 1, 1)]);
        complete.mark_reviewed(0, 1, true, vec![6; 32]);
        assert_eq!(
            complete.progress_label(),
            "All changed files reviewed (1/1)"
        );
    }

    fn changed_file(
        path: &str,
        latest_seq_no: u64,
        version_count: u64,
    ) -> mux_protocol::ChangedFile {
        mux_protocol::ChangedFile {
            path: path.to_string(),
            latest_seq_no,
            version_count,
            current_exists: true,
            first_trigger: "write".to_string(),
        }
    }
}
#[cfg(test)]
mod timeline_tests {
    use super::*;

    fn review_response(
        versions: Vec<(u64, u64, &str)>,
        latest_seq_no: u64,
        current_content: &[u8],
        current_state: mux_protocol::FileContentState,
    ) -> mux_protocol::GetFileReviewStateResponse {
        mux_protocol::GetFileReviewStateResponse {
            versions: versions
                .into_iter()
                .map(|(version_id, seq_no, trigger)| mux_protocol::FileVersion {
                    version_id,
                    seq_no,
                    trigger: trigger.to_string(),
                })
                .collect(),
            latest_seq_no,
            current_exists: current_state != mux_protocol::FileContentState::Deleted,
            current_size: current_content.len() as u64,
            current_sha256: vec![7; 32],
            current_state: current_state as i32,
            current_content: current_content.to_vec(),
        }
    }

    #[test]
    fn review_timeline_is_strictly_ascending_by_seq_no() {
        let response = review_response(
            vec![(3, 30, "write"), (1, 10, "create"), (2, 20, "close")],
            30,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let timeline = ReviewTimeline::from_response(&response);
        assert_eq!(
            timeline
                .versions()
                .iter()
                .map(|version| version.seq_no)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert_eq!(timeline.from(), &ReviewEndpoint::Historical(2));
        assert_eq!(timeline.to(), &ReviewEndpoint::Current);
    }

    #[test]
    fn review_timeline_supports_current_and_history_endpoints() {
        let response = review_response(
            vec![(1, 10, "create"), (2, 20, "write"), (3, 30, "write")],
            30,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let mut timeline = ReviewTimeline::from_response(&response);
        timeline.select_from(1);
        timeline.select_to(2);
        assert_eq!(timeline.from(), &ReviewEndpoint::Historical(1));
        assert_eq!(timeline.to(), &ReviewEndpoint::Historical(2));
        timeline.compare_to_current();
        assert_eq!(timeline.to(), &ReviewEndpoint::Current);
    }

    #[test]
    fn history_history_comparison_keeps_both_selected_versions() {
        let response = review_response(
            vec![(1, 10, "create"), (2, 20, "write"), (3, 30, "write")],
            30,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let mut timeline = ReviewTimeline::from_response(&response);
        timeline.select_from(1);
        timeline.select_to(2);
        assert_eq!(timeline.from().label(), "Version 1");
        assert_eq!(timeline.to().label(), "Version 2");
        assert_eq!(timeline.active_endpoint(), ReviewEndpointRole::To);
    }

    #[test]
    fn adjacent_version_navigation_clamps_at_endpoints() {
        let response = review_response(
            vec![(1, 10, "create"), (2, 20, "write"), (3, 30, "write")],
            30,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let mut timeline = ReviewTimeline::from_response(&response);
        timeline.select_from(1);
        assert_eq!(timeline.previous(), ReviewEndpoint::Historical(1));
        assert_eq!(timeline.next(), ReviewEndpoint::Historical(2));
        assert_eq!(timeline.next(), ReviewEndpoint::Historical(3));
        assert_eq!(timeline.next(), ReviewEndpoint::Historical(3));
    }

    #[test]
    fn typed_unavailable_states_are_not_lossily_decoded() {
        assert_eq!(
            ReviewContentState::from_proto(mux_protocol::FileContentState::Binary as i32),
            ReviewContentState::Binary
        );
        assert_eq!(
            ReviewContentState::from_proto(mux_protocol::FileContentState::TooLarge as i32),
            ReviewContentState::TooLarge
        );
        assert_eq!(
            ReviewContentState::from_proto(mux_protocol::FileContentState::Deleted as i32),
            ReviewContentState::Deleted
        );
        assert!(!ReviewContentState::Binary.is_text_comparable());
    }

    #[test]
    fn restore_confirmation_describes_target_and_operation() {
        let replace = RestoreConfirmation::new(
            PathBuf::from("/srv/project/readme.md"),
            42,
            17,
            ReviewContentState::Text,
        );
        assert_eq!(
            replace.text(),
            "Restore /srv/project/readme.md to version 42 (SeqNo 17)? \
             Current text bytes will be replaced; all history remains."
        );

        let remove = RestoreConfirmation::new(
            PathBuf::from("/srv/project/readme.md"),
            43,
            18,
            ReviewContentState::Deleted,
        );
        assert!(remove.text().contains("current file will be removed"));
    }

    #[test]
    fn refresh_adds_restore_node_and_retains_selection_after_success() {
        let initial = review_response(
            vec![(1, 10, "create"), (2, 20, "write")],
            20,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let refreshed = review_response(
            vec![(1, 10, "create"), (2, 20, "write"), (3, 30, "restore")],
            30,
            b"restored",
            mux_protocol::FileContentState::Text,
        );
        let mut model = ReviewStateModel::from_response(&initial);
        model.select_from(1);
        assert!(model.refresh_from(&refreshed));
        assert_eq!(model.timeline().from(), &ReviewEndpoint::Historical(1));
        assert_eq!(
            model
                .timeline()
                .versions()
                .last()
                .map(|version| version.version_id),
            Some(3)
        );
        assert_eq!(
            model
                .timeline()
                .versions()
                .last()
                .map(ReviewTimelineVersion::trigger_label),
            Some("Restored".to_string())
        );
    }

    /// A refresh must leave the user on the endpoint they were browsing.
    /// `select_from` / `select_to` / `compare_to_current` each move the active
    /// endpoint as a side effect, so restoring it before them was a no-op and
    /// the version arrows silently started driving the other side.
    #[test]
    fn refresh_keeps_the_endpoint_the_user_was_browsing() {
        let initial = review_response(
            vec![(1, 10, "create"), (2, 20, "write")],
            20,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let refreshed = review_response(
            vec![(1, 10, "create"), (2, 20, "write"), (3, 30, "decline")],
            30,
            b"restored",
            mux_protocol::FileContentState::Text,
        );

        let mut model = ReviewStateModel::from_response(&initial);
        model.select_from(1);
        assert_eq!(model.timeline().active_endpoint(), ReviewEndpointRole::From);

        model.refresh_from(&refreshed);
        assert_eq!(
            model.timeline().active_endpoint(),
            ReviewEndpointRole::From,
            "the browsing endpoint must survive a refresh"
        );

        model.timeline_mut().select_to(2);
        assert_eq!(model.timeline().active_endpoint(), ReviewEndpointRole::To);
        model.refresh_from(&refreshed);
        assert_eq!(model.timeline().active_endpoint(), ReviewEndpointRole::To);
    }

    #[test]
    fn refresh_failure_retains_current_selection_and_records_error() {
        let response = review_response(
            vec![(1, 10, "create"), (2, 20, "write")],
            20,
            b"current",
            mux_protocol::FileContentState::Text,
        );
        let mut model = ReviewStateModel::from_response(&response);
        model.select_from(1);
        model.retain_error("stale review");
        assert_eq!(model.timeline().from(), &ReviewEndpoint::Historical(1));
        assert_eq!(model.error(), Some("stale review"));
    }
}
