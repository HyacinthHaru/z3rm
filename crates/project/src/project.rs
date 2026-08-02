pub mod bookmark_store;
pub mod buffer_store;
pub mod debounced_delay;
pub mod environment;
pub mod git_store;
pub mod manifest_tree;
pub mod project_settings;
pub mod search;
pub mod search_history;
pub mod stubs;
pub mod toolchain_store;
pub mod trusted_worktrees;
pub mod worktree_store;

pub use stubs::*;

use buffer_diff::BufferDiff;
pub use environment::ProjectEnvironmentEvent;
use git::repository::get_git_committer;
use git_store::{Repository, RepositoryId};

use crate::{
    git_store::GitStore,
    project_settings::{ProjectSettings, SettingsObserver, SettingsObserverEvent},
    trusted_worktrees::{PathTrust, RemoteHostLocation, TrustedWorktrees},
    worktree_store::WorktreeIdCounter,
};
pub use git_store::{
    ConflictRegion, ConflictSet, ConflictSetSnapshot, ConflictSetUpdate,
    git_traversal::{ChildEntriesGitIter, GitEntry, GitEntryRef, GitTraversal},
    repo_identity_path,
};
pub use manifest_tree::ManifestTree;
pub use worktree_store::WorktreePaths;

use anyhow::{Context as _, Result, anyhow};
use buffer_store::{BufferStore, BufferStoreEvent};
use clock::ReplicaId;

use collections::{BTreeSet, HashMap, HashSet, IndexSet};
use debounced_delay::DebouncedDelay;

pub use environment::ProjectEnvironment;

use ::git::{blame::Blame, status::FileStatus};
use gpui::{
    App, AppContext, AsyncApp, BorrowAppContext, Context, Entity, EventEmitter, Hsla, SharedString,
    Task, TaskExt, WeakEntity, Window,
};
use language::{Buffer, File as LanguageFile, LanguageRegistry};
use parking_lot::Mutex;
use rpc::{
    AnyProtoClient, ErrorCode,
    proto::{self, LanguageServerPromptResponse, REMOTE_SERVER_PROJECT_ID},
};
use search::{SearchInputKind, SearchQuery, SearchResult};
use search_history::SearchHistory;
use settings::{InvalidSettingsError, RegisterSetting, Settings, SettingsLocation, SettingsStore};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    ops::{Not as _, Range},
    path::{Path, PathBuf},
    pin::pin,
    str::{self, FromStr},
    sync::Arc,
    time::Duration,
};
use text::{Anchor, BufferId, Point, Rope};
use toolchain_store::EmptyToolchainStore;
use util::{
    ResultExt as _, maybe,
    path_list::PathList,
    paths::{PathStyle, SanitizedPath, is_absolute},
    rel_path::RelPath,
};
use worktree::{CreatedEntry, Snapshot, Traversal};
pub use worktree::{
    Entry, EntryKind, FS_WATCH_LATENCY, File, LocalWorktree, PathChange, ProjectEntryId,
    UpdatedEntriesSet, UpdatedGitRepositoriesSet, Worktree, WorktreeId, WorktreeSettings,
    discover_root_repo_common_dir,
};
use worktree_store::{WorktreeStore, WorktreeStoreEvent};

pub use buffer_store::ProjectTransaction;
pub use fs::*;
pub use language::Location;
pub use stubs::Shell;
pub use toolchain_store::{ToolchainStore, Toolchains};
const MAX_PROJECT_SEARCH_HISTORY_SIZE: usize = 500;

#[derive(Clone, Copy, Debug)]
pub struct LocalProjectFlags {
    pub init_worktree_trust: bool,
    pub watch_global_configs: bool,
}

impl Default for LocalProjectFlags {
    fn default() -> Self {
        Self {
            init_worktree_trust: true,
            watch_global_configs: true,
        }
    }
}

pub trait ProjectItem: 'static {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>>
    where
        Self: Sized;
    fn entry_id(&self, cx: &App) -> Option<ProjectEntryId>;
    fn project_path(&self, cx: &App) -> Option<ProjectPath>;
    fn is_dirty(&self) -> bool;
}

