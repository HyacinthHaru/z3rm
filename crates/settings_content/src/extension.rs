use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use settings_macros::{MergeFrom, with_fallible_options};
use std::path::PathBuf;

/// 扩展设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct ExtensionSettingsContent {
    /// Directory where extensions are stored. Default: "~/.config/z3rm/extensions"
    pub directory: PathBuf,

    /// Whether to automatically sync extensions to remote servers. Default: true
    pub auto_sync_to_remote: bool,
}
