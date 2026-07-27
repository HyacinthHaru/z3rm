//! Stub types for project-crate APIs removed during dependency stripping.
//! These keep downstream crates (editor, picker, platform_title_bar) compiling.

use std::{ops::Range, path::PathBuf, sync::Arc};

use collections::BTreeMap;
use gpui::{App, Entity, SharedString, Task};
use serde::{Deserialize, Serialize};
use fs::Fs;
use extension::ExtensionProvides;
use text::Anchor;
use worktree::ProjectEntryId;

pub type CompletionId = u64;

// ---------------------------------------------------------------------------
// Inlay / hover types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum InlayId {
    Hint(u64),
    Color(u64),
    EditPrediction(u64),
    DebuggerValue(u64),
    ReplResult(u64),
}

#[derive(Clone, Debug)]
pub struct InlayHint {
    pub position: Anchor,
    pub label: InlayHintLabel,
    pub kind: lsp::InlayHintKind,
    pub text_edits: Option<Vec<lsp::TextEdit>>,
    pub tooltip: Option<InlayHintTooltip>,
    pub padding_before: bool,
    pub padding_after: bool,
}

impl InlayHint {
    pub fn text(&self) -> String {
        match &self.label {
            InlayHintLabel::String(text) => text.clone(),
            InlayHintLabel::LabelParts(parts) => parts.iter().map(|p| &p.value).cloned().collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum InlayHintLabel {
    String(String),
    LabelParts(Vec<InlayHintLabelPart>),
}

#[derive(Clone, Debug)]
pub struct InlayHintLabelPart {
    pub value: String,
    pub tooltip: Option<String>,
    pub location: Option<LocationLink>,
}

#[derive(Clone, Debug)]
pub struct InlayHintTooltip {
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct InlayHintLabelPartTooltip {
    pub text: String,
}

#[derive(Clone, Copy, Debug)]
pub enum InvalidationStrategy {
    OnBufferChange,
    OnCursorChange,
    OnFileChange,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveState {
    Pending,
    Resolved,
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoverBlockKind {
    PlainText,
    Markdown,
    Code { language: String },
}

#[derive(Clone, Debug)]
pub struct HoverBlock {
    pub text: String,
    pub kind: HoverBlockKind,
}

#[derive(Clone, Debug)]
pub struct Hover {
    pub contents: Vec<HoverBlock>,
    pub range: Option<Range<Anchor>>,
}

#[derive(Clone, Debug)]
pub struct DocumentHighlight {
    pub range: std::ops::Range<text::Anchor>,
    pub kind: lsp::DocumentHighlightKind,
}

// ---------------------------------------------------------------------------
// Links / paths
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LocationLink {
    pub origin_selection_range: Option<Range<Anchor>>,
    pub target_uri: Arc<dyn language::File>,
    pub target_range: Range<Anchor>,
    pub target_selection_range: Range<Anchor>,
}

impl std::fmt::Debug for LocationLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocationLink")
            .field("origin_selection_range", &self.origin_selection_range)
            .field("target_range", &self.target_range)
            .field("target_selection_range", &self.target_selection_range)
            .finish_non_exhaustive()
    }
}

/// Stub: navigation kind (from editor::GotoDefinitionKind)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GotoDefinitionKind {
    Symbol,
    Declaration,
    Type,
    Implementation,
}

#[derive(Clone, Debug)]
pub struct ResolvedPath {
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LanguageServerToQuery {
    pub server_id: lsp::LanguageServerId,
}

// ---------------------------------------------------------------------------
// AI / settings stubs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DisableAiSettings {
    pub disable_ai: bool,
}

impl DisableAiSettings {
    pub fn is_ai_disabled_for_buffer(_buffer: Option<&language::Buffer>, _cx: &App) -> bool {
        false
    }
    // 来源: spec §2.1 — settings 访问需要 get_global 方法
    pub fn get_global(_cx: &gpui::App) -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Document colors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DocumentColor {
    pub color: Color,
    pub range: Range<Anchor>,
}

#[derive(Clone, Debug)]
pub struct Color {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

// ---------------------------------------------------------------------------
// Symbol
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SymbolLabel {
    pub text: String,
}

impl SymbolLabel {
    pub fn filter_text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: lsp::SymbolKind,
    pub range: Range<language::PointUtf16>,
    pub label: SymbolLabel,
    pub path: Option<ProjectPath>,
}


// ---------------------------------------------------------------------------
// Diagnostic summary stubs (spec §8.2 M2)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticSummary {
    pub warning_count: usize,
    pub error_count: usize,
}

// ---------------------------------------------------------------------------
// Directory lister stub (spec §8.2 M2)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum DirectoryLister {
    Local(Arc<Project>, Arc<dyn Fs>),
    Project(Arc<Project>),
}

// ---------------------------------------------------------------------------
// Bookmark store
// ---------------------------------------------------------------------------

pub mod bookmark_store {
    use super::*;
    use std::ops::Range;

    #[derive(Default)]
    pub struct BookmarkStore;

    impl BookmarkStore {
        pub fn all_bookmark_locations(
            _store: Entity<BookmarkStore>,
            _cx: &mut gpui::AsyncApp,
        ) -> Task<anyhow::Result<std::collections::HashMap<gpui::Entity<language::Buffer>, (Vec<std::ops::Range<text::Point>>)>>> {
            Task::ready(Ok(std::collections::HashMap::new()))
        }

        /// Stub: toggle bookmark (bookmark 模块已删除)
        pub fn toggle_bookmark(
            _buffer: Entity<language::Buffer>,
            _anchor: text::Anchor,
            _label: String,
            _cx: &mut gpui::Context<Self>,
        ) {
        }

        /// Stub: bookmarks for buffer (bookmark 模块已删除)
        pub fn bookmarks_for_buffer(
            _store: Entity<BookmarkStore>,
            _buffer: &Entity<language::Buffer>,
            _cx: &mut gpui::Context<Entity<BookmarkStore>>,
        ) -> Task<anyhow::Result<Vec<text::Anchor>>> {
            Task::ready(Ok(Vec::new()))
        }

        /// Stub: find bookmark (bookmark 模块已删除)
        pub fn find_bookmark(
            _store: Entity<BookmarkStore>,
            _buffer: Entity<language::Buffer>,
            _point: text::Point,
            _cx: &gpui::App,
        ) -> Option<text::Anchor> {
            None
        }

        pub fn edit_bookmark(
            &mut self,
            _buffer: &Entity<language::Buffer>,
            _anchor: text::Anchor,
            _label: String,
            _cx: &mut gpui::Context<Self>,
        ) {
        }
    }
}

// ---------------------------------------------------------------------------
// Debugger
// ---------------------------------------------------------------------------

pub mod debugger {
    pub mod breakpoint_store {
        use super::super::*;
        use gpui::Entity;
        use std::ops::Range;
        use text::{Anchor, Point};

        #[derive(Clone, Copy, Debug)]
        pub enum BreakpointState {
            Enabled,
            Disabled,
        }

        #[derive(Clone, Debug)]
        pub struct Breakpoint {
            pub state: BreakpointState,
            pub condition: Option<String>,
            pub hit_condition: Option<String>,
            pub log_point: Option<String>,
            pub message: Option<String>,
        }

        impl Breakpoint {
            pub fn new_standard() -> Self {
                Self { state: BreakpointState::Enabled, condition: None, hit_condition: None, log_point: None, message: None }
            }

            pub fn is_enabled(&self) -> bool {
                matches!(self.state, BreakpointState::Enabled)
            }

            pub fn is_disabled(&self) -> bool {
                !self.is_enabled()
            }
        }

        #[derive(Clone, Copy, Debug)]
        pub struct BreakpointSessionState {
            pub verified: bool,
        }

        #[derive(Default)]
        pub struct BreakpointStore;

        impl BreakpointStore {
            pub fn breakpoints(
                &self,
                _buffer: &Entity<language::Buffer>,
                _range: Option<Range<Anchor>>,
                _snapshot: &language::BufferSnapshot,
                _cx: &App,
            ) -> std::vec::IntoIter<(BreakpointWithPosition, Option<BreakpointSessionState>)> {
                Vec::new().into_iter()
            }

            pub fn active_position(&self) -> Option<super::super::StackFrame> {
                None
            }

            pub fn set_active_debug_pane_id(&mut self, _pane_id: gpui::EntityId) {}

            pub fn active_debug_line_pane_id(&self) -> Option<gpui::EntityId> {
                None
            }

            pub fn set_active_debug_line_pane_id(&mut self, _pane_id: gpui::EntityId) {}

            pub fn toggle_breakpoint(
                &mut self,
                _buffer: Entity<language::Buffer>,
                _breakpoint: BreakpointWithPosition,
                _edit_action: BreakpointEditAction,
                _cx: &mut gpui::Context<Self>,
            ) {
            }
        }

        #[derive(Clone, Debug)]
        pub struct BreakpointWithPosition {
            pub bp: Breakpoint,
            pub position: text::Anchor,
        }

        #[derive(Clone, Debug)]
        pub enum BreakpointEditAction {
            Toggle,
            InvertState,
            EditLogMessage(String),
            EditHitCondition(String),
            EditCondition(String),
        }
    }

    pub mod session {
        #[derive(Default)]
        pub struct Session;

        #[derive(Clone, Debug)]
        pub enum SessionEvent {
            InvalidateInlineValue,
        }

        impl gpui::EventEmitter<SessionEvent> for Session {}

        impl Session {
            pub fn any_stopped_thread(&self) -> Option<usize> {
                None
            }
        }
    }

    pub mod dap_store {
        use super::session::Session;
        use gpui::Entity;

        #[derive(Default)]
        pub struct DapStore;

        impl DapStore {
            pub fn sessions(&self) -> std::slice::Iter<'_, Entity<Session>> {
                [].iter()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LSP command helpers
// ---------------------------------------------------------------------------

pub mod lsp_command {
    use super::LocationLink;

    pub fn location_link_from_proto(_link: &rpc::proto::LocationLink) -> Option<LocationLink> {
        None
    }
}

// ---------------------------------------------------------------------------
// LSP store
// ---------------------------------------------------------------------------

pub mod lsp_store {
    use super::*;
    use std::ops::Range;

    pub mod lsp_ext_command {
        use text::{BufferId, Point};

        #[derive(Clone, Debug)]
        pub struct SwitchSourceHeaderResult(pub String);

        #[derive(Clone, Debug)]
        pub struct SwitchSourceHeader;

        #[derive(Clone, Debug)]
        pub struct ExpandedMacro {
            pub name: String,
            pub expansion: String,
        }

        #[derive(Clone, Debug)]
        pub struct GoToParentModule {
            pub position: Point,
        }

        #[derive(Clone, Debug)]
        pub struct OpenDocs {
            pub position: Point,
        }

        #[derive(Clone, Debug)]
        pub struct DocsUrls {
            pub web: Option<String>,
            pub local: Option<String>,
        }
    }

    pub mod rust_analyzer_ext {
        pub const RUST_ANALYZER_NAME: &str = "rust-analyzer";

        pub fn run_flycheck(_cx: &mut gpui::App) {}
        pub fn clear_flycheck(_cx: &mut gpui::App) {}
        pub fn cancel_flycheck(_cx: &mut gpui::App) {}
    }

    pub mod clangd_ext {
        pub const CLANGD_SERVER_NAME: &str = "clangd";
    }

    #[derive(Clone, Debug)]
    pub struct LspDocumentLink {
        pub range: Range<Anchor>,
        pub target: String,
    }

    #[derive(Clone, Debug)]
    pub struct ResolvedDocumentLink {
        pub buffer_id: text::BufferId,
        pub link: LspDocumentLink,
    }

    #[derive(Clone, Debug)]
    pub struct BufferDocumentLinks {
        pub links: Vec<LspDocumentLink>,
    }

    #[derive(Clone, Debug, Default)]
    pub struct LspFoldingRange {
        pub start: text::Point,
        pub end: text::Point,
        pub kind: Option<lsp::FoldingRangeKind>,
    }

    #[derive(Clone, Copy, Debug)]
    pub struct TokenType(pub u32);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum FormatTrigger {
        Invocation,
        TypeChange,
        Save,
        Manual,
    }

    #[derive(Clone, Debug)]
    pub struct BufferSemanticToken {
        pub range: Range<text::Anchor>,
        pub token_type: TokenType,
        pub token_modifiers: u32,
    }

    #[derive(Clone, Debug, Default)]
    pub struct BufferSemanticTokens {
        pub tokens: Vec<BufferSemanticToken>,
    }

    #[derive(Clone, Debug)]
    pub struct CacheInlayHints;

    #[derive(Clone, Debug)]
    pub struct ResolvedHint;

    #[derive(Clone, Debug)]
    pub struct RefreshForServer {
        pub server_id: lsp::LanguageServerId,
        pub request_id: usize,
    }

    #[derive(Clone, Debug, Default)]
    pub struct SemanticTokenStylizer;

    impl SemanticTokenStylizer {
        pub fn server_id(&self) -> lsp::LanguageServerId {
            lsp::LanguageServerId(0)
        }
    }

    #[derive(Default)]
    pub struct LspStore;

    impl LspStore {
        pub fn upstream_client(&self) -> Option<(anyhow::Result<()>, u64)> {
            None
        }

        pub fn last_formatting_failure(&self) -> Option<&str> {
            None
        }

        pub fn as_local(&self) -> Option<&Self> {
            Some(self)
        }

        pub fn result_id_for_buffer_pull(
            &self,
            _server_id: lsp::LanguageServerId,
            _buffer_id: text::BufferId,
            _extra: &Option<String>,
            _cx: &mut gpui::Context<Self>,
        ) -> Option<String> {
            None
        }
    }

    /// Stub: SymbolLocation (from lsp_store crate)
    #[derive(Clone, Debug)]
    pub struct SymbolLocation {
        pub symbol: super::Symbol,
        pub path: ProjectPath,
    }

}

// ---------------------------------------------------------------------------
// Task store
// ---------------------------------------------------------------------------

pub mod task_store {
    use super::*;
    use std::ops::Range;

    #[derive(Default)]
    pub struct TaskStore;

    impl TaskStore {
        pub fn task_inventory(&self) -> Option<Entity<TaskInventory>> {
            None
        }

        pub fn task_context_for_location(
            &self,
            _variables: crate::TaskVariables,
            _location: language::Location,
            _cx: &mut gpui::Context<Self>,
        ) -> Task<anyhow::Result<Option<crate::TaskVariables>>> {
            Task::ready(Ok(None))
        }
    }

    #[derive(Default)]
    pub struct TaskInventory;
}

// ---------------------------------------------------------------------------
// Re-exported task-like types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TaskVariables {
    pub map: BTreeMap<String, String>,
}

impl Default for TaskVariables {
    fn default() -> Self {
        Self {
            map: BTreeMap::default(),
        }
    }
}

impl TaskVariables {
    pub fn insert(&mut self, key: VariableName, value: String) {
        self.map.insert(key.to_string(), value);
    }
}

#[derive(Clone, Debug)]
pub enum VariableName {
    Custom(String),
}

impl std::fmt::Display for VariableName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VariableName::Custom(s) => f.write_str(s),
        }
    }
}

// ---------------------------------------------------------------------------
// Types referenced by the Project method stubs below
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct OpenLspBufferHandle;

#[derive(Clone, Debug)]
pub enum PrepareRenameResponse {
    Ready(std::ops::Range<text::Anchor>, bool),
    Success(Range<text::Anchor>),
    OnlyUnpreparedRenameSupported,
    InvalidPosition,
}

#[derive(Default)]
pub struct DapStore;

impl DapStore {
    pub fn sessions(&self) -> std::slice::Iter<'_, gpui::Entity<debugger::session::Session>> {
        [].iter()
    }
}

#[derive(Default)]
pub struct Client;

#[derive(Default)]
pub struct Telemetry;

impl Client {
    pub fn telemetry(&self) -> Arc<Telemetry> {
        Arc::new(Telemetry)
    }

