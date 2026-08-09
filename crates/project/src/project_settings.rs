use anyhow::Context as _;
use collections::HashMap;
// use context_server::ContextServerCommand;  // removed-crate: context_server
// use dap::adapters::DebugAdapterName;  // removed-crate: dap
use fs::Fs;
use futures::StreamExt as _;
use git::repository::DEFAULT_WORKTREE_DIRECTORY;
use gpui::{AsyncApp, BorrowAppContext, Context, Entity, EventEmitter, Subscription, Task};
use lsp::{DEFAULT_LSP_REQUEST_TIMEOUT_SECS, LanguageServerName};
use paths::{EDITORCONFIG_NAME, local_settings_file_relative_path};
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, REMOTE_SERVER_PROJECT_ID},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
// BinarySettings and DirenvSettings moved to local definitions (spec §16 Plan 16)
pub use settings::LspSettings;
use settings::{
    EditorconfigEvent, InvalidSettingsError, LocalSettingsKind, LocalSettingsPath, RegisterSetting,
    SemanticTokenRules, Settings, SettingsLocation, SettingsStore, parse_json_with_comments,
    watch_config_file,
};
use std::{cell::OnceCell, collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
// use task::{DebugTaskFile, TaskTemplates, VsCodeDebugTaskFile, VsCodeTaskFile};  // removed-crate: task
use util::{ResultExt, rel_path::RelPath, serde::default_true};
use worktree::{PathChange, UpdatedEntriesSet, Worktree, WorktreeId};

use crate::{
    trusted_worktrees::{PathTrust, TrustedWorktrees, TrustedWorktreesEvent},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

#[derive(Debug, Clone, RegisterSetting)]
pub struct ProjectSettings {
    /// Configuration for language servers.
    ///
    /// The following settings can be overridden for specific language servers:
    /// - initialization_options
    ///
    /// To override settings for a language, add an entry for that language server's
    /// name to the lsp value.
    /// Default: null
    // todo(settings-follow-up)
    // We should change to use a non content type (settings::LspSettings is a content type)
    // Note: Will either require merging with defaults, which also requires deciding where the defaults come from,
    //       or case by case deciding which fields are optional and which are actually required.
    pub lsp: HashMap<LanguageServerName, settings::LspSettings>,

    /// Common language server settings.
    pub global_lsp_settings: GlobalLspSettings,

    /// Configuration for Diagnostics-related features.
    pub diagnostics: DiagnosticsSettings,

    /// Configuration for Git-related features
    pub git: GitSettings,

    /// Configuration for Node-related features
    pub node: NodeBinarySettings,

    /// Configuration for how direnv configuration should be loaded
    pub load_direnv: DirenvSettings,

    /// Configuration for session-related features
    pub session: SessionSettings,
}

#[derive(Copy, Clone, Debug)]
pub struct SessionSettings {
    /// Whether or not to restore unsaved buffers on restart.
    ///
    /// If this is true, user won't be prompted whether to save/discard
    /// dirty files when closing the application.
    ///
    /// Default: true
    pub restore_unsaved_buffers: bool,
    /// Whether or not to skip worktree trust checks.
    /// When trusted, project settings are synchronized automatically,
    /// language and MCP servers are downloaded and started automatically.
    ///
    /// Default: false
    pub trust_all_worktrees: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeBinarySettings {
    /// The path to the Node binary.
    pub path: Option<String>,
    /// The path to the npm binary Zed should use (defaults to `.path/../npm`).
    pub npm_path: Option<String>,
    /// If enabled, Zed will download its own copy of Node.
    pub ignore_system_version: bool,
}

/// Common language server settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct GlobalLspSettings {
    /// Whether to show the LSP servers button in the status bar.
    ///
    /// Default: `true`
    pub button: bool,
    /// The maximum amount of time to wait for responses from language servers, in seconds.
    /// A value of `0` will result in no timeout being applied (causing all LSP responses to wait
    /// indefinitely until completed).
    /// This should not be used outside of serialization/de-serialization in favor of get_request_timeout.
    ///
    /// Default: `120`
    pub request_timeout: u64,
    pub notifications: LspNotificationSettings,

    /// Rules for highlighting semantic tokens.
    pub semantic_token_rules: SemanticTokenRules,
}

impl Default for GlobalLspSettings {
    fn default() -> Self {
        Self {
            button: true,
            request_timeout: DEFAULT_LSP_REQUEST_TIMEOUT_SECS,
            notifications: LspNotificationSettings::default(),
            semantic_token_rules: SemanticTokenRules::default(),
        }
    }
}

impl GlobalLspSettings {
    /// Returns the timeout duration for LSP-related interactions, or Duration::ZERO if no timeout should be applied.
    /// Zero durations are treated as no timeout by language servers, so code using this in an async context can
    /// simply call unwrap_or_default.
    pub const fn get_request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout)
    }
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, JsonSchema, Debug)]
#[serde(tag = "source", rename_all = "snake_case")]
pub struct LspNotificationSettings {
    /// Timeout in milliseconds for automatically dismissing language server notifications.
    /// Set to 0 to disable auto-dismiss.
    ///
    /// Default: 5000
    pub dismiss_timeout_ms: Option<u64>,
}

impl Default for LspNotificationSettings {
    fn default() -> Self {
        Self {
            dismiss_timeout_ms: Some(5000),
        }
    }
}

