mod action;
mod editor;
mod extension;
mod fallible_options;
pub mod merge_from;
mod mux;
mod project;
mod project_panel;
mod serde_helper;
mod shadow_snapshot;
mod terminal;
mod theme;
mod title_bar;
mod workspace;

pub use action::{ActionName, ActionWithArguments, CommandAliasTarget};
pub use editor::*;
pub use extension::*;
pub use fallible_options::*;
pub use merge_from::MergeFrom as MergeFromTrait;
pub use mux::*;
pub use project::*;
pub use project_panel::*;
use serde::de::DeserializeOwned;
pub use serde_helper::{
    serialize_f32_with_two_decimal_places, serialize_optional_f32_with_two_decimal_places,
};
use settings_json::parse_json_with_comments;
pub use shadow_snapshot::*;
pub use terminal::*;
pub use theme::*;
pub use title_bar::*;
pub use workspace::*;

use collections::{HashMap, IndexMap};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};
use std::{num::NonZeroU32, sync::Arc};

/// 定义设置覆盖结构体，每个字段为 `Option<Box<SettingsContent>>`，
/// 同时生成 `OVERRIDE_KEYS` 和 `get_by_key` 方法。
macro_rules! settings_overrides {
    (
        $(#[$attr:meta])*
        pub struct $name:ident { $($field:ident),* $(,)? }
    ) => {
        $(#[$attr])*
        pub struct $name {
            $(pub $field: Option<Box<SettingsContent>>,)*
        }

        impl $name {
            /// JSON 覆盖键名，从此结构体的字段名派生。
            pub const OVERRIDE_KEYS: &[&str] = &[$(stringify!($field)),*];

            /// 通过 JSON 键名查找覆盖设置。
            pub fn get_by_key(&self, key: &str) -> Option<&SettingsContent> {
                match key {
                    $(stringify!($field) => self.$field.as_deref(),)*
                    _ => None,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseStatus {
    /// 设置解析成功
    Success,
    /// 设置文件未变更，跳过解析
    Unchanged,
    /// 设置解析失败
    Failed { error: String },
}

/// 键盘输入时隐藏鼠标的时机 (spec §16 Plan 16)
/// 默认: on_typing_and_action
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum HideMouseMode {
    /// 不隐藏鼠标
    Never,
    /// 仅在打字时隐藏
    OnTyping,
    /// 打字和执行操作时隐藏
    #[default]
    OnTypingAndAction,
}

/// 终端/多路复用器/UI chrome 设置结构体 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Debug, PartialEq, Default, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct SettingsContent {
    #[serde(flatten)]
    pub project: ProjectSettingsContent,

    #[serde(flatten)]
    pub theme: Box<ThemeSettingsContent>,

    /// 扩展设置 (spec §16 Plan 16)
    pub extension: ExtensionSettingsContent,

    #[serde(flatten)]
    pub workspace: WorkspaceSettingsContent,

    /// 远程连接设置 (spec §16 Plan 16)
    #[serde(flatten)]
    pub remote: RemoteSettingsContent,

    /// 编辑器与搜索设置 (spec §16 Plan 16)
    pub editor: Option<EditorSettingsContent>,

    /// 终端设置 (spec §16 Plan 16)
    pub terminal: Option<TerminalSettingsContent>,

    /// 多路复用器设置 (spec §16 Plan 16)
    pub mux: Option<MuxSettingsContent>,

    /// 影子快照设置 (spec §16 Plan 16)
    pub shadow_snapshot: Option<ShadowSnapshotSettingsContent>,

    /// 标题栏设置 (spec §16 Plan 16)
    pub title_bar: Option<TitleBarSettingsContent>,

    /// Tab 栏设置 (spec §16 Plan 16)
    pub tab_bar: Option<TabBarSettingsContent>,

    /// 单个 Tab 项设置 (spec §16 Plan 16)
    pub tabs: Option<ItemSettingsContent>,

    /// 预览 Tab 设置 (spec §16 Plan 16)
    pub preview_tabs: Option<PreviewTabsSettingsContent>,

    /// Which-key 弹窗设置
    pub which_key: Option<WhichKeySettingsContent>,

    /// 状态栏设置 (spec §16 Plan 16)
    pub status_bar: Option<StatusBarSettingsContent>,

    /// 基础键盘映射方案
    /// 默认: VSCode
    pub base_keymap: Option<BaseKeymapContent>,

    /// 鼠标隐藏模式
    pub hide_mouse: Option<HideMouseMode>,

    /// 自动更新
    pub auto_update: Option<bool>,

    /// 遥测设置
    pub telemetry: Option<TelemetrySettingsContent>,

    /// 日志范围到级别的映射
    pub log: Option<HashMap<String, String>>,

    /// 功能标志本地覆盖
    pub feature_flags: Option<FeatureFlagsMap>,

    /// Vim 模式开关 (spec §16 Plan 16)
    pub vim_mode: Option<bool>,

    /// 行号指示器格式 (spec §16 Plan 16)
    pub line_indicator_format: Option<LineIndicatorFormat>,

    /// 诊断设置 (spec §16 Plan 16)
    pub diagnostics: Option<DiagnosticsSettingsContent>,

    /// 文件查找器设置 (spec §16 Plan 16)
    pub file_finder: Option<FileFinderSettingsContent>,

    /// 项目面板设置 (spec §16 Plan 16)
    pub project_panel: Option<ProjectPanelSettingsContent>,

    /// 会话恢复设置 (spec §16 Plan 16)
    pub session: Option<SessionSettingsContent>,

    /// 通用语言服务器设置 (spec §16 Plan 16)
    pub global_lsp_settings: Option<GlobalLspSettingsContent>,

    /// direnv 配置加载方式 (spec §16 Plan 16)
    pub load_direnv: Option<LoadDirenv>,
}

/// 诊断设置内容 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct DiagnosticsSettingsContent {
    /// 是否在状态栏显示诊断按钮
    ///
    /// Default: true
    pub button: Option<bool>,

    /// 是否在诊断视图中包含警告级别的诊断
    ///
    /// Default: true
    pub include_warnings: Option<bool>,

    /// LSP 拉取式诊断设置
    pub lsp_pull_diagnostics: Option<LspPullDiagnosticsSettingsContent>,

    /// 内联诊断设置
    pub inline: Option<InlineDiagnosticsSettingsContent>,
}

/// LSP 拉取式诊断设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(default)]
pub struct LspPullDiagnosticsSettingsContent {
    /// 是否向语言服务器拉取诊断
    ///
    /// Default: true
    pub enabled: Option<bool>,

    /// 拉取诊断前的最小等待时间, 0 表示关闭防抖
    ///
    /// Default: 50
    pub debounce_ms: Option<u64>,
}

/// 内联诊断设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(default)]
pub struct InlineDiagnosticsSettingsContent {
    /// 是否显示内联诊断
    ///
    /// Default: false
    pub enabled: Option<bool>,

    /// 编辑器最后一次事件之后、显示内联诊断之前的等待时间
    ///
    /// Default: 150
    pub update_debounce_ms: Option<u64>,

    /// 源代码行尾与内联诊断之间的间距, 以列为单位
    ///
    /// Default: 4
    pub padding: Option<u32>,

    /// 显示内联诊断的最小列号, 用于横向对齐。超过该列的行仍会把诊断继续右推。
    ///
    /// Default: 0
    pub min_column: Option<u32>,

    /// 内联显示的最高诊断级别, null 表示跟随编辑器的 `diagnostics_max_severity`
    ///
    /// Default: null
    pub max_severity: Option<DiagnosticSeverityContent>,
}

/// 会话恢复设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(default)]
pub struct SessionSettingsContent {
    /// 重启后是否恢复未保存的缓冲区。为 true 时关闭应用不再询问是否保存。
    ///
    /// Default: true
    pub restore_unsaved_buffers: Option<bool>,

    /// 是否跳过 worktree 信任检查。信任后项目设置会自动同步, 语言服务器与 MCP
    /// 服务器会自动下载并启动。
    ///
    /// Default: false
    pub trust_all_worktrees: Option<bool>,
}

/// 通用语言服务器设置 (spec §16 Plan 16)
///
/// 仅包含 z3rm 中仍有消费方的字段; 其余 `GlobalLspSettings` 字段随 LSP 子系统一起
/// 被裁剪, 暂不暴露为用户设置。
#[with_fallible_options]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(default)]
pub struct GlobalLspSettingsContent {
    /// 语言服务器通知设置
    pub notifications: Option<LspNotificationSettingsContent>,
}

/// 语言服务器通知设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(default)]
pub struct LspNotificationSettingsContent {
    /// 自动消除语言服务器通知的超时毫秒数, 0 表示不自动消除
    ///
    /// Default: 5000
    pub dismiss_timeout_ms: Option<u64>,
}

/// direnv 配置加载方式 (spec §16 Plan 16)
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum LoadDirenv {
    /// 不加载 direnv 配置
    #[default]
    Disabled,
    /// 直接执行 direnv 命令
    Direct,
    /// 使用 shell hook 方式加载
    ShellHook,
}

/// Which-key 弹窗设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct WhichKeySettingsContent {
    /// 按住键组合时是否显示 which-key 弹窗
    ///
    /// Default: false
    pub enabled: Option<bool>,

    /// 显示 which-key 弹窗前的延迟毫秒数
    ///
    /// Default: 1000
    pub delay_ms: Option<u64>,
}

/// 文件查找器宽度 (spec §16 Plan 16)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum FileFinderWidthContent {
    #[default]
    Small,
    Medium,
    Large,
    XLarge,
    Full,
}

/// 文件查找器设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct FileFinderSettingsContent {
    pub file_icons: bool,
    pub modal_max_width: Option<FileFinderWidthContent>,
    pub skip_focus_for_active_in_search: bool,
}

/// 远程连接设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct RemoteSettingsContent {
    /// 远程服务器路径
    pub remote_server_path: Option<String>,

    /// 是否自动安装远程服务器。默认: true
    pub auto_install: bool,
}

/// 工具遥测设置
#[with_fallible_options]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Debug, MergeFrom)]
#[serde(default)]
pub struct TelemetrySettingsContent {
    /// 是否收集诊断事件
    pub diagnostics: bool,
    /// 是否收集应用事件
    pub events: bool,
    /// 是否收集 metrics
    pub metrics: bool,
}

impl Default for TelemetrySettingsContent {
    fn default() -> Self {
        Self {
            diagnostics: true,
            events: true,
            metrics: true,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, MergeFrom)]
#[serde(transparent)]
pub struct FeatureFlagsMap(pub HashMap<String, String>);

impl JsonSchema for FeatureFlagsMap {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FeatureFlagsMap".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": { "type": "string" }
        })
    }
}

