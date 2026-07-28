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
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
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
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
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
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum LineIndicatorFormat {
    #[default]
    Short,
    Long,
}

/// 编码显示选项 (spec §16 Plan 16)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
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