    /// Stub: read (client crate 已删除)
    pub fn read(&self, _cx: &gpui::App) -> &Self {
        self
    }

    /// Stub: shell (client crate 已删除)
    pub fn shell(&self) -> Option<Arc<ShellConfig>> {
        None
    }

    /// Stub: is_disconnected
    pub fn is_disconnected(&self) -> bool {
        true
    }
}

impl Telemetry {
    pub fn log_edit_event(&self, _name: &str, _is_via_ssh: bool) {}
}

// ---------------------------------------------------------------------------
// Project method stubs for APIs removed during dependency stripping
// ---------------------------------------------------------------------------

use crate::{
    bookmark_store::BookmarkStore,
    debugger::breakpoint_store::BreakpointStore,
    Location, Project, ProjectPath, ProjectTransaction, Worktree, WorktreeId,
};
use git::blame::Blame;
use lsp::LanguageServerId;
use util::rel_path::RelPath;

#[derive(Clone, Debug)]
pub struct StackFrame { pub position: text::Point }

/// Stub for deleted remote::RemoteConnectionOptions (spec §8.2 M2)
#[derive(Clone, Debug)]
pub struct RemoteConnectionOptionsStub;

/// Stub for deleted git::Repository (spec §8.2 M2)
#[derive(Clone, Debug)]
pub struct Repository {
    pub work_directory_abs_path: std::path::PathBuf,
    pub id: u64,
    pub branch: Option<String>,
}

impl Repository {
    /// Stub: entity_id
    pub fn entity_id(&self) -> gpui::EntityId {
        gpui::EntityId::from(0)
    }