impl std::ops::Deref for FeatureFlagsMap {
    type Target = HashMap<String, String>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for FeatureFlagsMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// 优化构建避免下游单态化
pub trait RootUserSettings: Sized + DeserializeOwned {
    fn parse_json(json: &str) -> (Option<Self>, ParseStatus);
    fn parse_json_with_comments(json: &str) -> anyhow::Result<Self>;
}

impl RootUserSettings for SettingsContent {
    fn parse_json(json: &str) -> (Option<Self>, ParseStatus) {
        fallible_options::parse_json(json)
    }
    fn parse_json_with_comments(json: &str) -> anyhow::Result<Self> {
        parse_json_with_comments(json)
    }
}

impl RootUserSettings for Option<SettingsContent> {
    fn parse_json(json: &str) -> (Option<Self>, ParseStatus) {
        fallible_options::parse_json(json)
    }
    fn parse_json_with_comments(json: &str) -> anyhow::Result<Self> {
        parse_json_with_comments(json)
    }
}

impl RootUserSettings for UserSettingsContent {
    fn parse_json(json: &str) -> (Option<Self>, ParseStatus) {
        fallible_options::parse_json(json)
    }
    fn parse_json_with_comments(json: &str) -> anyhow::Result<Self> {
        parse_json_with_comments(json)
    }
}

settings_overrides! {
    #[with_fallible_options]
    #[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
    pub struct ReleaseChannelOverrides { dev, nightly, preview, stable }
}

settings_overrides! {
    #[with_fallible_options]
    #[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
    pub struct PlatformOverrides { macos, linux, windows }
}

/// 配置文件基于的基础设置
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ProfileBase {
    /// 在用户设置之上应用配置文件覆盖
    #[default]
    User,
    /// 在默认设置之上应用配置文件覆盖，忽略用户自定义
    Default,
}

/// 命名配置文件，可以临时覆盖设置
#[with_fallible_options]
#[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct SettingsProfile {
    /// 应用此配置文件覆盖之前的基础设置
    #[serde(default)]
    pub base: ProfileBase,