/// 控制 direnv 配置加载方式 (spec §16 Plan 16)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirenvSettings {
    /// 不加载 direnv 配置
    #[default]
    Disabled,
    /// 直接执行 direnv 命令
    Direct,
    /// 使用 shell hook 方式加载
    ShellHook,
}

/// 诊断严重程度内容类型 (spec §16 Plan 16)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverityContent {
    Off,
    Error,
    Warning,
    Info,
    Hint,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DiagnosticSeverity {
    // No diagnostics are shown.
    Off,
    Error,
    Warning,
    Info,
    Hint,
}

impl DiagnosticSeverity {
    pub fn into_lsp(self) -> Option<lsp::DiagnosticSeverity> {
        match self {
            DiagnosticSeverity::Off => None,
            DiagnosticSeverity::Error => Some(lsp::DiagnosticSeverity::ERROR),
            DiagnosticSeverity::Warning => Some(lsp::DiagnosticSeverity::WARNING),
            DiagnosticSeverity::Info => Some(lsp::DiagnosticSeverity::INFORMATION),
            DiagnosticSeverity::Hint => Some(lsp::DiagnosticSeverity::HINT),
        }
    }
}

impl From<DiagnosticSeverityContent> for DiagnosticSeverity {
    fn from(severity: DiagnosticSeverityContent) -> Self {
        match severity {
            DiagnosticSeverityContent::Off => DiagnosticSeverity::Off,
            DiagnosticSeverityContent::Error => DiagnosticSeverity::Error,
            DiagnosticSeverityContent::Warning => DiagnosticSeverity::Warning,
            DiagnosticSeverityContent::Info => DiagnosticSeverity::Info,
            DiagnosticSeverityContent::Hint => DiagnosticSeverity::Hint,
            DiagnosticSeverityContent::All => DiagnosticSeverity::Hint,
        }
    }
}

/// Determines the severity of the diagnostic that should be moved to.
#[derive(PartialEq, PartialOrd, Clone, Copy, Debug, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoToDiagnosticSeverity {
    /// Errors
    Error = 3,
    /// Warnings
    Warning = 2,
    /// Information
    Information = 1,
    /// Hints
    Hint = 0,
}

impl From<lsp::DiagnosticSeverity> for GoToDiagnosticSeverity {
    fn from(severity: lsp::DiagnosticSeverity) -> Self {
        match severity {
            lsp::DiagnosticSeverity::ERROR => Self::Error,
            lsp::DiagnosticSeverity::WARNING => Self::Warning,
            lsp::DiagnosticSeverity::INFORMATION => Self::Information,
            lsp::DiagnosticSeverity::HINT => Self::Hint,
            _ => Self::Error,
        }
    }
}

impl GoToDiagnosticSeverity {
    pub fn min() -> Self {
        Self::Hint
    }

    pub fn max() -> Self {
        Self::Error
    }
}

/// Allows filtering diagnostics that should be moved to.
#[derive(PartialEq, Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum GoToDiagnosticSeverityFilter {
    /// Move to diagnostics of a specific severity.
    Only(GoToDiagnosticSeverity),

    /// Specify a range of severities to include.
    Range {
        /// Minimum severity to move to. Defaults no "error".
        #[serde(default = "GoToDiagnosticSeverity::min")]
        min: GoToDiagnosticSeverity,
        /// Maximum severity to move to. Defaults to "hint".
        #[serde(default = "GoToDiagnosticSeverity::max")]
        max: GoToDiagnosticSeverity,
    },
}

impl Default for GoToDiagnosticSeverityFilter {
    fn default() -> Self {
        Self::Range {
            min: GoToDiagnosticSeverity::min(),
            max: GoToDiagnosticSeverity::max(),
        }
    }
}

impl GoToDiagnosticSeverityFilter {
    pub fn matches(&self, severity: lsp::DiagnosticSeverity) -> bool {
        let severity: GoToDiagnosticSeverity = severity.into();
        match self {
            Self::Only(target) => *target == severity,
            Self::Range { min, max } => severity >= *min && severity <= *max,
        }
    }
}

/// Git gutter 显示模式 (spec §16 Plan 16)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitGutterSetting {
    /// 显示所有文件的 gutter
    AllFiles,
    /// 仅显示已跟踪文件的 gutter
    #[default]
    TrackedFiles,
    /// 不显示 gutter
    Off,
}

/// Git hunk 显示样式 (spec §16 Plan 16)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHunkStyleSetting {
    /// 暂存区显示空心, 未暂存显示实心
    #[default]
    StagedHollow,
    /// 暂存区显示实心, 未暂存显示空心
    StagedSolid,
    /// 两者都显示实心
    Solid,
    /// 两者都显示空心
    Hollow,
}

/// 延迟毫秒数包装类型 (spec §16 Plan 16)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DelayMs(pub u64);