    /// Stub: read
    pub fn read(&self, _cx: &gpui::App) -> &Self {
        self
    }

    /// Stub: update — generic over context type for async callers
    pub fn update<C, F, R>(&self, _cx: &mut C, f: F) -> R
    where
        F: FnOnce(&mut Self, &mut C) -> R,
    {
        // Stub: cannot mutate through Arc, call with dummy
        let mut dummy = self.clone();
        f(&mut dummy, _cx)
    }

    /// Stub: remove_worktree
    pub fn remove_worktree(
        &mut self,
        _path: std::path::PathBuf,
        _force: bool,
    ) -> gpui::Task<anyhow::Result<()>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub")))
    }

    /// Stub: default_branch
    pub fn default_branch(
        &mut self,
        _include_remote_name: bool,
    ) -> gpui::Task<anyhow::Result<String>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub")))
    }

    /// Stub: worktrees
    pub fn worktrees(&mut self) -> gpui::Task<anyhow::Result<Vec<git::repository::Worktree>>> {
        gpui::Task::ready(Ok(Vec::new()))
    }

    /// Stub: status_for_path
    pub fn status_for_path(&self, _path: &git::repository::RepoPath) -> Option<crate::git_store::StatusEntry> {
        None
    }

    /// Stub: project_path_to_repo_path
    pub fn project_path_to_repo_path(&self, _path: &ProjectPath, _cx: &gpui::App) -> Option<git::repository::RepoPath> {
        None
    }

    /// Stub: barrier
    pub fn barrier(&mut self) -> futures::channel::oneshot::Receiver<()> {
        let (tx, rx) = futures::channel::oneshot::channel();
        tx.send(()).ok();
        rx
    }
}