    /// 此配置文件的设置覆盖
    #[serde(default)]
    pub settings: Box<SettingsContent>,
}

#[with_fallible_options]
#[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct UserSettingsContent {
    #[serde(flatten)]
    pub content: Box<SettingsContent>,

    #[serde(flatten)]
    pub release_channel_overrides: ReleaseChannelOverrides,

    #[serde(flatten)]
    pub platform_overrides: PlatformOverrides,

    #[serde(default)]
    pub profiles: IndexMap<String, SettingsProfile>,
}

/// 基础键盘映射方案
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
    MergeFrom,
    PartialEq,
    Eq,
    strum::VariantArray,
    strum::VariantNames,
    strum::FromRepr,
)]
#[serde(rename_all = "snake_case")]
pub enum BaseKeymapContent {
    /// VSCode 键盘映射
    #[default]
    #[serde(alias = "VSCode")]
    VSCode,
    #[serde(alias = "JetBrains")]
    JetBrains,
    #[serde(alias = "SublimeText")]
    SublimeText,
    /// Vim 键盘映射
    Vim,
    /// Zed 默认键盘映射
    Zed,
    /// Helix 键盘映射
    Helix,
    /// Atom 键盘映射
    Atom,
    /// TextMate 键盘映射
    #[serde(alias = "TextMate")]
    TextMate,
    /// Emacs 键盘映射
    Emacs,
    /// Cursor 键盘映射
    Cursor,
    /// 无键盘映射
    None,
}