/// `Project` manages worktree and git integration.
pub struct Project {
    active_entry: Option<ProjectEntryId>,
    languages: Arc<LanguageRegistry>,
    fs: Arc<dyn Fs>,
    git_store: Entity<GitStore>,
    worktree_store: Entity<WorktreeStore>,
    buffer_store: Entity<BufferStore>,
    remote_client: Option<Entity<remote::RemoteClient>>,
    remote_connection_options: Option<remote::RemoteConnectionOptions>,
    _subscriptions: Vec<gpui::Subscription>,
    buffers_needing_diff: HashSet<WeakEntity<Buffer>>,
    git_diff_debouncer: DebouncedDelay<Self>,
    search_history: SearchHistory,
    search_included_history: SearchHistory,
    search_excluded_history: SearchHistory,
    environment: Entity<ProjectEnvironment>,
    settings_observer: Entity<SettingsObserver>,
    toolchain_store: Option<Entity<ToolchainStore>>,
    /// Inert store entities for removed features (task/debugger/bookmarks/breakpoints/LSP).
    /// Created once at construction so callers get a valid handle instead of a panic.
    task_store_entity: Entity<crate::task_store::TaskStore>,
    dap_store_entity: Entity<stubs::DapStore>,
    bookmark_store_entity: Entity<crate::bookmark_store::BookmarkStore>,
    breakpoint_store_entity: Entity<stubs::debugger::breakpoint_store::BreakpointStore>,
    lsp_store_entity: Entity<stubs::lsp_store::LspStore>,
    last_worktree_paths: WorktreePaths,
}

pub enum Event {
    Closed,
    WorktreeAdded(WorktreeId),
    WorktreeRemoved(WorktreeId),
    WorktreeOrderChanged,
    ActiveEntryChanged(Option<ProjectEntryId>),
    DeletedEntry(WorktreeId, ProjectEntryId),
    WorktreePathsChanged {
        old_worktree_paths: WorktreePaths,
    },
    WorktreeUpdatedEntries(WorktreeId, UpdatedEntriesSet),
    Toast {
        notification_id: String,
        message: String,
        link: Option<String>,
    },
    /// Stub variants for deleted diagnostic/remote features (spec §8.2 M2)
    DiskBasedDiagnosticsStarted,
    DiskBasedDiagnosticsFinished {
        language_server_id: lsp::LanguageServerId,
    },
    DiagnosticsUpdated {
        paths: Vec<Arc<util::rel_path::RelPath>>,
        language_server_id: lsp::LanguageServerId,
    },
    LanguageServerRemoved(lsp::LanguageServerId),
    DisconnectedFromRemote {
        server_not_running: bool,
    },
    DisconnectedFromHost,
    LanguageNotFound(Entity<language::Buffer>),
    /// Stub variants for project_panel (spec §8.2 M3)
    RevealInProjectPanel(ProjectEntryId),
    ActivateProjectPanel,
    ExpandedAllForEntry(WorktreeId, ProjectEntryId),
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ProjectPath {
    pub worktree_id: WorktreeId,
    pub path: Arc<RelPath>,
}

impl ProjectPath {
    pub fn from_file(value: &dyn language::File, cx: &App) -> Self {
        ProjectPath {
            worktree_id: value.worktree_id(cx),
            path: value.path().clone(),
        }
    }

    pub fn from_proto(p: proto::ProjectPath) -> Option<Self> {
        Some(Self {
            worktree_id: WorktreeId::from_proto(p.worktree_id),
            path: RelPath::from_proto(&p.path).log_err()?,
        })
    }

    pub fn to_proto(&self) -> proto::ProjectPath {
        proto::ProjectPath {
            worktree_id: self.worktree_id.to_proto(),
            path: self.path.as_ref().to_proto(),
        }
    }

    pub fn root_path(worktree_id: WorktreeId) -> Self {
        Self {
            worktree_id,
            path: RelPath::empty_arc(),
        }
    }

    pub fn starts_with(&self, other: &ProjectPath) -> bool {
        self.worktree_id == other.worktree_id && self.path.starts_with(&other.path)
    }
}

impl Project {
    pub fn local(
        languages: Arc<LanguageRegistry>,
        fs: Arc<dyn Fs>,
        env: Option<HashMap<String, String>>,
        worktrees: Vec<PathBuf>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let worktree_store =
                cx.new(|cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::get(cx)));

            let buffer_store = cx.new(|cx| BufferStore::local(worktree_store.clone(), cx));
            let project_settings = cx.new(|cx| {
                SettingsObserver::new_local(fs.clone(), worktree_store.clone(), true, cx)
            });

            let environment = cx.new(|cx| {
                ProjectEnvironment::new(env, worktree_store.downgrade(), None, false, cx)
            });

            let git_store = cx.new(|cx| {
                GitStore::local(
                    &worktree_store,
                    buffer_store.clone(),
                    environment.clone(),
                    fs.clone(),
                    cx,
                )
            });

            let worktree_store_subscription =
                cx.subscribe(&worktree_store, Self::on_worktree_store_event);

            let bookmark_store_entity = cx.new(|_| {
                crate::bookmark_store::BookmarkStore::new(
                    worktree_store.clone(),
                    buffer_store.clone(),
                )
            });