#[derive(Clone, Debug)]
pub struct GitSettings {
    /// Whether or not git integration is enabled.
    ///
    /// Default: true
    pub enabled: GitEnabledSettings,
    /// Whether or not to show the git gutter.
    ///
    /// Default: tracked_files
    pub git_gutter: GitGutterSetting,
    /// Sets the debounce threshold (in milliseconds) after which changes are reflected in the git gutter.
    ///
    /// Default: 0
    pub gutter_debounce: u64,
    /// Whether or not to show git blame data inline in
    /// the currently focused line.
    ///
    /// Default: on
    pub inline_blame: InlineBlameSettings,
    /// Git blame settings.
    pub blame: BlameSettings,
    /// Which information to show in the branch picker.
    ///
    /// Default: on
    pub branch_picker: BranchPickerSettings,
    /// How hunks are displayed visually in the editor.
    ///
    /// Default: staged_hollow
    pub hunk_style: GitHunkStyleSetting,
    /// How file paths are displayed in the git gutter.
    ///
    /// Default: file_name_first
    pub path_style: GitPathStyle,
    /// Whether to show the stage and restore buttons on diff hunks.
    ///
    /// Default: true
    pub show_stage_restore_buttons: bool,
    /// Directory where git worktrees are created, relative to the repository
    /// working directory. When the resolved directory is outside the project
    /// root, the project's directory name is automatically appended so that
    /// sibling repos don't collide.
    ///
    /// Default: ../worktrees
    pub worktree_directory: String,
}