/// 兼容占位类型: SaturatingBool (spec §16 Plan 16)
#[derive(Debug, Default, Copy, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SaturatingBool(pub bool);

impl std::fmt::Display for SaturatingBool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<bool> for SaturatingBool {
    fn from(b: bool) -> Self {
        Self(b)
    }
}

impl From<SaturatingBool> for bool {
    fn from(s: SaturatingBool) -> Self {
        s.0
    }
}

impl merge_from::MergeFrom for SaturatingBool {
    fn merge_from(&mut self, other: &Self) {
        self.0 = self.0 || other.0;
    }
}
// 以下为已删除模块的兼容占位类型 (spec §16 Plan 16)
// 保留以兼容 settings_store 中尚未清理的引用。

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct SemanticTokenRules {
    pub rules: Vec<SemanticTokenRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct SemanticTokenRule {
    pub token_type: Option<String>,
    pub token_modifiers: Vec<String>,
    pub style: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ExtensionsSettingsContent {
    pub all_languages: LanguageToSettingsMap,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct LanguageToSettingsMap {
    pub settings: HashMap<String, LanguageSettingsContent>,
    pub defaults: LanguageSettingsContent,
    pub languages: HashMap<String, LanguageSettingsContent>,
    pub edit_predictions: Option<EditPredictionSettingsContent>,
    pub file_types: Option<FileTypeMap>,
}

/// Maps language names to additional filename and extension patterns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct FileTypeMap(pub HashMap<Arc<str>, Vec<String>>);

impl merge_from::MergeFrom for FileTypeMap {
    fn merge_from(&mut self, other: &Self) {
        for (language, patterns) in &other.0 {
            self.0
                .entry(language.clone())
                .or_default()
                .extend(patterns.iter().cloned());
        }
    }
}

impl<'a> IntoIterator for &'a FileTypeMap {
    type Item = (&'a Arc<str>, &'a Vec<String>);
    type IntoIter = std::collections::hash_map::Iter<'a, Arc<str>, Vec<String>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct LanguageSettingsContent {
    // 兼容占位字段 - Option<T> 保持下游 unwrap() 调用 (spec §16 Plan 16)
    pub tab_size: Option<NonZeroU32>,
    pub hard_tabs: Option<bool>,
    pub soft_wrap: Option<SoftWrap>,
    pub preferred_line_length: Option<u32>,
    pub show_wrap_guides: Option<bool>,
    pub wrap_guides: Option<Vec<usize>>,
    pub format_on_save: Option<FormatOnSave>,
    pub remove_trailing_whitespace_on_save: Option<bool>,
    pub ensure_final_newline_on_save: Option<bool>,
    pub line_ending: Option<LineEndingSetting>,
    pub formatter: Option<FormatterList>,
    pub jsx_tag_auto_close: Option<JsxTagAutoCloseContent>,
    pub enable_language_server: Option<bool>,
    pub language_servers: Option<Vec<String>>,
    pub semantic_tokens: Option<SemanticTokens>,
    pub document_folding_ranges: Option<DocumentFoldingRanges>,
    pub document_symbols: Option<DocumentSymbols>,
    pub allow_rewrap: Option<RewrapBehavior>,
    pub show_edit_predictions: Option<bool>,
    pub edit_predictions_disabled_in: Option<Vec<String>>,
    pub show_whitespaces: Option<ShowWhitespaceSetting>,
    pub extend_comment_on_newline: Option<bool>,
    pub extend_list_on_newline: Option<bool>,
    pub indent_list_on_tab: Option<bool>,
    pub use_autoclose: Option<bool>,
    pub use_auto_surround: Option<bool>,
    pub use_on_type_format: Option<bool>,
    pub auto_indent: Option<AutoIndentMode>,
    pub auto_indent_on_paste: Option<bool>,
    pub always_treat_brackets_as_autoclosed: Option<bool>,
    pub code_actions_on_format: Option<HashMap<String, bool>>,
    pub linked_edits: Option<bool>,
    pub show_completions_on_input: Option<bool>,
    pub show_completion_documentation: Option<bool>,
    pub colorize_brackets: Option<bool>,
    pub debuggers: Option<Vec<String>>,
    pub word_diff_enabled: Option<bool>,
    // 子设置结构体 - Option<T>
    pub inlay_hints: Option<InlayHintsSettingsContent>,
    pub completions: Option<CompletionSettingsContent>,
    pub prettier: Option<PrettierSettingsContent>,
    pub indent_guides: Option<IndentGuidesSettingsContent>,
    pub tasks: Option<TaskSettingsContent>,
    pub whitespace_map: Option<WhitespaceMapContent>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct LspSettings {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct LspSettingsMap {
    pub settings: HashMap<String, LspSettings>,
}

// ============================================================
// 兼容占位类型 - 保持下游 crate 编译 (spec §16 Plan 16)
// 这些类型已删除但下游代码仍引用，保留 stub 避免编译错误。
// ============================================================

/// 软换行模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum SoftWrap {
    #[default]
    None,
    PreferLine,
    EditorWidth,
    Bounded,
}

/// 光标形状
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Bar,
    #[default]
    Block,
    Underline,
    Hollow,
}

/// 缩进引导线着色
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum IndentGuideColoring {
    /// Use the same color for all indent guides.
    #[default]
    SingleHue,
    /// Use a different color for each indent level.
    DistinctHues,
    Disabled,
}

/// 缩进引导背景着色
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum IndentGuideBackgroundColoring {
    #[default]
    Off,
    Active,
}

/// 文档折叠范围
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentFoldingRanges {
    #[default]
    TreeSitterAndIndent,
    LanguageServer,
}

/// 文档符号来源
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentSymbols {
    #[default]
    TreeSitter,
    LanguageServer,
}

/// 语义 token 高亮模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum SemanticTokens {
    #[default]
    Off,
    On,
    TreeSitterFallback,
}

/// 自动缩进模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum AutoIndentMode {
    #[default]
    On,
    Off,
    OnFormatting,
    OnTyping,
    SyntaxAware,
    None,
}

/// 保存时格式化模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum FormatOnSave {
    #[default]
    Off,
    On,
    OnWithExtraLspActions,
}

