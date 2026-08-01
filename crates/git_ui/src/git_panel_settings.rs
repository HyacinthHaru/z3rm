use gpui::Pixels;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};
use ui::{
    px,
    scrollbars::{ScrollbarVisibility, ShowScrollbar},
};
use workspace::dock::DockPosition;

/// 滚动条设置
#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ScrollbarSettings {
    pub show: Option<ShowScrollbar>,
}

/// 状态样式 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusStyle {
    /// 使用图标显示状态
    #[default]
    Icon,
    /// 使用标签颜色显示状态
    LabelColor,
}

/// Git 面板排序方式 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitPanelSortBy {
    /// 按路径排序
    #[default]
    Path,
    /// 按名称排序
    Name,
}

/// Git 面板分组方式 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitPanelGroupBy {
    /// 不分组
    #[default]
    None,
    /// 按状态分组
    Status,
    /// 按暂存状态分组
    Staging,
}

/// Git 面板点击行为 (spec §16 Plan 16)
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitPanelClickBehavior {
    /// 点击打开文件差异
    #[default]
    FileDiff,
    /// 点击查看文件
    ViewFile,
    /// 点击打开项目差异
    ProjectDiff,
}

/// Git 面板设置 (spec §16 Plan 16)
#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct GitPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub status_style: StatusStyle,
    pub file_icons: bool,
    pub folder_icons: bool,
    pub scrollbar: ScrollbarSettings,
    pub fallback_branch_name: String,
    pub sort_by: GitPanelSortBy,
    pub group_by: GitPanelGroupBy,
    pub collapse_untracked_diff: bool,
    pub tree_view: bool,
    pub diff_stats: bool,
    pub show_count_badge: bool,
    pub starts_open: bool,
    pub commit_title_max_length: usize,
    pub entry_primary_click_action: GitPanelClickBehavior,
}

#[derive(Default)]
pub(crate) struct GitPanelScrollbarAccessor;

impl ScrollbarVisibility for GitPanelScrollbarAccessor {
    fn visibility(&self, _cx: &ui::App) -> ShowScrollbar {
        // editor crate removed; default to showing scrollbar
        ShowScrollbar::Always
    }
}

impl Settings for GitPanelSettings {
    fn from_settings(_content: &settings::SettingsContent) -> Self {
        // git_panel 字段已从 SettingsContent 中移除 (spec §16 Plan 16)，
        // 这里的常量必须与 assets/settings/default.json 里记录的产品默认值一致。
        Self {
            button: true,
            dock: DockPosition::Right,
            default_width: px(360.),
            status_style: StatusStyle::Icon,
            file_icons: false,
            folder_icons: true,
            scrollbar: ScrollbarSettings { show: None },
            fallback_branch_name: "main".to_string(),
            sort_by: GitPanelSortBy::Path,
            group_by: GitPanelGroupBy::Status,
            collapse_untracked_diff: false,
            tree_view: false,
            diff_stats: true,
            show_count_badge: false,
            starts_open: false,
            commit_title_max_length: 0,
            entry_primary_click_action: GitPanelClickBehavior::ProjectDiff,
        }
    }
}