impl Project {
    pub fn open_buffer_by_id(
        &mut self,
        id: text::BufferId,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Entity<language::Buffer>>> {
        Task::ready(self.buffer_store.read(cx).get_existing(id))
    }

    pub fn open_local_buffer(
        &mut self,
        path: &std::path::Path,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Entity<language::Buffer>>> {
        let Some(project_path) = self
            .worktree_store
            .read(cx)
            .project_path_for_absolute_path(path, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!(
                "path is not in a project worktree: {}",
                path.display()
            )));
        };
        self.buffer_store
            .update(cx, |store, cx| store.open_buffer(project_path, cx))
    }

    pub fn open_path(
        &mut self,
        path: ProjectPath,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<(Entity<worktree::Worktree>, Entity<language::Buffer>)>> {
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!(
                "unknown worktree {}",
                path.worktree_id
            )));
        };
        let buffer_task = self
            .buffer_store
            .update(cx, |store, cx| store.open_buffer(path, cx));
        cx.spawn(async move |_, _| Ok((worktree, buffer_task.await?)))
    }

    pub fn open_local_buffer_via_lsp(
        &mut self,
        _uri: lsp::Uri,
        _server_id: LanguageServerId,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Entity<language::Buffer>>> {
        Task::ready(Err(anyhow::anyhow!("stub: open_local_buffer_via_lsp")))
    }

    pub fn find_project_path(
        &self,
        full_path: &std::path::Path,
        cx: &gpui::App,
    ) -> Option<ProjectPath> {
        self.worktree_store
            .read(cx)
            .project_path_for_absolute_path(full_path, cx)
    }

    pub fn find_worktree(
        &mut self,
        abs_path: &std::path::Path,
        cx: &mut gpui::Context<Self>,
    ) -> Option<(Entity<Worktree>, Arc<RelPath>)> {
        self.worktree_store.read(cx).find_worktree(abs_path, cx)
    }

    /// 存根: 解析绝对文件路径 (来源: spec §8.2 M3)
    pub fn resolve_abs_file_path(
        &mut self,
        _abs_path: &str,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<Option<ProjectPath>> {
        Task::ready(None)
    }

    pub fn save_buffers(
        &mut self,
        buffers: collections::HashSet<Entity<language::Buffer>>,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let tasks: Vec<_> = buffers
            .into_iter()
            .map(|buffer| {
                self.buffer_store
                    .update(cx, |store, cx| store.save_buffer(buffer, cx))
            })
            .collect();
        cx.spawn(async move |_, _| {
            for task in tasks {
                task.await?;
            }
            Ok(())
        })
    }

    pub fn save_buffer_as(
        &mut self,
        buffer: Entity<language::Buffer>,
        path: ProjectPath,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.buffer_store
            .update(cx, |store, cx| store.save_buffer_as(buffer, path, cx))
    }

    pub fn reload_buffers(
        &mut self,
        _buffers: collections::HashSet<Entity<language::Buffer>>,
        _reload: bool,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<ProjectTransaction>> {
        Task::ready(Ok(ProjectTransaction::default()))
    }

    pub fn blame_buffer(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _version: Option<git::Oid>,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Blame>> {
        Task::ready(Err(anyhow::anyhow!("stub: blame_buffer")))
    }

    pub fn references(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<Location>>>> {
        Task::ready(Ok(None))
    }

    pub fn hover(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<super::Hover>>>> {
        Task::ready(Ok(None))
    }

    pub fn document_highlights(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Vec<DocumentHighlight>>> {
        Task::ready(Ok(Vec::new()))
    }


    pub fn definitions(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _kind: GotoDefinitionKind,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        Task::ready(Ok(None))
    }

    pub fn declarations(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        Task::ready(Ok(None))
    }

    pub fn type_definitions(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        Task::ready(Ok(None))
    }

    pub fn implementations(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        Task::ready(Ok(None))
    }

    pub fn prepare_rename(
        &mut self,
        _buffer: Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<PrepareRenameResponse>> {
        Task::ready(Ok(PrepareRenameResponse::InvalidPosition))
    }

    pub fn apply_code_action_kind(
        &mut self,
        _buffers: collections::HashSet<Entity<language::Buffer>>,
        _kind: lsp::CodeActionKind,
        _only: bool,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Ok(()))
    }

    pub fn supports_range_formatting(
        &self,
        _buffer: &Entity<language::Buffer>,
        _cx: &gpui::App,
    ) -> bool {
        false
    }

    pub fn restart_language_servers_for_buffers(
        &mut self,
        _buffers: collections::HashSet<Entity<language::Buffer>>,
        _server_ids: collections::HashSet<LanguageServerId>,
        _restart: bool,
        _cx: &mut gpui::Context<Self>,
    ) {
    }

    pub fn stop_language_servers_for_buffers(
        &mut self,
        _buffers: collections::HashSet<Entity<language::Buffer>>,
        _server_ids: collections::HashSet<LanguageServerId>,
        _cx: &mut gpui::Context<Self>,
    ) {
    }

    pub fn cancel_language_server_work_for_buffers(
        &mut self,
        _buffers: collections::HashSet<Entity<language::Buffer>>,
        _cx: &mut gpui::Context<Self>,
    ) {
    }

    pub fn reveal_path(&mut self, _path: &std::path::Path, _cx: &mut gpui::Context<Self>) {}

    pub fn register_buffer_with_language_servers(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _cx: &mut gpui::Context<Self>,
    ) -> OpenLspBufferHandle {
        OpenLspBufferHandle
    }

    pub fn client(&self) -> &Client {
        static CLIENT: std::sync::LazyLock<Client> = std::sync::LazyLock::new(Client::default);
        &CLIENT
    }

    pub fn task_store(&self) -> Entity<crate::task_store::TaskStore> {
        unimplemented!("stub: task_store")
    }

    pub fn dap_store(&self) -> Entity<DapStore> {
        unimplemented!("stub: dap_store")
    }

    pub fn bookmark_store(&self) -> Entity<BookmarkStore> {
        unimplemented!("stub: bookmark_store")
    }

    pub fn breakpoint_store(&self) -> Entity<BreakpointStore> {
        unimplemented!("stub: breakpoint_store")
    }

    pub fn active_debug_session(
        &self,
        _cx: &gpui::App,
    ) -> Option<(Entity<crate::debugger::session::Session>, StackFrame)> {
        None
    }

    pub fn any_language_server_supports_inlay_hints(
        &mut self,
        _buffer: &language::Buffer,
    ) -> bool {
        false
    }

    pub fn any_language_server_supports_semantic_tokens(
        &mut self,
        _buffer: &language::Buffer,
    ) -> bool {
        false
    }

    pub fn inline_values(
        &mut self,
        _session: Entity<crate::debugger::session::Session>,
        _stack_frame: StackFrame,
        _buffer_handle: Entity<language::Buffer>,
        _range: Range<text::Anchor>,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Vec<InlayHint>>> {
        Task::ready(Ok(Vec::new()))
    }

    pub fn visible_worktrees(&self, cx: &gpui::App) -> impl Iterator<Item = Entity<Worktree>> {
        self.worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .collect::<Vec<_>>()
            .into_iter()
    }

    // --- Stub methods for deleted diagnostic/remote features (spec §8.2 M2) ---

    pub fn diagnostic_summary(&self, _warnings: bool, _cx: &App) -> DiagnosticSummary {
        DiagnosticSummary::default()
    }

    pub fn diagnostic_summary_for_path(&self, _path: &ProjectPath, _cx: &App) -> DiagnosticSummary {
        DiagnosticSummary::default()
    }

    pub fn diagnostic_summaries(
        &mut self,
        _only_local: bool,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<BTreeMap<WorktreeId, DiagnosticSummary>>> {
        Task::ready(Ok(BTreeMap::new()))
    }

    pub fn capability(&self) -> language::Capability {
        language::Capability::ReadWrite
    }

    pub fn is_local(&self) -> bool {
        true
    }

    pub fn remote_client(&self) -> Option<Arc<Client>> {
        None
    }

    pub fn remote_connection_options(&self) -> Option<remote::RemoteConnectionOptions> {
        None
    }

    pub fn language_servers_running_disk_based_diagnostics(
        &self,
        _cx: &App,
    ) -> Vec<lsp::LanguageServerId> {
        Vec::new()
    }

    pub fn remove_worktree(
        &mut self,
        _worktree_id: WorktreeId,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Ok(()))
    }

    pub fn repositories(&self, cx: &App) -> Vec<Entity<crate::git_store::Repository>> {
        self.git_store
            .read(cx)
            .repositories()
            .values()
            .cloned()
            .collect()
    }

    pub fn active_repository(&self, cx: &App) -> Option<Entity<crate::git_store::Repository>> {
        self.git_store.read(cx).active_repository()
    }

    pub fn save_buffer(
        &mut self,
        buffer: Entity<language::Buffer>,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.buffer_store
            .update(cx, |store, cx| store.save_buffer(buffer, cx))
    }

    pub fn get_open_buffer(
        &self,
        file: &ProjectPath,
        cx: &App,
    ) -> Option<Entity<language::Buffer>> {
        self.buffer_store.read(cx).get_by_path(file)
    }

    pub fn create_buffer(
        &mut self,
        language: Option<Arc<language::Language>>,
        project_searchable: bool,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<Entity<language::Buffer>>> {
        self.buffer_store
            .update(cx, |store, cx| store.create_buffer(language, project_searchable, cx))
    }

    /// 返回搜索历史可变引用
    pub fn search_history_mut(
        &mut self,
        kind: crate::search::SearchInputKind,
    ) -> &mut crate::search_history::SearchHistory {
        match kind {
            crate::search::SearchInputKind::Query => &mut self.search_history,
            crate::search::SearchInputKind::Include => &mut self.search_included_history,
            crate::search::SearchInputKind::Exclude => &mut self.search_excluded_history,
        }
    }

    /// 返回搜索历史引用
    pub fn search_history(
        &self,
        kind: crate::search::SearchInputKind,
    ) -> &crate::search_history::SearchHistory {
        match kind {
            crate::search::SearchInputKind::Query => &self.search_history,
            crate::search::SearchInputKind::Include => &self.search_included_history,
            crate::search::SearchInputKind::Exclude => &self.search_excluded_history,
        }
    }

    /// 执行项目搜索
    pub fn search(
        &mut self,
        _query: crate::search::SearchQuery,
        _cx: &mut gpui::Context<Self>,
    ) -> SearchResults<crate::search::SearchResult> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        SearchResults { tx, rx }
    }

    /// 是否支持终端
    pub fn supports_terminal(&self, _cx: &App) -> bool {
        true
    }

    /// 当前活动项目目录
    pub fn active_project_directory(
        &self,
        _cx: &App,
    ) -> Option<std::path::PathBuf> {
        None
    }

    /// 当前活动项目目录 (const ref)
    pub fn active_entry_directory(
        &self,
        _cx: &App,
    ) -> Option<std::path::PathBuf> {
        None
    }

    /// 是否远程项目
    pub fn is_remote(&self) -> bool {
        false
    }

    /// 根据 entry_id 获取 ProjectPath
    pub fn path_for_entry(
        &self,
        _entry_id: ProjectEntryId,
        _cx: &App,
    ) -> Option<crate::ProjectPath> {
        None
    }

    /// 根据 entry_id 获取 worktree_id
    pub fn worktree_id_for_entry(
        &self,
        _entry_id: ProjectEntryId,
        _cx: &App,
    ) -> Option<worktree::WorktreeId> {
        None
    }

    /// 判断 entry 是否是 worktree root
    pub fn entry_is_worktree_root(
        &self,
        _entry_id: ProjectEntryId,
        _cx: &App,
    ) -> bool {
        false
    }

    /// 创建 entry (stub)
    pub fn create_entry(
        &mut self,
        _path: crate::ProjectPath,
        _is_dir: bool,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<worktree::CreatedEntry>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub: create_entry")))
    }

    /// 重命名 entry (stub)
    pub fn rename_entry(
        &mut self,
        _entry_id: ProjectEntryId,
        _new_path: crate::ProjectPath,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<worktree::CreatedEntry>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub: rename_entry")))
    }

    /// 删除 entry (stub)
    pub fn delete_entry(
        &mut self,
        _entry_id: ProjectEntryId,
        _trash: bool,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<Option<fs::TrashedEntry>>> {
        gpui::Task::ready(Ok(None))
    }

    /// 恢复 entry (stub)
    pub fn restore_entry(
        &mut self,
        _worktree_id: worktree::WorktreeId,
        _trashed_entry: fs::TrashedEntry,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<crate::ProjectPath>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub: restore_entry")))
    }

    /// 复制 entry (stub)
    pub fn copy_entry(
        &mut self,
        _entry_id: ProjectEntryId,
        _path: crate::ProjectPath,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<worktree::CreatedEntry>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub: copy_entry")))
    }

    /// 展开 entry (stub)
    pub fn expand_entry(
        &mut self,
        _worktree_id: worktree::WorktreeId,
        _entry_id: ProjectEntryId,
        _cx: &mut gpui::Context<Self>,
    ) {
    }

    /// 展开所有子目录 (stub)
    pub fn expand_all_for_entry(
        &mut self,
        _worktree_id: worktree::WorktreeId,
        _entry_id: ProjectEntryId,
        _cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Task<anyhow::Result<()>>> {
        None
    }

    /// 获取 buffer (stub)
    pub fn buffer_for_id(
        &mut self,
        buffer_id: text::BufferId,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Entity<language::Buffer>> {
        self.buffer_store.read(cx).get(buffer_id)
    }

    /// 获取脏 buffers (stub)
    pub fn dirty_buffers(
        &self,
        _cx: &App,
    ) -> impl Iterator<Item = crate::ProjectPath> {
        std::iter::empty()
    }

    /// WSL 互操作性检查 (stub)
    pub fn is_via_wsl_with_host_interop(&self, _cx: &App) -> bool {
        false
    }

    /// 下载文件 (stub)
    pub fn download_file(
        &mut self,
        _worktree_id: worktree::WorktreeId,
        _entry_path: crate::ProjectPath,
        _destination_path: PathBuf,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub: download_file")))
    }

    /// 移动 worktree (stub)
    pub fn move_worktree(
        &mut self,
        _worktree_id: worktree::WorktreeId,
        _destination_id: worktree::WorktreeId,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        gpui::Task::ready(Err(anyhow::anyhow!("stub: move_worktree")))
    }

    /// 获取符号列表
    pub fn symbols(
        &mut self,
        _query: &str,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<Vec<crate::lsp_store::SymbolLocation>>> {
        gpui::Task::ready(Ok(Vec::new()))
    }
}

/// Stub: FileFinderSettings (open_path_prompt 模块已删除)
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileFinderSettings {
    pub file_icons: bool,
}

impl settings::SettingsKey for FileFinderSettings {
    const KEY: Option<&'static str> = None;
}

impl settings::Settings for FileFinderSettings {
    fn from_settings(_content: &settings::SettingsContent) -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Extension stubs (spec §8.2 M2)
// ---------------------------------------------------------------------------

/// Stub: ExtensionMetadata (cloud_api_types crate 已删除)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionMetadata {
    pub id: Arc<str>,
    pub dev: bool,
    pub manifest: ExtensionMetadataManifest,
    pub published_at: Option<String>,
    pub download_count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionMetadataManifest {
    pub version: Arc<str>,
    pub schema_version: Option<i32>,
    pub wasm_api_version: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub repository: Option<String>,
    pub authors: Vec<String>,
    pub provides_list: Vec<ExtensionProvides>,
}

impl ExtensionMetadataManifest {
    pub fn provides(&self) -> &Vec<ExtensionProvides> {
        &self.provides_list
    }
}

/// Stub: VimModeSetting (vim_mode_setting crate 已删除)
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct VimModeSetting(pub bool);

impl settings::SettingsKey for VimModeSetting {
    const KEY: Option<&'static str> = None;
}

impl settings::Settings for VimModeSetting {
    fn from_settings(_content: &settings::SettingsContent) -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Terminal / task stubs (spec §8.2 M2)
// ---------------------------------------------------------------------------

/// Stub: TaskId (task crate 已删除)
pub type TaskId = u64;

/// Stub: RevealStrategy (open_path_prompt crate 已删除)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevealStrategy {
    #[default]
    Center,
    Top,
    Always,
    NoFocus,
    Never,
}

/// Stub: RevealTarget (open_path_prompt crate 已删除)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevealTarget {
    #[default]
    Center,
    Dock,
}

/// Stub: Shell (task crate 已删除)
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Shell {
    #[default]
    System,
    Program(Arc<ShellConfig>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellConfig {
    pub program: String,
    pub args: Vec<String>,
}

/// Stub: ShellBuilder (task crate 已删除)
#[derive(Debug, Clone)]
pub struct ShellBuilder {
    pub program: String,
    pub args: Vec<String>,
}

impl ShellBuilder {
    pub fn new(shell: &Shell, _is_windows: bool) -> Self {
        let (program, args) = match shell {
            Shell::System => (util::get_system_shell(), Vec::new()),
            Shell::Program(config) => (config.program.clone(), config.args.clone()),
        };
        Self { program, args }
    }

    /// 生成命令标签字符串
    pub fn command_label(&self, command: &str) -> String {
        if command.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, command)
        }
    }

    /// 构建命令和参数 (no shell quoting)
    pub fn build_no_quote(&self, command: Option<String>, _args: &[String]) -> (String, Vec<String>) {
        let mut all_args = self.args.clone();
        if let Some(cmd) = command {
            if !cmd.is_empty() {
                all_args.push("-c".to_string());
                all_args.push(cmd);
                (self.program.clone(), all_args)
            } else {
                (self.program.clone(), all_args)
            }
        } else {
            (self.program.clone(), all_args)
        }
    }
}

/// Stub: SpawnInTerminal (task crate 已删除)
#[derive(Debug, Clone, Default)]
pub struct SpawnInTerminal {
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: Option<ProjectPath>,
    pub shell: Shell,
    pub allow_concurrent_runs: bool,
    pub use_new_terminal: bool,
    pub full_label: String,
    pub id: u64,
    pub reveal: RevealStrategy,
    pub reveal_target: RevealTarget,
    pub command: String,
    pub label: String,
    pub command_label: String,
    pub show_summary: bool,
    pub show_command: bool,
    pub show_rerun: bool,
    pub env: std::collections::HashMap<String, String>,
    pub cwd: Option<std::path::PathBuf>,
}

/// Stub: Breadcrumbs (breadcrumbs crate 已删除)
#[derive(Debug, Clone)]
pub struct Breadcrumbs {}

impl Breadcrumbs {
    pub fn new() -> Self {
        Self {}
    }
}

/// Stub: path_suffix (from project crate, 已删除)
pub fn path_suffix(path: &std::path::Path, detail: usize) -> String {
    let _ = detail;
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Stub: TerminalDockPosition re-export from settings
pub use settings::TerminalDockPosition;

/// Stub: SearchResults (task crate 已删除)
pub struct SearchResults<T> {
    pub tx: futures::channel::mpsc::UnboundedSender<T>,
    pub rx: futures::channel::mpsc::UnboundedReceiver<T>,
}

impl<T> Clone for SearchResults<T> {
    fn clone(&self) -> Self {
        let (tx2, rx2) = futures::channel::mpsc::unbounded();
        SearchResults {
            tx: self.tx.clone(),
            rx: rx2,
        }
    }
}

/// Stub: Search alias for SearchQuery (task crate 已删除)
pub type Search = crate::search::SearchQuery;