/// 格式化器
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct Formatter {
    pub name: String,
}

/// 格式化器列表
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct FormatterList {
    pub formatters: Vec<Formatter>,
}

/// 空白显示设置
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ShowWhitespaceSetting {
    #[default]
    Selected,
    All,
    Off,
    Trailing,
}

/// 单词补全模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum WordsCompletionMode {
    #[default]
    Enabled,
    LspOnly,
    Disabled,
}

/// 行尾设置
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum LineEndingSetting {
    #[default]
    Auto,
    Lf,
    Crlf,
    EnforceLf,
    EnforceCrlf,
}

/// LSP 插入模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum LspInsertMode {
    #[default]
    AsSnippet,
    AsPlainText,
}

/// 内联提示类型
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum InlayHintKind {
    #[default]
    All,
    OnlyParameters,
    OnlyTypes,
    Off,
    Type,
    Parameter,
}

/// 重排行为
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum RewrapBehavior {
    #[default]
    Enabled,
    VimModeOnly,
    Disabled,
}

/// 编辑预测提供者
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum EditPredictionProvider {
    #[default]
    None,
    Copilot,
    Codestral,
    OpenAiCompatible,
}

/// 编辑预测模式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum EditPredictionsMode {
    #[default]
    Off,
    Inline,
    Preview,
}

