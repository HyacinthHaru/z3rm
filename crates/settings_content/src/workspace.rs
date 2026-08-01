use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// UI chrome workspace settings (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct WorkspaceSettingsContent {
    /// What draws window decorations/titlebar. Default: client
    pub window_decorations: WindowDecorations,

    /// The text rendering mode to use. Default: platform_default
    pub text_rendering_mode: TextRenderingMode,

    /// Whether the focused panel follows the mouse location.
    pub focus_follows_mouse: FocusFollowsMouse,

    /// Whether or not to prompt the user to confirm before closing the application. Default: false
    pub confirm_quit: bool,

    /// What to do when the last window is closed.
    pub on_last_window_closed: OnLastWindowClosed,
}

/// What draws window decorations/titlebar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum WindowDecorations {
    /// Use system-provided window decorations.
    System,
    /// Use client-provided window decorations.
    #[default]
    Client,
    /// No window decorations.
    None,
}

/// The text rendering mode to use.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum TextRenderingMode {
    /// Use the platform default.
    #[default]
    PlatformDefault,
    /// Use software rendering.
    Software,
    /// Use anti-aliased rendering.
    AntiAliased,
}

/// Whether the focused panel follows the mouse location.
#[with_fallible_options]
#[derive(Copy, Clone, PartialEq, Debug, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct FocusFollowsMouse {
    /// Whether focus follows the mouse. Default: false
    pub enabled: bool,
}

/// What to do when the last window is closed.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum OnLastWindowClosed {
    /// Do nothing.
    #[default]
    Nothing,
    /// Quit the application.
    Quit,
}

/// Tab settings for terminal panes (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct TabBarSettingsContent {
    /// Whether to show the tab bar. Default: true
    pub show: bool,

    /// Whether to show the middle click to close tab behavior. Default: true
    pub middle_click_to_close: bool,

    /// Whether to show the mouse scroll to switch tab behavior. Default: true
    pub mouse_scroll_to_switch: bool,

    /// Whether to show the active item only. Default: false
    pub show_active_item: bool,

    /// Whether to show the button to close a tab. Default: hover
    pub show_close_button: ShowCloseButton,
}

/// Position of the close button in a tab.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "lowercase")]
pub enum ShowCloseButton {
    /// Show when the mouse hovers over the tab.
    #[default]
    Hover,
    /// Always show.
    Always,
    /// Never show.
    Never,
    /// Hidden (alias for Never, backward compat).
    Hidden,
}

/// Tab item settings (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ItemSettingsContent {
    /// Whether to show the Git file status on a tab item.
    ///
    /// Default: true
    pub git_status: Option<bool>,

    /// Position of the close button in a tab.
    ///
    /// Default: right
    pub close_position: Option<ClosePosition>,

    /// What to do after closing the current tab.
    ///
    /// Default: next
    pub activate_on_close: Option<ActivateOnClose>,

    /// Whether to show the file icon for a tab.
    ///
    /// Default: true
    pub file_icons: Option<bool>,

    /// Which files containing diagnostic errors/warnings to mark in the tabs.
    ///
    /// Default: off
    pub show_diagnostics: Option<ShowDiagnostics>,

    /// When to show the close button in a tab.
    ///
    /// Default: hover
    pub show_close_button: Option<ShowCloseButton>,
}

/// Position of the close button within a tab.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "lowercase")]
pub enum ClosePosition {
    Left,
    #[default]
    Right,
}

/// Which tab to activate after the current one is closed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum ActivateOnClose {
    #[default]
    Next,
    Neighbour,
    LeftNeighbour,
    History,
    None,
}

/// Which diagnostic severities to mark on a tab.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum ShowDiagnostics {
    #[default]
    Off,
    Errors,
    All,
    Inline,
    OnHover,
}

/// Preview tab settings (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct PreviewTabsSettingsContent {
    /// Whether to show opened items as preview tabs. Preview tabs do not stay
    /// open, are reused until explicitly set to be kept open and show their
    /// title in italic.
    ///
    /// Default: true
    pub enabled: Option<bool>,

    /// Whether to open tabs in preview mode when opened from the project panel
    /// with a single click.
    ///
    /// Default: true
    pub enable_preview_from_project_panel: Option<bool>,

    /// Whether to open tabs in preview mode when selected from the file finder.
    ///
    /// Default: true
    pub enable_preview_from_file_finder: Option<bool>,

    /// Whether to open tabs in preview mode when opened from a multibuffer.
    ///
    /// Default: true
    pub enable_preview_from_multibuffer: Option<bool>,

    /// Whether to open tabs in preview mode when code navigation is used to
    /// open a multibuffer.
    ///
    /// Default: true
    pub enable_preview_multibuffer_from_code_navigation: Option<bool>,

    /// Whether to open tabs in preview mode when code navigation is used to
    /// open a single file.
    ///
    /// Default: true
    pub enable_preview_file_from_code_navigation: Option<bool>,

    /// Whether to keep tabs in preview mode when code navigation is used to
    /// navigate away from them.
    ///
    /// Default: false
    pub enable_keep_preview_on_code_navigation: Option<bool>,
}

/// Status bar settings (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct StatusBarSettingsContent {
    /// Whether to show the stack size on the status bar. Default: false
    pub stack_size: bool,

    /// Whether to show the working directory on the status bar. Default: true
    pub working_directory: bool,

    /// Whether to show the session status on the status bar. Default: false
    pub session_status: bool,

    /// Whether to show the active language button on the status bar. Default: true
    pub active_language_button: bool,

    /// Encoding display option. Default: NonUtf8
    pub active_encoding_button: EncodingDisplayOptions,

    /// Whether to show the cursor position button on the status bar. Default: true
    pub cursor_position_button: bool,

    /// Whether to show the line endings button on the status bar. Default: false
    pub line_endings_button: bool,
}

/// 行号指示器格式 (spec §16 Plan 16)
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum LineIndicatorFormat {
    #[default]
    Short,
    Long,
}

/// 编码显示选项 (spec §16 Plan 16)
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum EncodingDisplayOptions {
    #[default]
    NonUtf8,
    All,
    Disabled,
    Never,
}

impl EncodingDisplayOptions {
    pub fn should_show(&self, is_utf8: bool, has_bom: bool) -> bool {
        match self {
            EncodingDisplayOptions::NonUtf8 => !is_utf8 || has_bom,
            EncodingDisplayOptions::All => true,
            EncodingDisplayOptions::Disabled | EncodingDisplayOptions::Never => false,
        }
    }
}