            let mut project = Self {
                active_entry: None,
                languages,
                fs: fs.clone(),
                git_store,
                worktree_store,
                buffer_store,
                remote_client: None,
                remote_connection_options: None,
                _subscriptions: vec![worktree_store_subscription],
                buffers_needing_diff: HashSet::default(),
                git_diff_debouncer: DebouncedDelay::new(),
                search_history: SearchHistory::new(
                    Some(MAX_PROJECT_SEARCH_HISTORY_SIZE),
                    search_history::QueryInsertionBehavior::default(),
                ),
                search_included_history: SearchHistory::new(
                    Some(MAX_PROJECT_SEARCH_HISTORY_SIZE),
                    search_history::QueryInsertionBehavior::default(),
                ),
                search_excluded_history: SearchHistory::new(
                    Some(MAX_PROJECT_SEARCH_HISTORY_SIZE),
                    search_history::QueryInsertionBehavior::default(),
                ),
                environment,
                settings_observer: project_settings,
                toolchain_store: None,
                task_store_entity: cx.new(|_| crate::task_store::TaskStore::default()),
                dap_store_entity: cx.new(|_| stubs::DapStore::default()),
                bookmark_store_entity,
                breakpoint_store_entity: cx
                    .new(|_| stubs::debugger::breakpoint_store::BreakpointStore::default()),
                lsp_store_entity: cx.new(|_| stubs::lsp_store::LspStore::default()),
                last_worktree_paths: WorktreePaths::default(),
            };

            for worktree_path in worktrees {
                project
                    .add_local_worktree(worktree_path, true, cx)
                    .detach_and_log_err(cx);
            }