#[derive(Clone, Copy, Debug)]
pub struct GitEnabledSettings {
    /// Whether git integration is enabled for showing git status.
    ///
    /// Default: true
    pub status: bool,
    /// Whether git integration is enabled for showing diffs.
    ///
    /// Default: true
    pub diff: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum GitPathStyle {
    #[default]
    FileNameFirst,
    FilePathFirst,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InlineBlameLocation {
    #[default]
    Inline,
    StatusBar,
}

#[derive(Clone, Copy, Debug)]
pub struct InlineBlameSettings {
    /// Whether or not to show git blame data inline in
    /// the currently focused line.
    ///
    /// Default: true
    pub enabled: bool,
    /// Whether to only show the inline blame information
    /// after a delay once the cursor stops moving.
    ///
    /// Default: 0
    pub delay_ms: DelayMs,
    /// Where to render the blame information when enabled.
    ///
    /// Default: inline
    pub location: InlineBlameLocation,
    /// The amount of padding between the end of the source line and the start
    /// of the inline blame in units of columns.
    ///
    /// Default: 7
    pub padding: u32,
    /// The minimum column number to show the inline blame information at
    ///
    /// Default: 0
    pub min_column: u32,
    /// Whether to show commit summary as part of the inline blame.
    ///
    /// Default: false
    pub show_commit_summary: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct BlameSettings {
    /// Whether to show the avatar of the author of the commit.
    ///
    /// Default: true
    pub show_avatar: bool,
}

impl GitSettings {
    pub fn inline_blame_delay(&self) -> Option<Duration> {
        if self.inline_blame.delay_ms.0 > 0 {
            Some(Duration::from_millis(self.inline_blame.delay_ms.0))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct BranchPickerSettings {
    /// Whether to show author name as part of the commit information.
    ///
    /// Default: false
    #[serde(default)]
    pub show_author_name: bool,
}

impl Default for BranchPickerSettings {
    fn default() -> Self {
        Self {
            show_author_name: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiagnosticsSettings {
    /// Whether to show the project diagnostics button in the status bar.
    pub button: bool,

    /// Whether or not to include warning diagnostics.
    pub include_warnings: bool,

    /// Settings for using LSP pull diagnostics mechanism in Zed.
    pub lsp_pull_diagnostics: LspPullDiagnosticsSettings,

    /// Settings for showing inline diagnostics.
    pub inline: InlineDiagnosticsSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineDiagnosticsSettings {
    /// Whether or not to show inline diagnostics
    ///
    /// Default: false
    pub enabled: bool,
    /// Whether to only show the inline diagnostics after a delay after the
    /// last editor event.
    ///
    /// Default: 150
    pub update_debounce_ms: u64,
    /// The amount of padding between the end of the source line and the start
    /// of the inline diagnostic in units of columns.
    ///
    /// Default: 4
    pub padding: u32,
    /// The minimum column to display inline diagnostics. This setting can be
    /// used to horizontally align inline diagnostics at some position. Lines
    /// longer than this value will still push diagnostics further to the right.
    ///
    /// Default: 0
    pub min_column: u32,

    pub max_severity: Option<DiagnosticSeverity>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct LspPullDiagnosticsSettings {
    /// Whether to pull for diagnostics or not.
    ///
    /// Default: true
    pub enabled: bool,
    /// Minimum time to wait before pulling diagnostics from the language server(s).
    /// 0 turns the debounce off.
    ///
    /// Default: 50
    pub debounce_ms: u64,
}

impl Settings for ProjectSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let diagnostics = content.diagnostics.clone().unwrap_or_default();
        let lsp_pull_diagnostics = diagnostics.lsp_pull_diagnostics.unwrap_or_default();
        let inline_diagnostics = diagnostics.inline.unwrap_or_default();
        let global_lsp_settings = content.global_lsp_settings.unwrap_or_default();
        let lsp_notifications = global_lsp_settings.notifications.unwrap_or_default();
        let session = content.session.unwrap_or_default();

        Self {
            // `settings::LspSettings` currently carries no fields and no reader,
            // so there is nothing for a user to configure per language server.
            lsp: HashMap::default(),
            global_lsp_settings: GlobalLspSettings {
                button: true,
                request_timeout: DEFAULT_LSP_REQUEST_TIMEOUT_SECS,
                notifications: LspNotificationSettings {
                    dismiss_timeout_ms: Some(lsp_notifications.dismiss_timeout_ms.unwrap_or(5000)),
                },
                semantic_token_rules: settings::SemanticTokenRules::default(),
            },
            diagnostics: DiagnosticsSettings {
                button: diagnostics.button.unwrap_or(true),
                include_warnings: diagnostics.include_warnings.unwrap_or(true),
                lsp_pull_diagnostics: LspPullDiagnosticsSettings {
                    enabled: lsp_pull_diagnostics.enabled.unwrap_or(true),
                    debounce_ms: lsp_pull_diagnostics.debounce_ms.unwrap_or(50),
                },
                inline: InlineDiagnosticsSettings {
                    enabled: inline_diagnostics.enabled.unwrap_or(false),
                    update_debounce_ms: inline_diagnostics.update_debounce_ms.unwrap_or(150),
                    padding: inline_diagnostics.padding.unwrap_or(4),
                    min_column: inline_diagnostics.min_column.unwrap_or(0),
                    max_severity: inline_diagnostics
                        .max_severity
                        .map(|severity| match severity {
                            settings::DiagnosticSeverityContent::Off => DiagnosticSeverity::Off,
                            settings::DiagnosticSeverityContent::Error => DiagnosticSeverity::Error,
                            settings::DiagnosticSeverityContent::Warning => {
                                DiagnosticSeverity::Warning
                            }
                            settings::DiagnosticSeverityContent::Info => DiagnosticSeverity::Info,
                            settings::DiagnosticSeverityContent::Hint
                            | settings::DiagnosticSeverityContent::All => DiagnosticSeverity::Hint,
                        }),
                },
            },
            git: GitSettings {
                enabled: GitEnabledSettings {
                    status: true,
                    diff: true,
                },
                git_gutter: GitGutterSetting::TrackedFiles,
                gutter_debounce: 0,
                inline_blame: InlineBlameSettings {
                    enabled: false,
                    delay_ms: DelayMs(0),
                    location: InlineBlameLocation::Inline,
                    padding: 0,
                    min_column: 0,
                    show_commit_summary: false,
                },
                blame: BlameSettings { show_avatar: false },
                branch_picker: BranchPickerSettings {
                    show_author_name: true,
                },
                hunk_style: GitHunkStyleSetting::StagedHollow,
                path_style: GitPathStyle::FileNameFirst,
                show_stage_restore_buttons: true,
                worktree_directory: DEFAULT_WORKTREE_DIRECTORY.to_string(),
            },
            // z3rm bundles no Node runtime, so nothing reads these paths yet.
            node: NodeBinarySettings::default(),
            load_direnv: content
                .load_direnv
                .map_or_else(DirenvSettings::default, |load_direnv| match load_direnv {
                    settings::LoadDirenv::Disabled => DirenvSettings::Disabled,
                    settings::LoadDirenv::Direct => DirenvSettings::Direct,
                    settings::LoadDirenv::ShellHook => DirenvSettings::ShellHook,
                }),
            session: SessionSettings {
                restore_unsaved_buffers: session.restore_unsaved_buffers.unwrap_or(true),
                trust_all_worktrees: session.trust_all_worktrees.unwrap_or(false),
            },
        }
    }
}

pub enum SettingsObserverMode {
    Local(Arc<dyn Fs>),
    Remote { via_collab: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingsObserverEvent {
    LocalSettingsUpdated(Result<PathBuf, InvalidSettingsError>),
}

impl EventEmitter<SettingsObserverEvent> for SettingsObserver {}

pub struct SettingsObserver {
    mode: SettingsObserverMode,
    downstream_client: Option<AnyProtoClient>,
    worktree_store: Entity<WorktreeStore>,
    project_id: u64,
    pending_local_settings:
        HashMap<PathTrust, BTreeMap<(WorktreeId, Arc<RelPath>), Option<String>>>,
    _trusted_worktrees_watcher: Option<Subscription>,
    _user_settings_watcher: Option<Subscription>,
    _editorconfig_watcher: Option<Subscription>,
    _global_task_config_watcher: Task<()>,
    _global_debug_config_watcher: Task<()>,
}

/// SettingsObserver observers changes to .zed/{settings, task}.json files in local worktrees
/// (or the equivalent protobuf messages from upstream) and updates local settings
/// and sends notifications downstream.
/// In ssh mode it also monitors ~/.config/zed/{settings, task}.json and sends the content
/// upstream.
impl SettingsObserver {
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_message_handler(Self::handle_update_worktree_settings);
        client.add_entity_message_handler(Self::handle_update_user_settings);
    }

    pub fn new_local(
        fs: Arc<dyn Fs>,
        worktree_store: Entity<WorktreeStore>,
        watch_global_configs: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&worktree_store, Self::on_worktree_store_event)
            .detach();

        let _trusted_worktrees_watcher =
            TrustedWorktrees::try_get_global(cx).map(|trusted_worktrees| {
                cx.subscribe(
                    &trusted_worktrees,
                    move |settings_observer, _, e, cx| match e {
                        TrustedWorktreesEvent::Trusted(_, trusted_paths) => {
                            for trusted_path in trusted_paths {
                                if let Some(pending_local_settings) = settings_observer
                                    .pending_local_settings
                                    .remove(trusted_path)
                                {
                                    for ((worktree_id, directory_path), settings_contents) in
                                        pending_local_settings
                                    {
                                        let path =
                                            LocalSettingsPath::InWorktree(directory_path.clone());
                                        apply_local_settings(
                                            worktree_id,
                                            path.clone(),
                                            LocalSettingsKind::Settings,
                                            &settings_contents,
                                            cx,
                                        );
                                        if let Some(downstream_client) =
                                            &settings_observer.downstream_client
                                        {
                                            downstream_client
                                                .send(proto::UpdateWorktreeSettings {
                                                    project_id: settings_observer.project_id,
                                                    worktree_id: worktree_id.to_proto(),
                                                    path: path.to_proto(),
                                                    content: settings_contents,
                                                    kind: Some(
                                                        local_settings_kind_to_proto(
                                                            LocalSettingsKind::Settings,
                                                        )
                                                        .into(),
                                                    ),
                                                    outside_worktree: Some(false),
                                                })
                                                .log_err();
                                        }
                                    }
                                }
                            }
                        }
                        TrustedWorktreesEvent::Restricted(..) => {}
                    },
                )
            });

        let editorconfig_store = cx.global::<SettingsStore>().editorconfig_store.clone();
        let _editorconfig_watcher = cx.subscribe(
            &editorconfig_store,
            |this, _, event: &EditorconfigEvent, cx| {
                let EditorconfigEvent::ExternalConfigChanged {
                    path,
                    content,
                    affected_worktree_ids,
                } = event;
                for worktree_id in affected_worktree_ids {
                    if let Some(worktree) = this
                        .worktree_store
                        .read(cx)
                        .worktree_for_id(*worktree_id, cx)
                    {
                        this.update_settings(
                            worktree,
                            [(
                                path.clone(),
                                LocalSettingsKind::Editorconfig,
                                content.clone(),
                            )],
                            false,
                            cx,
                        );
                    }
                }
            },
        );

        Self {
            worktree_store,
            mode: SettingsObserverMode::Local(fs.clone()),
            downstream_client: None,
            _trusted_worktrees_watcher,
            pending_local_settings: HashMap::default(),
            _user_settings_watcher: None,
            _editorconfig_watcher: Some(_editorconfig_watcher),
            project_id: REMOTE_SERVER_PROJECT_ID,
            _global_task_config_watcher: Task::ready(()),
            _global_debug_config_watcher: Task::ready(()),
        }
    }

    pub fn new_remote(
        fs: Arc<dyn Fs>,
        worktree_store: Entity<WorktreeStore>,
        upstream_client: Option<AnyProtoClient>,
        via_collab: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut user_settings_watcher = None;
        if cx.try_global::<SettingsStore>().is_some() {
            if let Some(upstream_client) = upstream_client {
                let mut user_settings = None;
                user_settings_watcher = Some(cx.observe_global::<SettingsStore>(move |_, cx| {
                    if let Some(new_settings) = cx.global::<SettingsStore>().raw_user_settings() {
                        if Some(new_settings) != user_settings.as_ref() {
                            if let Some(new_settings_string) =
                                serde_json::to_string(new_settings).ok()
                            {
                                user_settings = Some(new_settings.clone());
                                upstream_client
                                    .send(proto::UpdateUserSettings {
                                        project_id: REMOTE_SERVER_PROJECT_ID,
                                        contents: new_settings_string,
                                    })
                                    .log_err();
                            }
                        }
                    }
                }));
            }
        };

        Self {
            worktree_store,
            mode: SettingsObserverMode::Remote { via_collab },
            downstream_client: None,
            project_id: REMOTE_SERVER_PROJECT_ID,
            _trusted_worktrees_watcher: None,
            pending_local_settings: HashMap::default(),
            _user_settings_watcher: user_settings_watcher,
            _editorconfig_watcher: None,
            _global_task_config_watcher: Task::ready(()),
            _global_debug_config_watcher: Task::ready(()),
        }
    }

    pub fn shared(
        &mut self,
        project_id: u64,
        downstream_client: AnyProtoClient,
        cx: &mut Context<Self>,
    ) {
        self.project_id = project_id;
        self.downstream_client = Some(downstream_client.clone());

        let store = cx.global::<SettingsStore>();
        for worktree in self.worktree_store.read(cx).worktrees() {
            let worktree_id = worktree.read(cx).id().to_proto();
            for (path, content) in store.local_settings(worktree.read(cx).id()) {
                let content = serde_json::to_string(&content).unwrap();
                downstream_client
                    .send(proto::UpdateWorktreeSettings {
                        project_id,
                        worktree_id,
                        path: path.to_proto(),
                        content: Some(content),
                        kind: Some(
                            local_settings_kind_to_proto(LocalSettingsKind::Settings).into(),
                        ),
                        outside_worktree: Some(false),
                    })
                    .log_err();
            }
            for (path, content, _) in store
                .editorconfig_store
                .read(cx)
                .local_editorconfig_settings(worktree.read(cx).id())
            {
                downstream_client
                    .send(proto::UpdateWorktreeSettings {
                        project_id,
                        worktree_id,
                        path: path.to_proto(),
                        content: Some(content.to_owned()),
                        kind: Some(
                            local_settings_kind_to_proto(LocalSettingsKind::Editorconfig).into(),
                        ),
                        outside_worktree: Some(path.is_outside_worktree()),
                    })
                    .log_err();
            }
        }
    }

    pub fn unshared(&mut self, _: &mut Context<Self>) {
        self.downstream_client = None;
    }

    async fn handle_update_worktree_settings(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::UpdateWorktreeSettings>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<()> {
        let kind = match envelope.payload.kind {
            Some(kind) => proto::LocalSettingsKind::from_i32(kind)
                .with_context(|| format!("unknown kind {kind}"))?,
            None => proto::LocalSettingsKind::Settings,
        };

        let path = LocalSettingsPath::from_proto(
            &envelope.payload.path,
            envelope.payload.outside_worktree.unwrap_or(false),
        )?;

        this.update(&mut cx, |this, cx| {
            let is_via_collab = match &this.mode {
                SettingsObserverMode::Local(..) => false,
                SettingsObserverMode::Remote { via_collab } => *via_collab,
            };
            let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
            let Some(worktree) = this
                .worktree_store
                .read(cx)
                .worktree_for_id(worktree_id, cx)
            else {
                return;
            };

            this.update_settings(
                worktree,
                [(
                    path,
                    local_settings_kind_from_proto(kind),
                    envelope.payload.content,
                )],
                is_via_collab,
                cx,
            );
        });
        Ok(())
    }

    async fn handle_update_user_settings(
        _: Entity<Self>,
        envelope: TypedEnvelope<proto::UpdateUserSettings>,
        cx: AsyncApp,
    ) -> anyhow::Result<()> {
        cx.update_global(|settings_store: &mut SettingsStore, cx| {
            settings_store
                .set_user_settings(&envelope.payload.contents, cx)
                .result()
                .context("setting new user settings")?;
            anyhow::Ok(())
        })?;
        Ok(())
    }

    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeStoreEvent::WorktreeAdded(worktree) => cx
                .subscribe(worktree, |this, worktree, event, cx| {
                    if let worktree::Event::UpdatedEntries(changes) = event {
                        this.update_local_worktree_settings(&worktree, changes, cx)
                    }
                })
                .detach(),
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.clear_local_settings(*worktree_id, cx).log_err();
                });
            }
            _ => {}
        }
    }

    fn update_local_worktree_settings(
        &mut self,
        worktree: &Entity<Worktree>,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        let SettingsObserverMode::Local(fs) = &self.mode else {
            return;
        };

        let mut settings_contents = Vec::new();
        for (path, _, change) in changes.iter() {
            let (settings_dir, kind) = if path.ends_with(local_settings_file_relative_path()) {
                let settings_dir = path
                    .ancestors()
                    .nth(local_settings_file_relative_path().components().count())
                    .unwrap()
                    .into();
                (settings_dir, LocalSettingsKind::Settings)
            } else if path.ends_with(RelPath::unix(EDITORCONFIG_NAME).unwrap()) {
                let Some(settings_dir) = path.parent().map(Arc::from) else {
                    continue;
                };
                if matches!(change, PathChange::Loaded) || matches!(change, PathChange::Added) {
                    let worktree_id = worktree.read(cx).id();
                    let worktree_path = worktree.read(cx).abs_path();
                    let fs = fs.clone();
                    cx.update_global::<SettingsStore, _>(|store, cx| {
                        store
                            .editorconfig_store
                            .update(cx, |editorconfig_store, cx| {
                                editorconfig_store.discover_local_external_configs_chain(
                                    worktree_id,
                                    worktree_path,
                                    fs,
                                    cx,
                                );
                            });
                    });
                }
                (settings_dir, LocalSettingsKind::Editorconfig)
            } else {
                continue;
            };

            let removed = change == &PathChange::Removed;
            let fs = fs.clone();
            let abs_path = worktree.read(cx).absolutize(path);
            settings_contents.push(async move {
                (
                    settings_dir,
                    kind,
                    if removed {
                        None
                    } else {
                        Some(async move { fs.load(&abs_path).await }.await)
                    },
                )
            });
        }

        if settings_contents.is_empty() {
            return;
        }

        let worktree = worktree.clone();
        cx.spawn(async move |this, cx| {
            let settings_contents: Vec<(Arc<RelPath>, _, _)> =
                futures::future::join_all(settings_contents).await;
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.update_settings(
                        worktree,
                        settings_contents.into_iter().map(|(path, kind, content)| {
                            (
                                LocalSettingsPath::InWorktree(path),
                                kind,
                                content.and_then(|c| c.log_err()),
                            )
                        }),
                        false,
                        cx,
                    )
                })
            })
        })
        .detach();
    }

    fn update_settings(
        &mut self,
        worktree: Entity<Worktree>,
        settings_contents: impl IntoIterator<
            Item = (LocalSettingsPath, LocalSettingsKind, Option<String>),
        >,
        is_via_collab: bool,
        cx: &mut Context<Self>,
    ) {
        let worktree_id = worktree.read(cx).id();
        let remote_worktree_id = worktree.read(cx).id();
        let can_trust_worktree = if is_via_collab {
            OnceCell::from(true)
        } else {
            OnceCell::new()
        };
        for (directory_path, kind, file_content) in settings_contents {
            let mut applied = true;
            match (&directory_path, kind) {
                (LocalSettingsPath::InWorktree(directory), LocalSettingsKind::Settings) => {
                    if *can_trust_worktree.get_or_init(|| {
                        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
                            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                                trusted_worktrees.can_trust(&self.worktree_store, worktree_id, cx)
                            })
                        } else {
                            true
                        }
                    }) {
                        apply_local_settings(
                            worktree_id,
                            LocalSettingsPath::InWorktree(directory.clone()),
                            kind,
                            &file_content,
                            cx,
                        )
                    } else {
                        applied = false;
                        self.pending_local_settings
                            .entry(PathTrust::Worktree(worktree_id))
                            .or_default()
                            .insert((worktree_id, directory.clone()), file_content.clone());
                    }
                }
                (directory, LocalSettingsKind::Editorconfig) => {
                    apply_local_settings(worktree_id, directory.clone(), kind, &file_content, cx);
                }
                (LocalSettingsPath::OutsideWorktree(path), kind) => {
                    log::error!(
                        "OutsideWorktree path {:?} with kind {:?} is only supported by editorconfig",
                        path,
                        kind
                    );
                    continue;
                }
                _ => continue,
            };

