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
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let project_panel = content.project_panel.clone().unwrap_or_default();
        let indent_guides = project_panel.indent_guides.unwrap_or_default();
        let scrollbar = project_panel.scrollbar.unwrap_or_default();
        let auto_open = project_panel.auto_open.unwrap_or_default();

        Self {
            button: project_panel.button.unwrap_or(true),
            hide_gitignore: project_panel.hide_gitignore.unwrap_or(false),
            default_width: px(project_panel.default_width.unwrap_or(240.0)),
            dock: project_panel
                .dock
                .map_or_else(DockSide::default, |dock| match dock {
                    settings::DockSide::Left => DockSide::Left,
                    settings::DockSide::Right => DockSide::Right,
                }),
            entry_spacing: project_panel.entry_spacing.map_or_else(
                ProjectPanelEntrySpacing::default,
                |entry_spacing| match entry_spacing {
                    settings::ProjectPanelEntrySpacing::Comfortable => {
                        ProjectPanelEntrySpacing::Comfortable
                    }
                    settings::ProjectPanelEntrySpacing::Standard => {
                        ProjectPanelEntrySpacing::Standard
                    }
                },
            ),
            file_icons: project_panel.file_icons.unwrap_or(true),
            folder_icons: project_panel.folder_icons.unwrap_or(true),
            git_status: project_panel.git_status.unwrap_or(true),
            indent_size: project_panel.indent_size.unwrap_or(20.0),
            indent_guides: IndentGuidesSettings {
                show: indent_guides
                    .show
                    .map_or_else(ShowIndentGuides::default, |show| match show {
                        settings::ShowIndentGuides::Always => ShowIndentGuides::Always,
                        settings::ShowIndentGuides::Never => ShowIndentGuides::Never,
                    }),
            },
            sticky_scroll: project_panel.sticky_scroll.unwrap_or(true),
            auto_reveal_entries: project_panel.auto_reveal_entries.unwrap_or(true),
            auto_fold_dirs: project_panel.auto_fold_dirs.unwrap_or(true),
            bold_folder_labels: project_panel.bold_folder_labels.unwrap_or(false),
            starts_open: project_panel.starts_open.unwrap_or(true),
            scrollbar: ScrollbarSettings {
                show: scrollbar.show.map(ui_scrollbar_settings_from_raw),
                horizontal_scroll: scrollbar.horizontal_scroll.unwrap_or(true),
            },
            show_diagnostics: project_panel.show_diagnostics.map_or_else(
                ShowDiagnostics::default,
                |show_diagnostics| match show_diagnostics {
                    settings::ProjectPanelShowDiagnostics::Off => ShowDiagnostics::Off,
                    settings::ProjectPanelShowDiagnostics::Errors => ShowDiagnostics::Errors,
                    settings::ProjectPanelShowDiagnostics::All => ShowDiagnostics::All,
                },
            ),
            hide_root: project_panel.hide_root.unwrap_or(false),
            hide_hidden: project_panel.hide_hidden.unwrap_or(false),
            drag_and_drop: project_panel.drag_and_drop.unwrap_or(true),
            auto_open: AutoOpenSettings {
                on_create: auto_open.on_create.unwrap_or(true),
                on_paste: auto_open.on_paste.unwrap_or(true),
                on_drop: auto_open.on_drop.unwrap_or(true),
            },
            sort_mode: project_panel.sort_mode.map_or_else(
                ProjectPanelSortMode::default,
                |sort_mode| match sort_mode {
                    settings::ProjectPanelSortMode::DirectoriesFirst => {
                        ProjectPanelSortMode::DirectoriesFirst
                    }
                    settings::ProjectPanelSortMode::Mixed => ProjectPanelSortMode::Mixed,
                    settings::ProjectPanelSortMode::FilesFirst => ProjectPanelSortMode::FilesFirst,
                },
            ),
            sort_order: project_panel.sort_order.map_or_else(
                ProjectPanelSortOrder::default,
                |sort_order| match sort_order {
                    settings::ProjectPanelSortOrder::Default => ProjectPanelSortOrder::Default,
                    settings::ProjectPanelSortOrder::Upper => ProjectPanelSortOrder::Upper,
                    settings::ProjectPanelSortOrder::Lower => ProjectPanelSortOrder::Lower,
                    settings::ProjectPanelSortOrder::Unicode => ProjectPanelSortOrder::Unicode,
                },
            ),
            diagnostic_badges: project_panel.diagnostic_badges.unwrap_or(false),
            git_status_indicator: project_panel.git_status_indicator.unwrap_or(false),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use settings::{
        ProjectPanelAutoOpenSettings, ProjectPanelIndentGuidesSettings,
        ProjectPanelScrollbarSettingsContent, ProjectPanelSettingsContent, SettingsContent,
    };

    fn content_with_project_panel_settings() -> SettingsContent {
        let mut content = SettingsContent::default();
        content.project_panel = Some(ProjectPanelSettingsContent {
            button: Some(false),
            hide_gitignore: Some(true),
            default_width: Some(360.0),
            dock: Some(settings::DockSide::Right),
            entry_spacing: Some(settings::ProjectPanelEntrySpacing::Standard),
            file_icons: Some(false),
            folder_icons: Some(false),
            git_status: Some(false),
            indent_size: Some(32.0),
            indent_guides: Some(ProjectPanelIndentGuidesSettings {
                show: Some(settings::ShowIndentGuides::Never),
            }),
            sticky_scroll: Some(false),
            auto_reveal_entries: Some(false),
            auto_fold_dirs: Some(false),
            bold_folder_labels: Some(true),
            starts_open: Some(false),
            scrollbar: Some(ProjectPanelScrollbarSettingsContent {
                show: Some(settings::ShowScrollbar::Always),
                horizontal_scroll: Some(false),
            }),
            show_diagnostics: Some(settings::ProjectPanelShowDiagnostics::Errors),
            hide_root: Some(true),
            hide_hidden: Some(true),
            drag_and_drop: Some(false),
            auto_open: Some(ProjectPanelAutoOpenSettings {
                on_create: Some(false),
                on_drop: Some(false),
                ..Default::default()
            }),
            sort_mode: Some(settings::ProjectPanelSortMode::FilesFirst),
            sort_order: Some(settings::ProjectPanelSortOrder::Unicode),
            diagnostic_badges: Some(true),
            git_status_indicator: Some(true),
        });
        content
    }

    #[test]
    fn test_reads_top_level_project_panel_settings_from_content() {
        let settings = ProjectPanelSettings::from_settings(&content_with_project_panel_settings());

        assert!(!settings.button);
        assert!(settings.hide_gitignore);
        assert_eq!(settings.default_width, px(360.0));
        assert_eq!(settings.dock, DockSide::Right);
        assert_eq!(settings.entry_spacing, ProjectPanelEntrySpacing::Standard);
        assert!(!settings.file_icons);
        assert!(!settings.folder_icons);
        assert!(!settings.git_status);
        assert_eq!(settings.indent_size, 32.0);
        assert!(!settings.sticky_scroll);
        assert!(!settings.auto_reveal_entries);
        assert!(!settings.auto_fold_dirs);
        assert!(settings.bold_folder_labels);
        assert!(!settings.starts_open);
        assert_eq!(settings.show_diagnostics, ShowDiagnostics::Errors);
        assert!(settings.hide_root);
        assert!(settings.hide_hidden);
        assert!(!settings.drag_and_drop);
        assert_eq!(settings.sort_mode, ProjectPanelSortMode::FilesFirst);
        assert_eq!(settings.sort_order, ProjectPanelSortOrder::Unicode);
        assert!(settings.diagnostic_badges);
        assert!(settings.git_status_indicator);
    }

    #[test]
    fn test_reads_nested_project_panel_settings_from_content() {
        let settings = ProjectPanelSettings::from_settings(&content_with_project_panel_settings());

        assert_eq!(settings.indent_guides.show, ShowIndentGuides::Never);

        assert_eq!(settings.scrollbar.show, Some(ShowScrollbar::Always));
        assert!(!settings.scrollbar.horizontal_scroll);

        assert!(!settings.auto_open.should_open_on_create());
        assert!(!settings.auto_open.should_open_on_drop());
        // Sibling fields the user did not mention keep their own defaults.
        assert!(settings.auto_open.should_open_on_paste());
    }

    #[test]
    fn test_falls_back_to_defaults_when_unset() {
        let settings = ProjectPanelSettings::from_settings(&SettingsContent::default());

        assert!(settings.button);
        assert!(!settings.hide_gitignore);
        assert_eq!(settings.default_width, px(240.0));
        assert_eq!(settings.dock, DockSide::Left);
        assert_eq!(
            settings.entry_spacing,
            ProjectPanelEntrySpacing::Comfortable
        );
        assert!(settings.file_icons);
        assert!(settings.folder_icons);
        assert!(settings.git_status);
        assert_eq!(settings.indent_size, 20.0);
        assert_eq!(settings.indent_guides.show, ShowIndentGuides::Always);
        assert!(settings.sticky_scroll);
        assert!(settings.auto_reveal_entries);
        assert!(settings.auto_fold_dirs);
        assert!(!settings.bold_folder_labels);
        assert!(settings.starts_open);
        assert_eq!(settings.scrollbar.show, None);
        assert!(settings.scrollbar.horizontal_scroll);
        assert_eq!(settings.show_diagnostics, ShowDiagnostics::Off);
        assert!(!settings.hide_root);
        assert!(!settings.hide_hidden);
        assert!(settings.drag_and_drop);
        assert!(settings.auto_open.should_open_on_create());
        assert!(settings.auto_open.should_open_on_paste());
        assert!(settings.auto_open.should_open_on_drop());
        assert_eq!(settings.sort_mode, ProjectPanelSortMode::DirectoriesFirst);
        assert_eq!(settings.sort_order, ProjectPanelSortOrder::Default);
        assert!(!settings.diagnostic_badges);
        assert!(!settings.git_status_indicator);
    }

    #[test]
    fn test_parses_project_panel_settings_from_json() {
        let content: SettingsContent = settings::parse_json_with_comments(
            r#"{
                "project_panel": {
                    "dock": "right",
                    "entry_spacing": "standard",
                    "default_width": 300,
                    "indent_guides": { "show": "never" },
                    "scrollbar": { "show": "never" },
                    "show_diagnostics": "errors",
                    "sort_mode": "files_first",
                    "sort_order": "unicode",
                    "auto_open": { "on_paste": false }
                }
            }"#,
        )
        .expect("project panel settings should parse");

        let settings = ProjectPanelSettings::from_settings(&content);

        assert_eq!(settings.dock, DockSide::Right);
        assert_eq!(settings.entry_spacing, ProjectPanelEntrySpacing::Standard);
        assert_eq!(settings.default_width, px(300.0));
        assert_eq!(settings.indent_guides.show, ShowIndentGuides::Never);
        assert_eq!(settings.scrollbar.show, Some(ShowScrollbar::Never));
        assert!(settings.scrollbar.horizontal_scroll);
        assert_eq!(settings.show_diagnostics, ShowDiagnostics::Errors);
        assert_eq!(settings.sort_mode, ProjectPanelSortMode::FilesFirst);
        assert_eq!(settings.sort_order, ProjectPanelSortOrder::Unicode);
        assert!(!settings.auto_open.should_open_on_paste());
        assert!(settings.auto_open.should_open_on_create());
    }

    /// Guards the JSON shape written in `assets/settings/default.json`: every
    /// key there has to deserialize, and the values have to agree with the
    /// fallbacks `from_settings` applies when the key is absent.
    ///
    /// Parsed through `UserSettingsContent` rather than `SettingsContent` so the
    /// test walks the same flattening that `SettingsStore` uses at startup.
    #[test]
    fn test_default_json_project_panel_section_matches_fallbacks() {
        let user_content = <settings::UserSettingsContent as settings::RootUserSettings>::parse_json_with_comments(
            settings::default_settings().as_ref(),
        )
        .expect("assets/settings/default.json should parse");
        let content = *user_content.content;
        let project_panel = content
            .project_panel
            .as_ref()
            .expect("assets/settings/default.json should define a `project_panel` section");

        let from_default_json = ProjectPanelSettings::from_settings(&content);
        let from_fallbacks = ProjectPanelSettings::from_settings(&SettingsContent::default());

        assert_eq!(
            from_default_json, from_fallbacks,
            "project_panel in default.json disagrees with the Rust fallbacks"
        );

        // A misspelled key deserializes to `None`, so spot-check that the
        // nested sections really were populated rather than skipped.
        assert!(project_panel.indent_guides.is_some());
        assert!(project_panel.scrollbar.is_some());
        assert!(project_panel.auto_open.is_some());
        assert!(project_panel.dock.is_some());
        assert!(project_panel.sort_mode.is_some());
    }

    /// A user who overrides one key must keep the rest of `default.json`, which
    /// only holds if the nested `Option`s merge recursively instead of being
    /// replaced wholesale.
    #[test]
    fn test_user_override_merges_into_default_json() {
        use settings::MergeFromTrait as _;

        let mut merged = <settings::UserSettingsContent as settings::RootUserSettings>::parse_json_with_comments(
            settings::default_settings().as_ref(),
        )
        .map(|user_content| *user_content.content)
        .expect("assets/settings/default.json should parse");

        let user: SettingsContent = settings::parse_json_with_comments(
            r#"{ "project_panel": { "auto_open": { "on_drop": false }, "dock": "right" } }"#,
        )
        .expect("user settings should parse");
        merged.merge_from(&user);

        let settings = ProjectPanelSettings::from_settings(&merged);

        assert!(!settings.auto_open.should_open_on_drop());
        assert_eq!(settings.dock, DockSide::Right);
        // Sibling keys defined only in default.json survive the merge.
        assert!(settings.auto_open.should_open_on_create());
        assert!(settings.auto_open.should_open_on_paste());
        assert_eq!(settings.default_width, px(240.0));
        assert_eq!(settings.indent_guides.show, ShowIndentGuides::Always);
    }
}
