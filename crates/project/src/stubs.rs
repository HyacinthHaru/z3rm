//! Stub types for project-crate APIs removed during dependency stripping.
//! These keep downstream crates (editor, picker, platform_title_bar) compiling.

use std::{ops::Range, path::PathBuf, sync::Arc};

use collections::BTreeMap;
use fs::Fs;
use gpui::{App, Entity, Task};
use serde::{Deserialize, Serialize};
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
// Bookmark store lives in the project crate's retained bookmark_store module.
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
                Self {
                    state: BreakpointState::Enabled,
                    condition: None,
                    hit_condition: None,
                    log_point: None,
                    message: None,
                }
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
            ) -> std::vec::IntoIter<(BreakpointWithPosition, Option<BreakpointSessionState>)>
            {
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

        /// No LSP store exists; no inlay chunks can be applicable.
        pub fn applicable_inlay_chunks(
            &self,
            _buffer: &Entity<language::Buffer>,
            _ranges: &[Range<text::Anchor>],
            _cx: &mut gpui::Context<Self>,
        ) -> Vec<Range<language::BufferRow>> {
            Vec::new()
        }

        /// No LSP store exists; nothing to invalidate.
        pub fn invalidate_inlay_hints(
            &self,
            _for_buffers: &collections::HashSet<text::BufferId>,
            _cx: &mut gpui::Context<Self>,
        ) {
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


// ---------------------------------------------------------------------------
// Project method stubs for APIs removed during dependency stripping
// ---------------------------------------------------------------------------

use crate::{
    Location, Project, ProjectItem, ProjectPath, ProjectTransaction, Worktree, WorktreeId,
    bookmark_store::BookmarkStore, debugger::breakpoint_store::BreakpointStore,
};
use git::blame::Blame;
use lsp::LanguageServerId;
use util::rel_path::RelPath;

#[derive(Clone, Debug)]
pub struct StackFrame {
    pub position: text::Point,
}


/// LSP integration was deleted during dependency stripping. Queries that would
/// previously have consulted a language server must fail explicitly instead of
/// fabricating "no results" (which would silently claim, e.g., that a symbol
/// has no definitions or references). Callers surface the error through their
/// normal Task/Result plumbing; local buffer behavior is unaffected.
fn lsp_unavailable<T>() -> gpui::Task<anyhow::Result<T>> {
    gpui::Task::ready(Err(anyhow::anyhow!(
        "language server support is unavailable in this build"
    )))
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
        if let Some(project_path) = self
            .worktree_store
            .read(cx)
            .project_path_for_absolute_path(path, cx)
        {
            return self
                .buffer_store
                .update(cx, |store, cx| store.open_buffer(project_path, cx));
        }

        // A file outside every worktree gets an invisible worktree of its own,
        // which is how a standalone file is reopened when its editor is restored.
        let entry = self.worktree_store.update(cx, |worktree_store, cx| {
            worktree_store.find_or_create_worktree(path, false, cx)
        });
        let buffer_store = self.buffer_store.clone();
        cx.spawn(async move |_, cx| {
            let (worktree, path) = entry.await?;
            let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
            buffer_store
                .update(cx, |store, cx| {
                    store.open_buffer(ProjectPath { worktree_id, path }, cx)
                })
                .await
        })
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
        uri: lsp::Uri,
        _server_id: LanguageServerId,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Entity<language::Buffer>>> {
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                return Task::ready(Err(anyhow::anyhow!(
                    "LSP location is not a local file URI: {uri}"
                )));
            }
        };
        self.open_local_buffer(&path, cx)
    }

    /// Resolves either an absolute path or a path prefixed with a visible
    /// worktree's root name (the form shown in the UI, e.g. `project_root/dir/file`).
    pub fn find_project_path(
        &self,
        full_path: &std::path::Path,
        cx: &gpui::App,
    ) -> Option<ProjectPath> {
        let worktree_store = self.worktree_store.read(cx);
        let path_style = worktree_store.path_style();
        for worktree in worktree_store.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let root_name = worktree.root_name();
            if let Some(relative_path) = path_style.strip_prefix(full_path, root_name.as_std_path())
            {
                return Some(ProjectPath {
                    worktree_id: worktree.id(),
                    path: relative_path.into_arc(),
                });
            }
        }
        worktree_store.project_path_for_absolute_path(full_path, cx)
    }

    pub fn find_worktree(
        &mut self,
        abs_path: &std::path::Path,
        cx: &mut gpui::Context<Self>,
    ) -> Option<(Entity<Worktree>, Arc<RelPath>)> {
        self.worktree_store.read(cx).find_worktree(abs_path, cx)
    }

    pub fn resolve_abs_file_path(
        &mut self,
        abs_path: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Option<ProjectPath>> {
        let abs_path = std::path::PathBuf::from(abs_path);
        if !abs_path.is_absolute() {
            return Task::ready(None);
        }
        let project_path = self.find_project_path(&abs_path, cx);
        let fs = self.fs.clone();
        gpui::AppContext::background_spawn(cx, async move {
            if fs.is_file(&abs_path).await {
                project_path
            } else {
                None
            }
        })
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
        buffers: collections::HashSet<Entity<language::Buffer>>,
        reload: bool,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<ProjectTransaction>> {
        self.buffer_store
            .update(cx, |store, cx| store.reload_buffers(buffers, reload, cx))
    }

    pub fn blame_buffer(
        &mut self,
        buffer: &Entity<language::Buffer>,
        version: Option<clock::Global>,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Blame>>> {
        self.git_store
            .update(cx, |git_store, cx| git_store.blame_buffer(buffer, version, cx))
    }

    pub fn references(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<Location>>>> {
        lsp_unavailable()
    }

    pub fn hover(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<super::Hover>>>> {
        lsp_unavailable()
    }

    pub fn document_highlights(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Vec<DocumentHighlight>>> {
        lsp_unavailable()
    }

    pub fn definitions(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _kind: GotoDefinitionKind,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        lsp_unavailable()
    }

    pub fn declarations(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        lsp_unavailable()
    }

    pub fn type_definitions(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        lsp_unavailable()
    }

    pub fn implementations(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<Option<Vec<LocationLink>>>> {
        lsp_unavailable()
    }

    pub fn prepare_rename(
        &mut self,
        _buffer: Entity<language::Buffer>,
        _position: text::Anchor,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<PrepareRenameResponse>> {
        lsp_unavailable()
    }

    pub fn apply_code_action_kind(
        &mut self,
        _buffers: collections::HashSet<Entity<language::Buffer>>,
        _kind: lsp::CodeActionKind,
        _only: bool,
        _cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        lsp_unavailable()
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

    pub fn reveal_path(&mut self, path: &std::path::Path, cx: &mut gpui::Context<Self>) {
        cx.reveal_path(path);
    }

    pub fn register_buffer_with_language_servers(
        &mut self,
        _buffer: &Entity<language::Buffer>,
        _cx: &mut gpui::Context<Self>,
    ) -> OpenLspBufferHandle {
        OpenLspBufferHandle
    }


    pub fn task_store(&self) -> Entity<crate::task_store::TaskStore> {
        self.task_store_entity.clone()
    }

    pub fn dap_store(&self) -> Entity<DapStore> {
        self.dap_store_entity.clone()
    }

    pub fn bookmark_store(&self) -> Entity<BookmarkStore> {
        self.bookmark_store_entity.clone()
    }

    pub fn breakpoint_store(&self) -> Entity<BreakpointStore> {
        self.breakpoint_store_entity.clone()
    }

    pub fn lsp_store(&self) -> Entity<crate::stubs::lsp_store::LspStore> {
        self.lsp_store_entity.clone()
    }

    pub fn active_debug_session(
        &self,
        _cx: &gpui::App,
    ) -> Option<(Entity<crate::debugger::session::Session>, StackFrame)> {
        None
    }

    pub fn any_language_server_supports_inlay_hints(&mut self, _buffer: &language::Buffer) -> bool {
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
        lsp_unavailable()
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
        worktree_id: WorktreeId,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.worktree_store.update(cx, |store, cx| {
            store.remove_worktree(worktree_id, cx);
            Task::ready(Ok(()))
        })
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
        self.buffer_store.update(cx, |store, cx| {
            store.create_buffer(language, project_searchable, cx)
        })
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

    pub fn search(
        &mut self,
        query: crate::search::SearchQuery,
        cx: &mut gpui::Context<Self>,
    ) -> SearchResults<crate::search::SearchResult> {
        const MAX_SEARCH_RESULT_RANGES: usize = 100_000;

        let (tx, rx) = async_channel::unbounded();
        let search_tx = tx.clone();
        let worktree_store = self.worktree_store.clone();
        let buffer_store = self.buffer_store.clone();
        let scan_completed = worktree_store.read(cx).initial_scan_completed();
        let wait_for_scan = worktree_store.read(cx).wait_for_initial_scan();
        let worktrees = worktree_store
            .read(cx)
            .visible_worktrees_and_single_files(cx)
            .collect::<Vec<_>>();
        let opened_buffers = query.buffers().cloned();

        cx.spawn(async move |_, cx| {
            let send = |result: crate::search::SearchResult| async {
                search_tx.send(result).await.is_ok()
            };

            if !scan_completed {
                if !send(crate::search::SearchResult::WaitingForScan).await {
                    return;
                }
                wait_for_scan.await;
            }
            if !send(crate::search::SearchResult::Searching).await {
                return;
            }

            let mut candidates = Vec::new();
            if let Some(opened_buffers) = opened_buffers {
                candidates.extend(
                    opened_buffers
                        .into_iter()
                        .filter_map(|buffer| {
                            let path = buffer.read_with(cx, |buffer, cx| {
                                buffer.project_path(cx)
                            })?;
                            query.match_path(&path.path).then_some((Some(buffer), path))
                        }),
                );
            } else {
                for worktree in worktrees {
                    let (worktree_id, snapshot) = worktree.read_with(cx, |worktree, _| {
                        (worktree.id(), worktree.snapshot())
                    });
                    candidates.extend(snapshot.files(query.include_ignored(), 0).filter_map(
                        |entry| {
                            if query.match_path(&entry.path) {
                                Some(ProjectPath {
                                    worktree_id,
                                    path: entry.path.clone(),
                                })
                            } else {
                                None
                            }
                        },
                    ).map(|path| (None, path)));
                }
            }

            let mut total_ranges = 0;
            for (opened_buffer, project_path) in candidates {
                if total_ranges >= MAX_SEARCH_RESULT_RANGES {
                    break;
                }

                let buffer = match opened_buffer {
                    Some(buffer) => buffer,
                    None => {
                        match buffer_store
                            .update(cx, |store, cx| {
                                store.open_buffer(project_path.clone(), cx)
                            })
                            .await
                        {
                            Ok(buffer) => buffer,
                            Err(error) => {
                                tracing::warn!(
                                    path = ?project_path.path,
                                    error = %error,
                                    "failed to load project search buffer"
                                );
                                continue;
                            }
                        }
                    }
                };

                let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
                let offsets = query.search(&snapshot, None).await;
                if offsets.is_empty() {
                    continue;
                }

                let remaining = MAX_SEARCH_RESULT_RANGES - total_ranges;
                let ranges = offsets
                    .into_iter()
                    .take(remaining)
                    .map(|range| {
                        snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end)
                    })
                    .collect::<Vec<_>>();
                total_ranges += ranges.len();

                if !send(crate::search::SearchResult::Buffer { buffer, ranges }).await {
                    return;
                }

                if total_ranges >= MAX_SEARCH_RESULT_RANGES {
                    if !send(crate::search::SearchResult::LimitReached).await {
                        return;
                    }
                    break;
                }
            }

            ()
        })
        .detach();

        SearchResults { tx, rx }
    }

    /// Whether this local project can create terminal sessions.
    pub fn supports_terminal(&self, _cx: &App) -> bool {
        true
    }

    /// Return the worktree containing the active entry.
    pub fn active_project_directory(&self, cx: &App) -> Option<std::path::PathBuf> {
        self.active_entry
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .and_then(|worktree| {
                let worktree = worktree.read(cx);
                (!worktree.is_single_file()).then(|| worktree.abs_path().to_path_buf())
            })
            .or_else(|| {
                self.worktree_store
                    .read(cx)
                    .visible_worktrees(cx)
                    .find_map(|worktree| {
                        let worktree = worktree.read(cx);
                        (!worktree.is_single_file()).then(|| worktree.abs_path().to_path_buf())
                    })
            })
    }

    /// Return the directory containing the active entry.
    pub fn active_entry_directory(&self, cx: &App) -> Option<std::path::PathBuf> {
        let entry_id = self.active_entry?;
        let worktree = self.worktree_for_entry(entry_id, cx)?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_id(entry_id)?;
        let path = worktree.absolutize(&entry.path);
        if entry.is_dir() {
            Some(path)
        } else {
            path.parent().map(std::path::Path::to_path_buf)
        }
    }

    pub fn is_remote(&self) -> bool {
        false
    }


    /// Resolve a `ProjectPath` for an entry by locating its owning worktree.
    pub fn path_for_entry(&self, entry_id: ProjectEntryId, cx: &App) -> Option<crate::ProjectPath> {
        self.worktree_store
            .read(cx)
            .worktree_and_entry_for_id(entry_id, cx)
            .map(|(worktree, entry)| crate::ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: entry.path.clone(),
            })
    }

    /// Resolve the owning worktree id for an entry.
    pub fn worktree_id_for_entry(
        &self,
        entry_id: ProjectEntryId,
        cx: &App,
    ) -> Option<worktree::WorktreeId> {
        self.worktree_store
            .read(cx)
            .worktree_for_entry(entry_id, cx)
            .map(|worktree| worktree.read(cx).id())
    }

    /// Whether the given entry is the root of its worktree.
    pub fn entry_is_worktree_root(&self, entry_id: ProjectEntryId, cx: &App) -> bool {
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_entry(entry_id, cx)
        else {
            return false;
        };
        worktree
            .read(cx)
            .root_entry()
            .is_some_and(|root| root.id == entry_id)
    }

    /// Create a file or directory entry in its worktree.
    pub fn create_entry(
        &mut self,
        path: crate::ProjectPath,
        is_dir: bool,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<worktree::CreatedEntry>> {
        let Some(worktree) = self.worktree_for_id(path.worktree_id, cx) else {
            return gpui::Task::ready(Err(anyhow::anyhow!(
                "worktree {} not found",
                path.worktree_id
            )));
        };
        worktree.update(cx, |worktree, cx| {
            worktree.create_entry(path.path, is_dir, None, cx)
        })
    }

    /// Rename an entry, supporting cross-worktree moves via `WorktreeStore`.
    pub fn rename_entry(
        &mut self,
        entry_id: ProjectEntryId,
        new_path: crate::ProjectPath,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<worktree::CreatedEntry>> {
        self.worktree_store
            .update(cx, |store, cx| store.rename_entry(entry_id, new_path, cx))
    }

    /// Delete an entry, returning the trashed entry when trashing.
    ///
    /// A missing worktree or entry yields `Err`, never a false success.
    pub fn delete_entry(
        &mut self,
        entry_id: ProjectEntryId,
        trash: bool,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<Option<fs::TrashedEntry>>> {
        let Some(worktree) = self.worktree_for_entry(entry_id, cx) else {
            return gpui::Task::ready(Err(anyhow::anyhow!("no worktree for entry {:?}", entry_id)));
        };
        let task = worktree.update(cx, |worktree, cx| {
            worktree.delete_entry(entry_id, trash, cx)
        });
        match task {
            Some(task) => cx.spawn(async move |_, _| task.await),
            None => gpui::Task::ready(Err(anyhow::anyhow!("no such entry {:?}", entry_id))),
        }
    }

    /// Restore a trashed entry and refresh its path so the worktree observes it.
    pub fn restore_entry(
        &mut self,
        worktree_id: worktree::WorktreeId,
        trashed_entry: fs::TrashedEntry,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<crate::ProjectPath>> {
        let Some(worktree) = self.worktree_for_id(worktree_id, cx) else {
            return gpui::Task::ready(Err(anyhow::anyhow!("worktree {} not found", worktree_id)));
        };
        cx.spawn(async move |this, cx| {
            let restored_path =
                Worktree::restore_entry(trashed_entry, worktree.clone(), cx).await?;
            let path: Arc<RelPath> = restored_path.into();
            // Refresh the restored path so the worktree picks up the on-disk change.
            let refresh = worktree.update(cx, |worktree, cx| match worktree {
                Worktree::Local(local) => local.refresh_entry(path.clone(), None, cx),
                Worktree::Remote(_) => Task::ready(Err(anyhow::anyhow!(
                    "cannot refresh remote worktree after restore"
                ))),
            });
            refresh.await?;
            this.update(cx, |_, _| crate::ProjectPath {
                worktree_id,
                path: path.clone(),
            })
        })
    }

    /// Copy an entry to a new project path via `WorktreeStore::copy_entry`.
    pub fn copy_entry(
        &mut self,
        entry_id: ProjectEntryId,
        path: crate::ProjectPath,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<worktree::CreatedEntry>> {
        let worktree_store = self.worktree_store.clone();
        let copy = self
            .worktree_store
            .update(cx, |store, cx| store.copy_entry(entry_id, path.clone(), cx));
        cx.spawn(async move |_, cx| match copy.await? {
            Some(entry) => Ok(worktree::CreatedEntry::Included(entry)),
            None => {
                let abs_path = worktree_store
                    .read_with(cx, |store, cx| store.absolutize(&path, cx))
                    .unwrap_or_default();
                Ok(worktree::CreatedEntry::Excluded { abs_path })
            }
        })
    }

    /// Expand a single entry, detaching the underlying worktree task.
    pub fn expand_entry(
        &mut self,
        worktree_id: worktree::WorktreeId,
        entry_id: ProjectEntryId,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(worktree) = self.worktree_for_id(worktree_id, cx) else {
            return;
        };
        let Some(task) = worktree.update(cx, |worktree, cx| worktree.expand_entry(entry_id, cx))
        else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(err) = task.await {
                log::error!("failed to expand entry {entry_id:?}: {err:?}");
            }
        })
        .detach();
    }

    /// Recursively expand an entry's descendants, emitting
    /// `Event::ExpandedAllForEntry` when the scan completes.
    pub fn expand_all_for_entry(
        &mut self,
        worktree_id: worktree::WorktreeId,
        entry_id: ProjectEntryId,
        cx: &mut gpui::Context<Self>,
    ) -> Option<gpui::Task<anyhow::Result<()>>> {
        let worktree = self.worktree_for_id(worktree_id, cx)?;
        let task = worktree.update(cx, |worktree, cx| {
            worktree.expand_all_for_entry(entry_id, cx)
        })?;
        Some(cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |_, cx| {
                cx.emit(crate::Event::ExpandedAllForEntry(worktree_id, entry_id));
            })?;
            Ok(())
        }))
    }

    /// Return a loaded buffer by its stable identifier.
    pub fn buffer_for_id(
        &self,
        buffer_id: text::BufferId,
        cx: &gpui::App,
    ) -> Option<gpui::Entity<language::Buffer>> {
        self.buffer_store.read(cx).get(buffer_id)
    }

    /// Return project paths for open buffers with unsaved changes.
    pub fn dirty_buffers(&self, cx: &App) -> impl Iterator<Item = crate::ProjectPath> {
        let paths = self
            .buffer_store
            .read(cx)
            .buffers()
            .filter_map(|buffer| {
                let buffer = buffer.read(cx);
                if !buffer.is_dirty() {
                    return None;
                }
                let file = buffer.file()?;
                Some(crate::ProjectPath {
                    worktree_id: file.worktree_id(cx),
                    path: file.path().clone(),
                })
            })
            .collect::<Vec<_>>();
        paths.into_iter()
    }

    pub fn is_via_wsl_with_host_interop(&self, _cx: &App) -> bool {
        false
    }

    /// Copy a local project file to a destination selected by the user.
    pub fn download_file(
        &mut self,
        worktree_id: worktree::WorktreeId,
        entry_path: crate::ProjectPath,
        destination_path: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        if entry_path.worktree_id != worktree_id {
            return gpui::Task::ready(Err(anyhow::anyhow!(
                "download path belongs to worktree {}, expected {}",
                entry_path.worktree_id,
                worktree_id
            )));
        }
        let Some(source_path) = self.absolute_path(&entry_path, cx) else {
            return gpui::Task::ready(Err(anyhow::anyhow!(
                "project entry does not exist: {}",
                entry_path.path.as_std_path().display()
            )));
        };
        let fs = self.fs.clone();
        gpui::AppContext::background_spawn(cx, async move {
            fs.copy_file(&source_path, &destination_path, fs::CopyOptions::default())
                .await
        })
    }

    pub fn move_worktree(
        &mut self,
        worktree_id: worktree::WorktreeId,
        destination_id: worktree::WorktreeId,
        cx: &mut gpui::Context<Self>,
    ) -> anyhow::Result<()> {
        self.worktree_store.update(cx, |worktree_store, cx| {
            worktree_store.move_worktree(worktree_id, destination_id, cx)
        })
    }

    /// 获取符号列表
    pub fn symbols(
        &mut self,
        _query: &str,
        _cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<anyhow::Result<Vec<crate::lsp_store::SymbolLocation>>> {
        lsp_unavailable()
    }
}

// ---------------------------------------------------------------------------
// Extension stubs (spec §8.2 M2)
// ---------------------------------------------------------------------------


/// Stub: VimModeSetting (vim_mode_setting crate 已删除)
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct VimModeSetting(pub bool);

impl settings::SettingsKey for VimModeSetting {
    const KEY: Option<&'static str> = None;
}

impl settings::Settings for VimModeSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.vim_mode.unwrap_or(false))
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

pub struct ShellBuilder {
    inner: util::shell_builder::ShellBuilder,
}

impl ShellBuilder {
    pub fn new(shell: &Shell, is_windows: bool) -> Self {
        let shell = match shell {
            Shell::System => util::shell::Shell::System,
            Shell::Program(config) => util::shell::Shell::WithArguments {
                program: config.program.clone(),
                args: config.args.clone(),
                title_override: None,
            },
        };
        Self {
            inner: util::shell_builder::ShellBuilder::new(&shell, is_windows),
        }
    }

    pub fn command_label(&self, command: &str) -> String {
        self.inner.command_label(command)
    }

    pub fn build_no_quote(
        self,
        command: Option<String>,
        args: &[String],
    ) -> (String, Vec<String>) {
        self.inner.build_no_quote(command, args)
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

/// Search result stream shared by project and text-finder searches.
pub struct SearchResults<T> {
    pub tx: async_channel::Sender<T>,
    pub rx: async_channel::Receiver<T>,
}

impl<T> Clone for SearchResults<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            rx: self.rx.clone(),
        }
    }
}

/// Stub: Search alias for SearchQuery (task crate 已删除)
pub type Search = crate::search::SearchQuery;

#[cfg(test)]
mod stub_delegate_tests {
    use super::*;
    use crate::Project;
    use fs::{FakeFs, Fs};
    use gpui::TestAppContext;
    use language::LanguageRegistry;
    use serde_json::json;
    use settings::SettingsStore;
    use std::{path::Path, path::PathBuf, sync::Arc};
    use util::rel_path::rel_path_buf;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn setup_project(
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, gpui::Entity<Project>, worktree::WorktreeId) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ "dir": {}, "search.txt": "hello needle world\n" }),
        )
        .await;
        // Build the Project synchronously (Project::test needs &mut App, which we
        // obtain via cx.update), then add the worktree with an awaitable task.
        let project = cx.update(|cx| {
            Project::local(
                Arc::new(language::LanguageRegistry::new(
                    cx.background_executor().clone(),
                )),
                fs.clone(),
                None,
                Vec::new(),
                cx,
            )
        });
        let worktree = project
            .update(cx, |p, cx| {
                p.add_local_worktree(std::path::PathBuf::from("/project"), true, cx)
            })
            .await
            .expect("worktree created");
        cx.update(|cx| {
            worktree
                .read(cx)
                .as_local()
                .expect("local worktree")
                .scan_complete()
        })
        .await;
        let worktree_id = worktree.read_with(cx, |w, _| w.id());
        (fs, project, worktree_id)
    }

    fn project_path(worktree_id: worktree::WorktreeId, rel: &str) -> ProjectPath {
        ProjectPath {
            worktree_id,
            path: rel_path_buf(rel).into(),
        }
    }

    #[gpui::test]
    async fn test_entry_lookups(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project, worktree_id) = setup_project(cx).await;

        let path = project_path(worktree_id, "dir/file.txt");
        let created = project
            .update(cx, |p, cx| p.create_entry(path.clone(), false, cx))
            .await
            .expect("create_entry");
        let entry_id = match created {
            worktree::CreatedEntry::Included(entry) => entry.id,
            other => panic!("expected Included, got {other:?}"),
        };

        // path_for_entry resolves the same ProjectPath.
        assert_eq!(
            project.read_with(cx, |p, cx| p.path_for_entry(entry_id, cx)),
            Some(path)
        );

        // worktree_id_for_entry resolves the owning worktree.
        assert_eq!(
            project.read_with(cx, |p, cx| p.worktree_id_for_entry(entry_id, cx)),
            Some(worktree_id)
        );

        // A nested file is not the worktree root.
        assert!(!project.read_with(cx, |p, cx| p.entry_is_worktree_root(entry_id, cx)));

        // The root entry IS the root.
        let root_id = project
            .read_with(cx, |p, cx| {
                p.worktrees(cx)
                    .next()
                    .and_then(|w| w.read(cx).root_entry().map(|e| e.id))
            })
            .expect("root entry");
        assert!(project.read_with(cx, |p, cx| p.entry_is_worktree_root(root_id, cx)));

        // Lookups for an unknown entry yield None/false.
        assert_eq!(
            project.read_with(cx, |p, cx| p.path_for_entry(ProjectEntryId::MAX, cx)),
            None
        );
        assert_eq!(
            project.read_with(cx, |p, cx| p.worktree_id_for_entry(ProjectEntryId::MAX, cx)),
            None
        );
        assert!(!project.read_with(cx, |p, cx| {
            p.entry_is_worktree_root(ProjectEntryId::MAX, cx)
        }));
    }

    #[gpui::test]
    async fn test_delete_missing_entry_returns_err(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project, _worktree_id) = setup_project(cx).await;

        // Deleting a never-created entry MUST be Err, never the false Ok(None)
        // success the old stub returned.
        let result = project
            .update(cx, |p, cx| p.delete_entry(ProjectEntryId::MAX, true, cx))
            .await;
        assert!(
            result.is_err(),
            "deleting a missing entry must return Err, got {result:?}"
        );
    }

    #[gpui::test]
    async fn test_create_rename_copy_delete_restore(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, project, worktree_id) = setup_project(cx).await;

        // create
        let src = project_path(worktree_id, "dir/a.txt");
        let created = project
            .update(cx, |p, cx| p.create_entry(src.clone(), false, cx))
            .await
            .expect("create_entry");
        let entry_id = match created {
            worktree::CreatedEntry::Included(e) => e.id,
            other => panic!("expected Included, got {other:?}"),
        };
        assert!(fs.is_file(Path::new("/project/dir/a.txt")).await);

        // rename a.txt -> b.txt
        let renamed = project_path(worktree_id, "dir/b.txt");
        project
            .update(cx, |p, cx| p.rename_entry(entry_id, renamed.clone(), cx))
            .await
            .expect("rename_entry");
        assert!(fs.is_file(Path::new("/project/dir/b.txt")).await);

        // copy b.txt -> c.txt
        let copied = project_path(worktree_id, "dir/c.txt");
        let copy_result = project
            .update(cx, |p, cx| p.copy_entry(entry_id, copied.clone(), cx))
            .await
            .expect("copy_entry");
        assert!(matches!(copy_result, worktree::CreatedEntry::Included(_)));
        assert!(fs.is_file(Path::new("/project/dir/c.txt")).await);

        // delete (trash) the renamed entry, receiving a TrashedEntry.
        let trashed = project
            .update(cx, |p, cx| p.delete_entry(entry_id, true, cx))
            .await
            .expect("delete_entry")
            .expect("trashed entry present after trash delete");
        assert!(!fs.is_file(Path::new("/project/dir/b.txt")).await);

        // restore recreates the file and returns its ProjectPath.
        let restored = project
            .update(cx, |p, cx| p.restore_entry(worktree_id, trashed, cx))
            .await
            .expect("restore_entry");
        assert_eq!(restored.worktree_id, worktree_id);
    }

    #[gpui::test]
    async fn test_create_entry_unknown_worktree_errors(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project, _worktree_id) = setup_project(cx).await;

        // Creating in a non-existent worktree fails rather than silently no-op'ing.
        let bogus = worktree::WorktreeId::from_proto(9999);
        let result = project
            .update(cx, |p, cx| {
                p.create_entry(project_path(bogus, "x.txt"), false, cx)
            })
            .await;
        assert!(
            result.is_err(),
            "create_entry in unknown worktree must error"
        );
    }
    #[gpui::test]
    async fn test_project_search_stream_returns_matching_buffer(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, project, _worktree_id) = setup_project(cx).await;
        let query = crate::search::SearchQuery::text(
            "needle",
            false,
            true,
            false,
            util::paths::PathMatcher::default(),
            util::paths::PathMatcher::default(),
            false,
            None,
        )
        .expect("valid text query");
        let results = project.update(cx, |project, cx| project.search(query, cx));

        let mut found = false;
        while let Ok(result) = results.rx.recv().await {
            if let crate::search::SearchResult::Buffer { ranges, .. } = result {
                assert!(!ranges.is_empty(), "matching buffers must contain ranges");
                found = true;
                break;
            }
        }
        assert!(found, "project search must emit a matching buffer result");
    }

}