            if applied {
                if let Some(downstream_client) = &self.downstream_client {
                    downstream_client
                        .send(proto::UpdateWorktreeSettings {
                            project_id: self.project_id,
                            worktree_id: remote_worktree_id.to_proto(),
                            path: directory_path.to_proto(),
                            content: file_content.clone(),
                            kind: Some(local_settings_kind_to_proto(kind).into()),
                            outside_worktree: Some(directory_path.is_outside_worktree()),
                        })
                        .log_err();
                }
            }
        }
    }
}

fn apply_local_settings(
    worktree_id: WorktreeId,
    path: LocalSettingsPath,
    kind: LocalSettingsKind,
    file_content: &Option<String>,
    cx: &mut Context<'_, SettingsObserver>,
) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        let result =
            store.set_local_settings(worktree_id, path.clone(), kind, file_content.as_deref(), cx);

        match result {
            Err(InvalidSettingsError::LocalSettings { path, message }) => {
                log::error!("Failed to set local settings in {path:?}: {message}");
                cx.emit(SettingsObserverEvent::LocalSettingsUpdated(Err(
                    InvalidSettingsError::LocalSettings { path, message },
                )));
            }
            Err(e) => log::error!("Failed to set local settings: {e}"),
            Ok(()) => {
                let settings_path = match &path {
                    LocalSettingsPath::InWorktree(rel_path) => rel_path
                        .as_std_path()
                        .join(local_settings_file_relative_path().as_std_path()),
                    LocalSettingsPath::OutsideWorktree(abs_path) => abs_path.to_path_buf(),
                };
                cx.emit(SettingsObserverEvent::LocalSettingsUpdated(Ok(
                    settings_path,
                )))
            }
        }
    })
}

