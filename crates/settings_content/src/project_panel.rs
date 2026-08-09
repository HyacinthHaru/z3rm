use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

use crate::ShowScrollbar;

/// 项目面板设置 (spec §16 Plan 16)
///
/// 字段与 `project_panel::ProjectPanelSettings` 一一对应。
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ProjectPanelSettingsContent {
    /// Whether to show the project panel button in the status bar.
    ///
    /// Default: true
    pub button: Option<bool>,

    /// Whether to hide gitignored entries in the project panel.
    ///
    /// Default: false
    pub hide_gitignore: Option<bool>,

    /// Customize default width (in pixels) taken by the project panel.
    ///
    /// Default: 240
    #[serde(serialize_with = "crate::serialize_optional_f32_with_two_decimal_places")]
    pub default_width: Option<f32>,

    /// Which side of the window the project panel docks to.
    ///
    /// Default: left
    pub dock: Option<DockSide>,

    /// Spacing between worktree entries in the project panel.
    ///
    /// Default: comfortable
    pub entry_spacing: Option<ProjectPanelEntrySpacing>,

    /// Whether to show file icons in the project panel.
    ///
    /// Default: true
    pub file_icons: Option<bool>,

    /// Whether to show folder icons or chevrons for directories in the project panel.
    ///
    /// Default: true
    pub folder_icons: Option<bool>,

    /// Whether to show the git status in the project panel.
    ///
    /// Default: true
    pub git_status: Option<bool>,

    /// Amount of indentation (in pixels) for nested items.
    ///
    /// Default: 20
    #[serde(serialize_with = "crate::serialize_optional_f32_with_two_decimal_places")]
    pub indent_size: Option<f32>,

    /// Settings related to indent guides in the project panel.
    pub indent_guides: Option<ProjectPanelIndentGuidesSettings>,

    /// Whether to stick parent directories at the top of the project panel.
    ///
    /// Default: true
    pub sticky_scroll: Option<bool>,

    /// Whether to reveal an entry in the project panel automatically when the
    /// corresponding project entry becomes active. Gitignored entries are never
    /// auto revealed.
    ///
    /// Default: true
    pub auto_reveal_entries: Option<bool>,

    /// Whether to fold directories automatically when a directory has only one
    /// directory inside.
    ///
    /// Default: true
    pub auto_fold_dirs: Option<bool>,

    /// Whether to show folder names with bold text in the project panel.
    ///
    /// Default: false
    pub bold_folder_labels: Option<bool>,

    /// Whether the project panel should open on startup.
    ///
    /// Default: true
    pub starts_open: Option<bool>,

    /// Scrollbar-related settings.
    pub scrollbar: Option<ProjectPanelScrollbarSettingsContent>,

    /// Which files containing diagnostic errors/warnings to mark in the project panel.
    ///
    /// Default: off
    pub show_diagnostics: Option<ProjectPanelShowDiagnostics>,

    /// Whether to hide the root entry when only one folder is open in the window.
    ///
    /// Default: false
    pub hide_root: Option<bool>,

    /// Whether to hide the hidden entries in the project panel.
    ///
    /// Default: false
    pub hide_hidden: Option<bool>,

    /// Whether to enable drag-and-drop operations in the project panel.
    ///
    /// Default: true
    pub drag_and_drop: Option<bool>,

    /// Settings for automatically opening files.
    pub auto_open: Option<ProjectPanelAutoOpenSettings>,

    /// How to group sibling entries in the project panel.
    ///
    /// Default: directories_first
    pub sort_mode: Option<ProjectPanelSortMode>,

    /// How to compare sibling entry names in the project panel. This works in
    /// combination with `sort_mode`: `sort_mode` controls how files and
    /// directories are grouped, while this controls how names are compared.
    ///
    /// Default: default
    pub sort_order: Option<ProjectPanelSortOrder>,

    /// Whether to show error and warning count badges next to file names in the
    /// project panel.
    ///
    /// Default: false
    pub diagnostic_badges: Option<bool>,

    /// Whether to show a git status indicator next to file names in the project panel.
    ///
    /// Default: false
    pub git_status_indicator: Option<bool>,
}

/// Settings for automatically opening files touched from the project panel.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ProjectPanelAutoOpenSettings {
    /// Whether to automatically open newly created files in the editor.
    ///
    /// Default: true
    pub on_create: Option<bool>,

    /// Whether to automatically open files after pasting or duplicating them.
    ///
    /// Default: true
    pub on_paste: Option<bool>,

    /// Whether to automatically open files dropped from external sources.
    ///
    /// Default: true
    pub on_drop: Option<bool>,
}

/// Scrollbar settings for the project panel.
#[with_fallible_options]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
pub struct ProjectPanelScrollbarSettingsContent {
    /// When to show the scrollbar in the project panel.
    ///
    /// Default: null, which inherits the editor scrollbar settings
    pub show: Option<ShowScrollbar>,

    /// Whether to allow horizontal scrolling in the project panel. When false,
    /// the view is locked to the leftmost position and long file names are clipped.
    ///
    /// Default: true
    pub horizontal_scroll: Option<bool>,
}

/// Indent guide settings for the project panel.
#[with_fallible_options]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
pub struct ProjectPanelIndentGuidesSettings {
    /// When to show indent guides in the project panel.
    ///
    /// Default: always
    pub show: Option<ShowIndentGuides>,
}

/// Which side of the window a dockable panel attaches to.
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
pub enum DockSide {
    #[default]
    Left,
    Right,
}

/// Spacing between entries in the project panel.
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
pub enum ProjectPanelEntrySpacing {
    /// Comfortable spacing of entries.
    #[default]
    Comfortable,
    /// The standard spacing of entries.
    Standard,
}

/// When to show indent guides in the project panel.
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
pub enum ShowIndentGuides {
    #[default]
    Always,
    Never,
}

/// Which diagnostic severities to mark in the project panel.
///
/// Distinct from [`crate::ShowDiagnostics`], which additionally carries the
/// `inline` and `on_hover` presentations that only apply to editor tabs.
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
pub enum ProjectPanelShowDiagnostics {
    /// Do not mark any files.
    #[default]
    Off,
    /// Only mark files with errors.
    Errors,
    /// Mark files with errors and warnings.
    All,
}

/// How to group sibling entries in the project panel.
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
pub enum ProjectPanelSortMode {
    /// Show directories first, then files.
    #[default]
    DirectoriesFirst,
    /// Mix directories and files together.
    Mixed,
    /// Show files first, then directories.
    FilesFirst,
}

/// How to compare sibling entry names in the project panel.
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
pub enum ProjectPanelSortOrder {
    /// Case-insensitive natural sort with lowercase preferred in ties.
    /// Numbers in file names are compared by value (e.g., `file2` before `file10`).
    #[default]
    Default,
    /// Uppercase names are grouped before lowercase names, with case-insensitive
    /// natural sort within each group. Dot-prefixed names sort before both groups.
    Upper,
    /// Lowercase names are grouped before uppercase names, with case-insensitive
    /// natural sort within each group. Dot-prefixed names sort before both groups.
    Lower,
    /// Pure Unicode codepoint comparison. No case folding, no natural number sorting.
    /// Uppercase ASCII sorts before lowercase. Accented characters sort after ASCII.
    Unicode,
}
