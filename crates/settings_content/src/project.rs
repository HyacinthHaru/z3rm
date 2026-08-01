use crate::{
    GitHostingProviderConfig, LanguageToSettingsMap, ProjectTerminalSettingsContent, SaturatingBool,
};
use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};
use std::path::PathBuf;

/// 项目基础设置 (spec §16 Plan 16)
/// 保留项目索引、排除路径等基本设置。
#[with_fallible_options]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct ProjectSettingsContent {
    /// Linked directories to index for file searching.
    pub linked_projects: Option<HashMap<PathBuf, LinkedProjectSettings>>,

    /// Paths to exclude from indexing and search results.
    pub excluded_paths: Option<Vec<PathBuf>>,

    /// Whether to scan symlinks when indexing content. Default: local_only
    pub scan_symlinks: ScanSymlinksSetting,

    // 兼容字段 - 已删除模块占位 (spec §16 Plan 16)
    pub all_languages: LanguageToSettingsMap,
    pub disable_ai: SaturatingBool,
    pub git_hosting_providers: Option<Vec<GitHostingProviderConfig>>,
    pub terminal: Option<ProjectTerminalSettingsContent>,
}

/// Settings for a linked project directory.
#[with_fallible_options]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct LinkedProjectSettings {
    /// Relative paths within the linked directory to include.
    pub paths: Option<Vec<PathBuf>>,
}

/// When to scan content of linked directories.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum ScanSymlinksSetting {
    /// Scan symlinks pointing to local directories only.
    #[default]
    LocalOnly,
    /// Scan all symlinks, including remote ones.
    All,
    /// Don't scan symlinks at all.
    Off,
}

impl crate::RootUserSettings for ProjectSettingsContent {
    fn parse_json(json: &str) -> (Option<Self>, crate::ParseStatus) {
        crate::fallible_options::parse_json(json)
    }
    fn parse_json_with_comments(json: &str) -> anyhow::Result<Self> {
        settings_json::parse_json_with_comments(json)
    }
}
