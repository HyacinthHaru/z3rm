use editor::{EditorSettings, ui_scrollbar_settings_from_raw};
use gpui::Pixels;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};
use ui::{
    px,
    scrollbars::{ScrollbarVisibility, ShowScrollbar},
};

/// 项目面板停靠位置 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DockSide {
    #[default]
    Left,
    Right,
}

/// 项目面板条目间距 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProjectPanelEntrySpacing {
    #[default]
    Comfortable,
    Standard,
}

/// 项目面板排序模式 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPanelSortMode {
    #[default]
    DirectoriesFirst,
    Mixed,
    FilesFirst,
}

/// 项目面板排序顺序 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPanelSortOrder {
    #[default]
    Default,
    Upper,
    Lower,
    Unicode,
}

/// 缩进引导线显示模式 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShowIndentGuides {
    #[default]
    Always,
    Never,
}

/// 诊断显示模式 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShowDiagnostics {
    #[default]
    Off,
    Errors,
    All,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Default, RegisterSetting)]
pub struct ProjectPanelSettings {
    pub button: bool,
    pub hide_gitignore: bool,
    pub default_width: Pixels,
    pub dock: DockSide,
    pub entry_spacing: ProjectPanelEntrySpacing,
    pub file_icons: bool,
    pub folder_icons: bool,
    pub git_status: bool,
    pub indent_size: f32,
    pub indent_guides: IndentGuidesSettings,
    pub sticky_scroll: bool,
    pub auto_reveal_entries: bool,
    pub auto_fold_dirs: bool,
    pub bold_folder_labels: bool,
    pub starts_open: bool,
    pub scrollbar: ScrollbarSettings,
    pub show_diagnostics: ShowDiagnostics,
    pub hide_root: bool,
    pub hide_hidden: bool,
    pub drag_and_drop: bool,
    pub auto_open: AutoOpenSettings,
    pub sort_mode: ProjectPanelSortMode,
    pub sort_order: ProjectPanelSortOrder,
    pub diagnostic_badges: bool,
    pub git_status_indicator: bool,
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct IndentGuidesSettings {
    pub show: ShowIndentGuides,
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ScrollbarSettings {
    /// When to show the scrollbar in the project panel.
    ///
    /// Default: inherits editor scrollbar settings
    pub show: Option<ShowScrollbar>,
    /// Whether to allow horizontal scrolling in the project panel.
    /// When false, the view is locked to the leftmost position and long file names are clipped.
    ///
    /// Default: true
    pub horizontal_scroll: bool,
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct AutoOpenSettings {
    pub on_create: bool,
    pub on_paste: bool,
    pub on_drop: bool,
}

impl AutoOpenSettings {
    #[inline]
    pub fn should_open_on_create(self) -> bool {
        self.on_create
    }

    #[inline]
    pub fn should_open_on_paste(self) -> bool {
        self.on_paste
    }

    #[inline]
    pub fn should_open_on_drop(self) -> bool {
        self.on_drop
    }
}

#[derive(Default)]
pub(crate) struct ProjectPanelScrollbarProxy;

impl ScrollbarVisibility for ProjectPanelScrollbarProxy {
    fn visibility(&self, cx: &ui::App) -> ShowScrollbar {
        ProjectPanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl Settings for ProjectPanelSettings {
    fn from_settings(_content: &settings::SettingsContent) -> Self {
        // 项目面板设置已从 SettingsContent 中移除 (spec §16 Plan 16)
        // 返回默认值
        Self::default()
    }
}

/// From trait for ProjectPanelSortMode -> util::paths::SortMode
impl From<ProjectPanelSortMode> for util::paths::SortMode {
    fn from(mode: ProjectPanelSortMode) -> Self {
        match mode {
            ProjectPanelSortMode::DirectoriesFirst => util::paths::SortMode::DirectoriesFirst,
            ProjectPanelSortMode::Mixed => util::paths::SortMode::Mixed,
            ProjectPanelSortMode::FilesFirst => util::paths::SortMode::FilesFirst,
        }
    }
}

/// From trait for ProjectPanelSortOrder -> util::paths::SortOrder
impl From<ProjectPanelSortOrder> for util::paths::SortOrder {
    fn from(order: ProjectPanelSortOrder) -> Self {
        match order {
            ProjectPanelSortOrder::Default => util::paths::SortOrder::Default,
            ProjectPanelSortOrder::Upper => util::paths::SortOrder::Upper,
            ProjectPanelSortOrder::Lower => util::paths::SortOrder::Lower,
            ProjectPanelSortOrder::Unicode => util::paths::SortOrder::Unicode,
        }
    }
}