/// 数据收集选项
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum EditPredictionDataCollectionChoice {
    #[default]
    Allow,
    Disallow,
}

/// 提示格式
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum EditPredictionPromptFormatContent {
    #[default]
    Context,
    Zeta1,
    Zeta2,
    Zeta3,
    Infer,
    Zeta,
    Zeta2_1,
    CodeLlama,
    StarCoder,
    DeepseekCoder,
    Qwen,
    CodeGemma,
    Codestral,
    Glm,
}

/// 补全设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CompletionSettingsContent {
    pub words: Option<WordsCompletionMode>,
    pub words_min_length: Option<u32>,
    pub lsp: Option<bool>,
    pub lsp_fetch_timeout_ms: Option<u32>,
    pub lsp_insert_mode: Option<LspInsertMode>,
}

/// 内联提示设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct InlayHintsSettingsContent {
    pub enabled: Option<bool>,
    pub show_value_hints: Option<bool>,
    pub show_type_hints: Option<bool>,
    pub show_parameter_hints: Option<bool>,
    pub show_other_hints: Option<bool>,
    pub show_background: Option<bool>,
    pub edit_debounce_ms: Option<u32>,
    pub scroll_debounce_ms: Option<u32>,
    pub toggle_on_modifiers_press: Option<ModifiersContent>,
}

/// Prettier 设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct PrettierSettingsContent {
    pub allowed: Option<bool>,
    pub parser: Option<String>,
    pub plugins: Option<Vec<String>>,
    pub options: Option<serde_json::Value>,
}

