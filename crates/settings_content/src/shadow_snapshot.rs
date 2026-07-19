use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// 影子快照设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct ShadowSnapshotSettingsContent {
    /// Whether shadow snapshots are enabled. Default: true
    pub enabled: bool,

    /// Quota mode for shadow snapshots. Default: "per_project"
    pub quota_mode: QuotaMode,

    /// Per-project quota in megabytes. Default: 500
    pub per_project_quota_mb: usize,

    /// Glob patterns to ignore when creating snapshots.
    pub ignore_patterns: Vec<String>,

    /// Whether to detect and skip binary files. Default: true
    pub binary_detection: bool,

    /// Debounce interval in milliseconds for file change events. Default: 500
    pub debounce_ms: u64,

    /// Circuit breaker threshold for file change frequency. Default: 10
    pub frequency_circuit_breaker_k: usize,

    /// Git commit hook behavior. Default: "clear"
    pub git_commit_hook: GitCommitHook,
}

/// Quota mode for shadow snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum QuotaMode {
    /// Each project has its own quota.
    #[default]
    PerProject,
    /// All projects share a single global quota.
    Global,
}

/// Git commit hook behavior for shadow snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum GitCommitHook {
    /// Clear the snapshot after a successful git commit.
    #[default]
    Clear,
    /// Keep the snapshot after a git commit.
    Keep,
    /// Never create snapshots during git commit operations.
    Skip,
}
