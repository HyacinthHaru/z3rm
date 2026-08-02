use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};
use std::path::PathBuf;
use std::sync::Arc;

/// 扩展设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct ExtensionSettingsContent {
    /// Directory where extensions are stored. Default: "~/.config/z3rm/extensions"
    pub directory: PathBuf,

    /// Whether to automatically sync extensions to remote servers. Default: true
    pub auto_sync_to_remote: bool,

    /// The extensions that should be automatically installed, keyed by extension id.
    ///
    /// Default: {}
    pub auto_install_extensions: HashMap<Arc<str>, bool>,

    /// Whether an installed extension should be automatically updated, keyed by
    /// extension id. Extensions not listed here are updated automatically.
    ///
    /// Default: {}
    pub auto_update_extensions: HashMap<Arc<str>, bool>,

    /// The capabilities the extension host grants to extensions. An extension can
    /// only use a capability that both its manifest requests and this list grants.
    ///
    /// Default: []
    pub granted_capabilities: Vec<ExtensionCapabilityContent>,
}

/// A capability granted to extensions by the extension host.
///
/// The serialized shape mirrors `extension::ExtensionCapability` so both sides
/// read the same JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCapabilityContent {
    /// Allows executing a command with the given arguments. `*` matches a single
    /// argument, a trailing `**` matches any remaining arguments.
    #[serde(rename = "process:exec")]
    ProcessExec { command: String, args: Vec<String> },

    /// Allows downloading files from the given host and path prefix.
    DownloadFile { host: String, path: Vec<String> },

    /// Allows installing the given NPM package. `*` matches any package.
    #[serde(rename = "npm:install")]
    NpmInstallPackage { package: String },
}