/// 缩进引导设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct IndentGuidesSettingsContent {
    pub enabled: Option<bool>,
    pub line_width: Option<u32>,
    pub active_line_width: Option<u32>,
    pub coloring: Option<IndentGuideColoring>,
    pub background_coloring: Option<IndentGuideBackgroundColoring>,
}

/// 任务设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct TaskSettingsContent {
    pub variables: Option<HashMap<String, String>>,
    pub enabled: Option<bool>,
    pub prefer_lsp: Option<bool>,
}

/// 空白映射设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct WhitespaceMapContent {
    pub space: Option<String>,
    pub tab: Option<String>,
}

/// JSX 标签自动关闭设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct JsxTagAutoCloseContent {
    pub enabled: Option<bool>,
}

/// 编辑预测设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct EditPredictionSettingsContent {
    pub provider: Option<EditPredictionProvider>,
    pub mode: Option<EditPredictionsMode>,
    pub disabled_globs: Option<Vec<String>>,
    pub allow_data_collection: Option<EditPredictionDataCollectionChoice>,
    pub prompt_format: Option<EditPredictionPromptFormatContent>,
    pub copilot: Option<CopilotSettingsContent>,
    pub codestral: Option<CodestralSettingsContent>,
    pub ollama: Option<OllamaSettingsContent>,
    pub open_ai_compatible_api: Option<OpenAiCompatibleApiSettingsContent>,
}

/// Copilot 设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CopilotSettingsContent {
    pub token: Option<String>,
    pub organization_id: Option<String>,
    pub proxy: Option<String>,
    pub proxy_no_verify: Option<bool>,
    pub enterprise_uri: Option<String>,
    pub enable_next_edit_suggestions: Option<bool>,
}

/// Codestral 设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CodestralSettingsContent {
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub api_url: Option<String>,
}

/// Ollama 设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct OllamaSettingsContent {
    pub model: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub api_url: Option<String>,
    pub prompt_format: Option<EditPredictionPromptFormatContent>,
}

/// OpenAI 兼容 API 设置内容 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct OpenAiCompatibleApiSettingsContent {
    pub model: Option<String>,
    pub api_url: Option<String>,
    pub prompt_format: Option<EditPredictionPromptFormatContent>,
}

/// Git 托管提供者配置
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct GitHostingProviderConfig {
    pub name: String,
    pub provider: GitHostingProviderKind,
    pub base_url: String,
}

/// Git 托管提供者类型
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum GitHostingProviderKind {
    #[default]
    Github,
    Gitlab,
    Bitbucket,
    Gitea,
    Forgejo,
    SourceHut,
}

/// SSH 端口转发选项 (spec §16 Plan 16)
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
pub struct SshPortForwardOption {
    pub local_host: String,
    pub remote_host: String,
    pub local_port: u16,
    pub remote_port: u16,
}

/// SSH 连接配置 (spec §16 Plan 16)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct SshConnection {
    pub host: String,
    pub username: Option<String>,
    pub port: Option<u16>,
    pub args: Option<Vec<String>>,
    pub nickname: Option<String>,
    pub upload_binary_over_ssh: Option<bool>,
    pub port_forwards: Option<Vec<SshPortForwardOption>>,
    pub connection_timeout: Option<u16>,
}

/// WSL 连接配置
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct WslConnection {
    pub distro_name: String,
    pub user: Option<String>,
}

/// 其余语言服务器占位常量
pub const REST_OF_LANGUAGE_SERVERS: &str = "...";
/// 修饰键设置 (spec §16 Plan 16) - 兼容 gpui::Modifiers
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
pub struct ModifiersContent {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
    pub function: bool,
}
/// 兼容类型别名 - 保持下游引用 AllLanguageSettingsContent 编译通过
pub type AllLanguageSettingsContent = LanguageToSettingsMap;