pub fn local_settings_kind_from_proto(kind: proto::LocalSettingsKind) -> LocalSettingsKind {
    match kind {
        proto::LocalSettingsKind::Settings => LocalSettingsKind::Settings,
        proto::LocalSettingsKind::Tasks => LocalSettingsKind::Tasks,
        proto::LocalSettingsKind::Editorconfig => LocalSettingsKind::Editorconfig,
        proto::LocalSettingsKind::Debug => LocalSettingsKind::Debug,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::{
        DiagnosticsSettingsContent, GlobalLspSettingsContent, InlineDiagnosticsSettingsContent,
        LspNotificationSettingsContent, LspPullDiagnosticsSettingsContent, SessionSettingsContent,
        SettingsContent,
    };

    fn content_with_project_settings() -> SettingsContent {
        let mut content = SettingsContent::default();
        content.diagnostics = Some(DiagnosticsSettingsContent {
            button: Some(false),
            include_warnings: Some(false),
            lsp_pull_diagnostics: Some(LspPullDiagnosticsSettingsContent {
                enabled: Some(false),
                debounce_ms: Some(750),
            }),
            inline: Some(InlineDiagnosticsSettingsContent {
                enabled: Some(true),
                update_debounce_ms: Some(900),
                padding: Some(12),
                max_severity: Some(settings::DiagnosticSeverityContent::Warning),
                ..Default::default()
            }),
        });
        content.session = Some(SessionSettingsContent {
            restore_unsaved_buffers: Some(false),
            trust_all_worktrees: Some(true),
        });
        content.global_lsp_settings = Some(GlobalLspSettingsContent {
            notifications: Some(LspNotificationSettingsContent {
                dismiss_timeout_ms: Some(0),
            }),
        });
        content.load_direnv = Some(settings::LoadDirenv::ShellHook);
        content
    }

    #[test]
    fn test_reads_top_level_project_settings_from_content() {
        let settings = ProjectSettings::from_settings(&content_with_project_settings());

        assert!(!settings.diagnostics.button);
        assert!(!settings.diagnostics.include_warnings);
        assert!(!settings.session.restore_unsaved_buffers);
        assert!(settings.session.trust_all_worktrees);
        assert_eq!(settings.load_direnv, DirenvSettings::ShellHook);
        assert_eq!(
            settings
                .global_lsp_settings
                .notifications
                .dismiss_timeout_ms,
            Some(0)
        );
    }

    #[test]
    fn test_reads_nested_diagnostics_settings_from_content() {
        let settings = ProjectSettings::from_settings(&content_with_project_settings());

        assert_eq!(
            settings.diagnostics.lsp_pull_diagnostics,
            LspPullDiagnosticsSettings {
                enabled: false,
                debounce_ms: 750,
            }
        );
        assert_eq!(
            settings.diagnostics.inline,
            InlineDiagnosticsSettings {
                enabled: true,
                update_debounce_ms: 900,
                padding: 12,
                // Sibling fields the user did not mention keep their own defaults.
                min_column: 0,
                max_severity: Some(DiagnosticSeverity::Warning),
            }
        );
    }

    #[test]
    fn test_falls_back_to_defaults_when_unset() {
        let settings = ProjectSettings::from_settings(&SettingsContent::default());

        assert!(settings.diagnostics.button);
        assert!(settings.diagnostics.include_warnings);
        assert_eq!(
            settings.diagnostics.lsp_pull_diagnostics,
            LspPullDiagnosticsSettings {
                enabled: true,
                debounce_ms: 50,
            }
        );
        assert_eq!(
            settings.diagnostics.inline,
            InlineDiagnosticsSettings {
                enabled: false,
                update_debounce_ms: 150,
                padding: 4,
                min_column: 0,
                max_severity: None,
            }
        );
        assert!(settings.session.restore_unsaved_buffers);
        assert!(!settings.session.trust_all_worktrees);
        assert_eq!(settings.load_direnv, DirenvSettings::Disabled);
        assert_eq!(
            settings
                .global_lsp_settings
                .notifications
                .dismiss_timeout_ms,
            Some(5000)
        );
    }

    #[test]
    fn test_parses_project_settings_from_json() {
        let content: SettingsContent = parse_json_with_comments(
            r#"{
                "diagnostics": {
                    "include_warnings": false,
                    "inline": { "enabled": true, "min_column": 40 }
                },
                "session": { "trust_all_worktrees": true },
                "load_direnv": "direct"
            }"#,
        )
        .expect("project settings should parse");

        let settings = ProjectSettings::from_settings(&content);

        assert!(!settings.diagnostics.include_warnings);
        assert!(settings.diagnostics.inline.enabled);
        assert_eq!(settings.diagnostics.inline.min_column, 40);
        // Sibling keys keep their fallbacks rather than being reset.
        assert_eq!(settings.diagnostics.inline.padding, 4);
        assert!(settings.diagnostics.button);
        assert!(settings.session.trust_all_worktrees);
        assert!(settings.session.restore_unsaved_buffers);
        assert_eq!(settings.load_direnv, DirenvSettings::Direct);
    }

    /// Guards the JSON shape written in `assets/settings/default.json`: every
    /// key there has to deserialize, and the values have to agree with the
    /// fallbacks `from_settings` applies when the key is absent.
    ///
    /// Parsed through `UserSettingsContent` rather than `SettingsContent` so the
    /// test walks the same flattening that `SettingsStore` uses at startup.
    #[test]
    fn test_default_json_project_sections_match_fallbacks() {
        let user_content = <settings::UserSettingsContent as settings::RootUserSettings>::parse_json_with_comments(
            settings::default_settings().as_ref(),
        )
        .expect("assets/settings/default.json should parse");
        let content = *user_content.content;
        let diagnostics = content
            .diagnostics
            .as_ref()
            .expect("assets/settings/default.json should define a `diagnostics` section");

        let from_default_json = ProjectSettings::from_settings(&content);
        let from_fallbacks = ProjectSettings::from_settings(&SettingsContent::default());

        assert_eq!(
            from_default_json.diagnostics.button, from_fallbacks.diagnostics.button,
            "diagnostics.button in default.json disagrees with the Rust fallback"
        );
        assert_eq!(
            from_default_json.diagnostics.include_warnings,
            from_fallbacks.diagnostics.include_warnings
        );
        assert_eq!(
            from_default_json.diagnostics.lsp_pull_diagnostics,
            from_fallbacks.diagnostics.lsp_pull_diagnostics
        );
        assert_eq!(
            from_default_json.diagnostics.inline,
            from_fallbacks.diagnostics.inline
        );
        assert_eq!(
            from_default_json.session.restore_unsaved_buffers,
            from_fallbacks.session.restore_unsaved_buffers
        );
        assert_eq!(
            from_default_json.session.trust_all_worktrees,
            from_fallbacks.session.trust_all_worktrees
        );
        assert_eq!(from_default_json.load_direnv, from_fallbacks.load_direnv);
        assert_eq!(
            from_default_json
                .global_lsp_settings
                .notifications
                .dismiss_timeout_ms,
            from_fallbacks
                .global_lsp_settings
                .notifications
                .dismiss_timeout_ms
        );

        // A misspelled key deserializes to `None`, so spot-check that the
        // nested sections really were populated rather than skipped.
        assert!(diagnostics.inline.is_some());
        assert!(diagnostics.lsp_pull_diagnostics.is_some());
        assert!(content.session.is_some());
        assert!(content.global_lsp_settings.is_some());
        assert!(content.load_direnv.is_some());
    }
}

pub fn local_settings_kind_to_proto(kind: LocalSettingsKind) -> proto::LocalSettingsKind {
    match kind {
        LocalSettingsKind::Settings => proto::LocalSettingsKind::Settings,
        LocalSettingsKind::Tasks => proto::LocalSettingsKind::Tasks,
        LocalSettingsKind::Editorconfig => proto::LocalSettingsKind::Editorconfig,
        LocalSettingsKind::Debug => proto::LocalSettingsKind::Debug,
    }
}