            project
        })
    }
    /// Constructs a project backed by a connected remote server.
    pub fn remote(
        remote: Entity<remote::RemoteClient>,
        languages: Arc<LanguageRegistry>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) -> Entity<Self> {
        let (proto_client, path_style, connection_options) = remote.read_with(cx, |remote, _| {
            (
                remote.proto_client(),
                remote.path_style(),
                remote.connection_options(),
            )
        });

        WorktreeStore::init(&proto_client);
        WorktreeStore::init_remote(&proto_client);
        BufferStore::init(&proto_client);
        GitStore::init(&proto_client);
        SettingsObserver::init(&proto_client);

        cx.new(|cx| {
            let worktree_store = cx.new(|cx| {
                WorktreeStore::remote(
                    false,
                    proto_client.clone(),
                    REMOTE_SERVER_PROJECT_ID,
                    path_style,
                    WorktreeIdCounter::get(cx),
                )
            });
            let buffer_store = cx.new(|cx| {
                BufferStore::remote(
                    worktree_store.clone(),
                    proto_client.clone(),
                    REMOTE_SERVER_PROJECT_ID,
                    cx,
                )
            });
            let project_settings = cx.new(|cx| {
                SettingsObserver::new_remote(
                    fs.clone(),
                    worktree_store.clone(),
                    Some(proto_client.clone()),
                    proto_client.is_via_collab(),
                    cx,
                )
            });
            let environment = cx.new(|cx| {
                ProjectEnvironment::new(
                    None,
                    worktree_store.downgrade(),
                    Some(remote.downgrade()),
                    true,
                    cx,
                )
            });
            let git_store = cx.new(|cx| {
                GitStore::remote(
                    &worktree_store,
                    buffer_store.clone(),
                    proto_client.clone(),
                    REMOTE_SERVER_PROJECT_ID,
                    cx,
                )
            });
            let worktree_store_subscription =
                cx.subscribe(&worktree_store, Self::on_worktree_store_event);
            let bookmark_store_entity = cx.new(|_| {
                crate::bookmark_store::BookmarkStore::new(
                    worktree_store.clone(),
                    buffer_store.clone(),
                )
            });

            let project = Self {
                active_entry: None,
                languages,
                fs,
                git_store,
                worktree_store: worktree_store.clone(),
                buffer_store,
                remote_client: Some(remote.clone()),
                remote_connection_options: Some(connection_options),
                _subscriptions: vec![worktree_store_subscription],
                buffers_needing_diff: HashSet::default(),
                git_diff_debouncer: DebouncedDelay::new(),
                search_history: SearchHistory::new(
                    Some(MAX_PROJECT_SEARCH_HISTORY_SIZE),
                    search_history::QueryInsertionBehavior::default(),
                ),
                search_included_history: SearchHistory::new(
                    Some(MAX_PROJECT_SEARCH_HISTORY_SIZE),
                    search_history::QueryInsertionBehavior::default(),
                ),
                search_excluded_history: SearchHistory::new(
                    Some(MAX_PROJECT_SEARCH_HISTORY_SIZE),
                    search_history::QueryInsertionBehavior::default(),
                ),
                environment,
                settings_observer: project_settings,
                toolchain_store: None,
                task_store_entity: cx.new(|_| crate::task_store::TaskStore::default()),
                dap_store_entity: cx.new(|_| stubs::DapStore::default()),
                bookmark_store_entity,
                breakpoint_store_entity: cx
                    .new(|_| stubs::debugger::breakpoint_store::BreakpointStore::default()),
                lsp_store_entity: cx.new(|_| stubs::lsp_store::LspStore::default()),
                last_worktree_paths: WorktreePaths::default(),
            };

            proto_client.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &cx.entity());
            proto_client.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &worktree_store);
            proto_client.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &project.buffer_store);
            proto_client.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &project.git_store);
            proto_client.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &project.settings_observer);

            project
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test(
        fs: Arc<dyn Fs>,
        root_paths: impl IntoIterator<Item = &Path>,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<Self> {
        let languages = Arc::new(LanguageRegistry::test(cx.executor()));
        let project = cx.update(|cx| Self::local(languages, fs, None, Vec::new(), cx));

        for root_path in root_paths {
            let root_path = root_path.to_path_buf();
            let worktree = project
                .update(cx, |project, cx| {
                    project.add_local_worktree(root_path.clone(), true, cx)
                })
                .await
                .unwrap_or_else(|error| {
                    panic!("failed to create test worktree at {root_path:?}: {error}")
                });

            // Tests inspect worktree entries immediately after this returns, so the
            // initial scan has to finish here or those lookups race against it.
            let scan_complete = worktree.read_with(cx, |worktree, _| {
                worktree.as_local().map(|local| local.scan_complete())
            });
            if let Some(scan_complete) = scan_complete {
                scan_complete.await;
            }
        }

        project
    }

    pub fn fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }

    pub fn languages(&self) -> &Arc<LanguageRegistry> {
        &self.languages
    }

    pub fn worktree_store(&self) -> &Entity<WorktreeStore> {
        &self.worktree_store
    }

    pub fn git_store(&self) -> &Entity<GitStore> {
        &self.git_store
    }

    pub fn buffer_store(&self) -> &Entity<BufferStore> {
        &self.buffer_store
    }

    pub fn environment(&self) -> &Entity<ProjectEnvironment> {
        &self.environment
    }

    pub fn settings_observer(&self) -> &Entity<SettingsObserver> {
        &self.settings_observer
    }

    pub fn active_entry(&self) -> Option<ProjectEntryId> {
        self.active_entry
    }

    pub fn set_active_entry(
        &mut self,
        active_entry: Option<ProjectEntryId>,
        cx: &mut Context<Self>,
    ) {
        if active_entry != self.active_entry {
            self.active_entry = active_entry;
            cx.emit(Event::ActiveEntryChanged(active_entry));
        }
    }

    pub fn set_active_path(&mut self, path: Option<&ProjectPath>, cx: &mut Context<Self>) {
        let active_entry = path.and_then(|path| Some(self.entry_for_path(path, cx)?.id));
        self.set_active_entry(active_entry, cx);
    }

    pub fn worktree_for_entry(
        &self,
        entry_id: ProjectEntryId,
        cx: &App,
    ) -> Option<Entity<Worktree>> {
        self.worktree_store
            .read(cx)
            .worktree_for_entry(entry_id, cx)
    }

    pub fn worktree_for_id(&self, id: WorktreeId, cx: &App) -> Option<Entity<Worktree>> {
        self.worktree_store.read(cx).worktree_for_id(id, cx)
    }

    pub fn worktrees(&self, cx: &App) -> impl Iterator<Item = Entity<Worktree>> {
        self.worktree_store.read(cx).worktrees()
    }

    pub fn entry_for_path<'a>(&'a self, path: &ProjectPath, cx: &'a App) -> Option<&'a Entry> {
        self.worktree_store.read(cx).entry_for_path(path, cx)
    }

    pub fn entry_for_id<'a>(&'a self, entry_id: ProjectEntryId, cx: &'a App) -> Option<&'a Entry> {
        self.worktree_store.read(cx).entry_for_id(entry_id, cx)
    }

    pub fn project_path_for_absolute_path(&self, abs_path: &Path, cx: &App) -> Option<ProjectPath> {
        self.worktree_store
            .read(cx)
            .project_path_for_absolute_path(abs_path, cx)
    }

    pub fn absolute_path(&self, path: &ProjectPath, cx: &App) -> Option<PathBuf> {
        self.worktree_store.read(cx).absolutize(path, cx)
    }

    pub fn default_visible_worktree_paths(
        worktree_store: &WorktreeStore,
        cx: &App,
    ) -> Vec<Arc<Path>> {
        worktree_store
            .worktrees()
            .filter(|worktree| worktree.read(cx).is_visible())
            .map(|worktree| worktree.read(cx).abs_path())
            .collect()
    }

    pub fn worktree_paths(&self, cx: &App) -> WorktreePaths {
        self.worktree_store.read(cx).paths(cx)
    }

    pub fn path_style(&self, cx: &App) -> PathStyle {
        self.worktree_store
            .read(cx)
            .worktrees()
            .next()
            .map(|worktree| worktree.read(cx).path_style())
            .unwrap_or(PathStyle::Posix)
    }

    /// Re-emits `WorktreeStore` events as `project::Event`s. Everything downstream
    /// (project panel, file finder, workspace serialization, git blame) subscribes
    /// to the project rather than to the store, so without this bridge those views
    /// never learn that the tree changed.
    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeStoreEvent::WorktreeAdded(worktree) => {
                let worktree_id = worktree.read(cx).id();
                cx.emit(Event::WorktreeAdded(worktree_id));
                self.emit_worktree_paths_changed(cx);
            }
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                cx.emit(Event::WorktreeRemoved(*worktree_id));
                self.emit_worktree_paths_changed(cx);
            }
            WorktreeStoreEvent::WorktreeOrderChanged => {
                cx.emit(Event::WorktreeOrderChanged);
                self.emit_worktree_paths_changed(cx);
            }
            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, changes) => {
                cx.emit(Event::WorktreeUpdatedEntries(*worktree_id, changes.clone()));
            }
            WorktreeStoreEvent::WorktreeDeletedEntry(worktree_id, entry_id) => {
                cx.emit(Event::DeletedEntry(*worktree_id, *entry_id));
            }
            WorktreeStoreEvent::WorktreeReleased(..)
            | WorktreeStoreEvent::WorktreeUpdateSent(_)
            | WorktreeStoreEvent::WorktreeUpdatedGitRepositories(..)
            | WorktreeStoreEvent::WorktreeUpdatedRootRepoCommonDir(_) => {}
        }
    }

    fn emit_worktree_paths_changed(&mut self, cx: &mut Context<Self>) {
        let worktree_paths = self.worktree_store.read(cx).paths(cx);
        if worktree_paths != self.last_worktree_paths {
            let old_worktree_paths =
                std::mem::replace(&mut self.last_worktree_paths, worktree_paths);
            cx.emit(Event::WorktreePathsChanged { old_worktree_paths });
        }
    }

    pub fn add_local_worktree(
        &mut self,
        abs_path: impl Into<PathBuf> + Send + 'static,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Worktree>>> {
        let worktree_store = self.worktree_store.clone();
        cx.spawn(async move |_, cx| {
            let task = worktree_store.update(cx, |store, cx| {
                store.create_worktree(abs_path.into(), visible, cx)
            });
            task.await
        })
    }

    pub fn open_buffer(
        &mut self,
        path: ProjectPath,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        self.buffer_store
            .update(cx, |store, cx| store.open_buffer(path, cx))
    }

    pub fn open_uncommitted_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<buffer_diff::BufferDiff>>> {
        self.git_store
            .update(cx, |store, cx| store.open_uncommitted_diff(buffer, cx))
    }

    pub fn open_unstaged_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<buffer_diff::BufferDiff>>> {
        self.git_store
            .update(cx, |store, cx| store.open_unstaged_diff(buffer, cx))
    }

    pub fn open_staged_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Entity<buffer_diff::BufferDiff>, Entity<Buffer>)>> {
        self.git_store
            .update(cx, |store, cx| store.open_staged_diff(buffer, cx))
    }

    // =====================================================================
    // 以下为 stub 方法 — 对应已删除的远程协作 / 终端 / 符号跳转功能模块
    // =====================================================================

    /// Stub: is_shared (collaboration 模块已删除)
    pub fn is_shared(&self) -> bool {
        false
    }

    /// Returns whether this project is backed by a connected remote server.
    pub fn is_via_remote_server(&self) -> bool {
        self.remote_client.is_some()
    }

    pub fn project_path_git_status(
        &self,
        path: &ProjectPath,
        cx: &App,
    ) -> Option<git::status::FileStatus> {
        self.git_store.read(cx).project_path_git_status(path, cx)
    }

    pub fn set_language_for_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        language: Arc<language::Language>,
        cx: &mut Context<Self>,
    ) {
        buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
    }

    pub fn open_buffer_for_symbol(
        &self,
        symbol: &Symbol,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Entity<language::Buffer>>> {
        let Some(path) = symbol.path.clone() else {
            return Task::ready(Err(anyhow::anyhow!(
                "symbol {:?} has no project path",
                symbol.name
            )));
        };

        self.buffer_store
            .update(cx, |store, cx| store.open_buffer(path, cx))
    }

    /// Create an interactive terminal using the configured shell.
    pub fn create_terminal_shell(
        &self,
        working_directory: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<gpui::Entity<terminal::Terminal>>> {
        let settings = terminal::terminal_settings::TerminalSettings::get_global(cx);
        let path_style = self.path_style(cx);
        let builder = terminal::TerminalBuilder::new(
            working_directory,
            None,
            settings.shell.clone(),
            settings.env.clone(),
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            settings.path_hyperlink_regexes.clone(),
            settings.path_hyperlink_timeout_ms,
            false,
            0,
            None,
            cx,
            Vec::new(),
            path_style,
        );
        cx.spawn(async move |_, cx| {
            let builder = builder.await?;
            Ok(cx.new(|cx| builder.subscribe(cx)))
        })
    }

    /// Clone an existing terminal, preserving its shell and terminal settings.
    pub fn clone_terminal(
        &self,
        terminal: &gpui::Entity<terminal::Terminal>,
        cx: &mut Context<Self>,
        working_directory: Option<std::path::PathBuf>,
    ) -> Task<anyhow::Result<gpui::Entity<terminal::Terminal>>> {
        let builder = terminal.read(cx).clone_builder(cx, working_directory);
        cx.spawn(async move |_, cx| {
            let builder = builder.await?;
            Ok(cx.new(|cx| builder.subscribe(cx)))
        })
    }

    pub fn is_via_collab(&self) -> bool {
        false
    }

    fn shell_for_terminal_task(task: &SpawnInTerminal) -> util::shell::Shell {
        let program = if !task.command.is_empty() {
            Some(task.command.clone())
        } else if !task.program.is_empty() {
            Some(task.program.clone())
        } else {
            None
        };

        if let Some(program) = program {
            return util::shell::Shell::WithArguments {
                program,
                args: task.args.clone(),
                title_override: None,
            };
        }

        match &task.shell {
            Shell::System => util::shell::Shell::System,
            Shell::Program(config) => util::shell::Shell::WithArguments {
                program: config.program.clone(),
                args: config.args.clone(),
                title_override: None,
            },
        }
    }

    /// Create a terminal that runs a task and reports its exit status to the view.
    pub fn create_terminal_task(
        &mut self,
        task: SpawnInTerminal,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<gpui::Entity<terminal::Terminal>>> {
        let settings = terminal::terminal_settings::TerminalSettings::get_global(cx);
        let path_style = self.path_style(cx);
        let working_directory = task.cwd.clone().or_else(|| {
            task.working_directory
                .as_ref()
                .and_then(|path| self.absolute_path(path, cx))
        });
        let shell = Self::shell_for_terminal_task(&task);
        let command = if task.command.is_empty() {
            (!task.program.is_empty()).then(|| task.program.clone())
        } else {
            Some(task.command.clone())
        };

        let (completion_tx, completion_rx) = async_channel::unbounded();
        let spawned_task = terminal::SpawnInTerminal {
            command,
            args: task.args.clone(),
            label: task.label.clone(),
            full_label: task.full_label.clone(),
            command_label: task.command_label.clone(),
            hide: terminal::HideStrategy::Never,
            show_summary: task.show_summary,
            show_command: task.show_command,
            id: task.id,
            show_rerun: task.show_rerun,
        };
        let task_state = terminal::TaskState {
            status: terminal::TaskStatus::Running,
            completion_rx,
            spawned_task,
        };
        let mut env = settings.env.clone();
        env.extend(task.env);
        let builder = terminal::TerminalBuilder::new(
            working_directory,
            Some(task_state),
            shell,
            env,
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            settings.path_hyperlink_regexes.clone(),
            settings.path_hyperlink_timeout_ms,
            false,
            0,
            Some(completion_tx),
            cx,
            Vec::new(),
            path_style,
        );
        cx.spawn(async move |_, cx| {
            let builder = builder.await?;
            Ok(cx.new(|cx| builder.subscribe(cx)))
        })
    }

    /// Local projects use the same configured shell as their regular terminal.
    pub fn create_local_terminal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<gpui::Entity<terminal::Terminal>>> {
        self.create_terminal_shell(None, cx)
    }

    pub fn try_windows_path_to_wsl(
        &mut self,
        path: &std::path::Path,
        _cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<std::path::PathBuf>> {
        gpui::Task::ready(Ok(path.to_path_buf()))
    }

    /// §8.2 Delegate to WorktreeStore::find_or_create_worktree.
    pub fn find_or_create_worktree(
        &mut self,
        abs_path: &std::path::Path,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<gpui::Entity<Worktree>>> {
        let task = self.worktree_store.update(cx, |store, cx| {
            store.find_or_create_worktree(abs_path, visible, cx)
        });
        cx.spawn(async move |_, _| {
            let (worktree, _rel) = task.await?;
            Ok(worktree)
        })
    }

    pub fn is_read_only(&self, _cx: &App) -> bool {
        false
    }

    /// Wait until all visible worktrees have completed their initial scan.
    pub fn wait_for_initial_scan(&self, cx: &App) -> gpui::Task<()> {
        let wait = self.worktree_store.read(cx).wait_for_initial_scan();
        cx.background_spawn(wait)
    }

    /// Delete a project path via its worktree entry.
    pub fn delete_file(
        &mut self,
        path: ProjectPath,
        trash: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(worktree) = self.worktree_for_id(path.worktree_id, cx) else {
            return Task::ready(Err(anyhow::anyhow!("worktree not found for path")));
        };
        let entry_id = worktree
            .read(cx)
            .entry_for_path(path.path.as_ref())
            .map(|e| e.id);
        let Some(entry_id) = entry_id else {
            return Task::ready(Err(anyhow::anyhow!("entry not found for path")));
        };
        let task = worktree.update(cx, |worktree, cx| {
            worktree.delete_entry(entry_id, trash, cx)
        });
        match task {
            Some(task) => cx.spawn(async move |_, _| {
                task.await?;
                Ok(())
            }),
            None => Task::ready(Err(anyhow::anyhow!("delete_entry unavailable"))),
        }
    }

    pub fn create_worktree(
        &mut self,
        abs_path: impl Into<std::path::PathBuf>,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<gpui::Entity<Worktree>>> {
        let abs_path = abs_path.into();
        self.worktree_store
            .update(cx, |store, cx| store.create_worktree(abs_path, visible, cx))
    }

    /// §16.6 Delegate git stage/unstage to GitStore.
    pub fn stage_hunks(
        &mut self,
        buffer: gpui::Entity<language::Buffer>,
        unstaged_diff: gpui::Entity<buffer_diff::BufferDiff>,
        worktree_ranges: Vec<std::ops::Range<language::Anchor>>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let git_store = self.git_store.clone();
        cx.spawn(async move |_, cx| {
            git_store.update(cx, |store, cx| {
                store.stage_hunks(buffer, unstaged_diff, worktree_ranges, cx)
            })?;
            Ok(())
        })
    }

    /// §16.6 Delegate git unstage to GitStore.
    pub fn unstage_staged_hunks(
        &mut self,
        staged_diff: gpui::Entity<buffer_diff::BufferDiff>,
        index_ranges: Vec<std::ops::Range<language::Anchor>>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let git_store = self.git_store.clone();
        cx.spawn(async move |_, cx| {
            git_store.update(cx, |store, cx| {
                store.unstage_staged_hunks(staged_diff, index_ranges, cx)
            })?;
            Ok(())
        })
    }

    /// §16.6 Delegate git_init to GitStore.
    pub fn git_init(
        &self,
        path: Arc<std::path::Path>,
        fallback_branch_name: String,
        cx: &mut gpui::Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.git_store
            .read(cx)
            .git_init(path, fallback_branch_name, cx)
    }

    /// §16.6 Delegate git_config to GitStore. Returns the raw stdout string
    /// rather than a parsed HashMap; callers that need key-value parsing
    /// should parse the `-z` delimited output.
    pub fn git_config(
        &self,
        path: Arc<std::path::Path>,
        args: Vec<String>,
        cx: &gpui::App,
    ) -> Task<anyhow::Result<String>> {
        self.git_store.read(cx).git_config(path, args, cx)
    }
}

impl ProjectItem for Buffer {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        Some(project.update(cx, |project, cx| project.open_buffer(path.clone(), cx)))
    }

    fn entry_id(&self, _cx: &App) -> Option<ProjectEntryId> {
        worktree::File::from_dyn(self.file()).and_then(|file| file.project_entry_id())
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        let file = self.file()?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    fn is_dirty(&self) -> bool {
        self.is_dirty()
    }
}

impl From<(WorktreeId, Arc<RelPath>)> for ProjectPath {
    fn from((worktree_id, path): (WorktreeId, Arc<RelPath>)) -> Self {
        Self { worktree_id, path }
    }
}

impl EventEmitter<Event> for Project {}

impl<'a> From<&'a ProjectPath> for SettingsLocation<'a> {
    fn from(val: &'a ProjectPath) -> Self {
        SettingsLocation {
            worktree_id: val.worktree_id,
            path: val.path.as_ref(),
        }
    }
}

// file finder 相关类型 (来源: spec §2.1 — file_finder 保留)
#[derive(Clone)]
pub struct PathMatchCandidateSet {
    pub snapshot: worktree::Snapshot,
    pub include_ignored: bool,
    pub include_root_name: bool,
    pub candidates: Candidates,
}

#[derive(Clone, Copy, Debug)]
pub enum Candidates {
    Entries,
    Directories,
    Files,
}

impl<'a> fuzzy_nucleo::PathMatchCandidateSet<'a> for PathMatchCandidateSet {
    type Candidates = std::iter::Map<
        worktree::Traversal<'a>,
        fn(&'a worktree::Entry) -> fuzzy_nucleo::PathMatchCandidate<'a>,
    >;

    fn id(&self) -> usize {
        self.snapshot.id().to_usize()
    }

    fn len(&self) -> usize {
        match self.candidates {
            Candidates::Files => {
                if self.include_ignored {
                    self.snapshot.file_count()
                } else {
                    self.snapshot.visible_file_count()
                }
            }
            Candidates::Directories => {
                if self.include_ignored {
                    self.snapshot.dir_count()
                } else {
                    self.snapshot.visible_dir_count()
                }
            }
            Candidates::Entries => {
                if self.include_ignored {
                    self.snapshot.entry_count()
                } else {
                    self.snapshot.visible_entry_count()
                }
            }
        }
    }

    fn root_is_file(&self) -> bool {
        self.snapshot
            .root_entry()
            .is_some_and(|entry| !entry.is_dir())
    }

    fn prefix(&self) -> std::sync::Arc<util::rel_path::RelPath> {
        let root_is_file = self
            .snapshot
            .root_entry()
            .is_some_and(|entry| !entry.is_dir());
        if self.include_root_name || root_is_file {
            self.snapshot.root_name().into()
        } else {
            util::rel_path::RelPath::empty_arc()
        }
    }

    fn candidates(&'a self, start: usize) -> Self::Candidates {
        fn to_candidate(entry: &worktree::Entry) -> fuzzy_nucleo::PathMatchCandidate<'_> {
            fuzzy_nucleo::PathMatchCandidate {
                is_dir: entry.is_dir(),
                path: entry.path.as_ref(),
                char_bag: entry.char_bag,
            }
        }

        let traversal = match self.candidates {
            Candidates::Files => self.snapshot.files(self.include_ignored, start),
            Candidates::Directories => self.snapshot.directories(self.include_ignored, start),
            Candidates::Entries => self.snapshot.entries(self.include_ignored, start),
        };
        traversal.map(to_candidate as fn(&worktree::Entry) -> fuzzy_nucleo::PathMatchCandidate<'_>)
    }

    fn path_style(&self) -> util::paths::PathStyle {
        self.snapshot.path_style()
    }
}

// §2.1 ProjectPath 基本不变量测试 — 这些不依赖 LSP/buffer,裁剪后必须仍能跑。
#[cfg(test)]
mod z3rm_path_tests {
    use super::*;

    #[test]
    fn root_path_has_empty_path() {
        let p = ProjectPath::root_path(WorktreeId::from_proto(0));
        assert!(p.path.is_empty(), "root path should be empty");
    }

    #[test]
    fn starts_with_same_worktree_and_prefix_path() {
        let wid = WorktreeId::from_proto(1);
        let root = ProjectPath::root_path(wid);
        let child = ProjectPath {
            worktree_id: wid,
            path: RelPath::new(
                std::path::Path::new("src/foo.rs"),
                util::paths::PathStyle::Posix,
            )
            .unwrap()
            .into_owned()
            .into(),
        };
        assert!(child.starts_with(&root), "child should start_with root");
        assert!(
            !root.starts_with(&child),
            "root should not start_with child"
        );
    }

    #[test]
    fn starts_with_different_worktree_is_false() {
        let w1 = WorktreeId::from_proto(1);
        let w2 = WorktreeId::from_proto(2);
        let p1 = ProjectPath::root_path(w1);
        let p2 = ProjectPath::root_path(w2);
        assert!(
            !p1.starts_with(&p2),
            "different worktree should not start_with"
        );
    }

    #[test]
    fn proto_round_trip_preserves_worktree_id_and_path() {
        let wid = WorktreeId::from_proto(42);
        let original = ProjectPath {
            worktree_id: wid,
            path: RelPath::new(
                std::path::Path::new("a/b.rs"),
                util::paths::PathStyle::Posix,
            )
            .unwrap()
            .into_owned()
            .into(),
        };
        let proto = original.to_proto();
        let recovered = ProjectPath::from_proto(proto).expect("round trip");
        assert_eq!(original, recovered);
    }
    #[test]
    fn prepared_terminal_task_uses_prepared_executable_and_arguments() {
        let task = SpawnInTerminal {
            command: "/bin/bash".to_string(),
            args: vec![
                "-i".to_string(),
                "-c".to_string(),
                "printf task".to_string(),
            ],
            shell: Shell::System,
            ..Default::default()
        };
        let shell = Project::shell_for_terminal_task(&task);
        assert_eq!(
            shell,
            util::shell::Shell::WithArguments {
                program: "/bin/bash".to_string(),
                args: task.args.clone(),
                title_override: None,
            }
        );
    }
}
