//! §4 引擎运行时配置。
//!
//! 值来自用户设置 `settings.json` 的 `shadow_snapshot` 段（见
//! `settings_content::ShadowSnapshotSettingsContent`），由宿主进程解析后构造
//! `SnapshotConfig` 传进来。这里刻意不依赖 serde / settings_content：引擎不该
//! 和设置系统的类型互相绑定，宿主只负责把用户值翻译成这个结构。

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::quota::{GlobalQuotaLedger, QuotaManager};

/// §4.9 默认配额：每个 project 500 MB。
pub const DEFAULT_QUOTA_BYTES: u64 = 500 * 1024 * 1024;

/// §4.7 默认 debounce 窗口。
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

/// §4.7 默认频率熔断阈值：单文件每秒写入次数上限。
pub const DEFAULT_CIRCUIT_BREAKER_K: f64 = 10.0;

/// §4.9 配额作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuotaMode {
    /// 每个 project（session）各自持有一份 `quota_bytes` 预算。
    #[default]
    PerProject,
    /// 所有 project 共享同一份 `quota_bytes` 预算，用量跨引擎累加。
    Global,
}

/// §4.9 git commit hook 行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitCommitHookMode {
    /// commit 之后把 pre-commit 的 delta 版本标记为 gc-eligible。
    #[default]
    Clear,
    /// 检测 commit 但不标记任何版本，历史全部保留。
    Keep,
    /// 完全不做 git 集成：不监听 `.git`，不标记。
    Skip,
}

/// §4 引擎 + monitor 的运行时配置。
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// 关闭后宿主不会为该 worktree 启动引擎与 watcher。
    pub enabled: bool,
    /// 配额作用域。
    pub quota_mode: QuotaMode,
    /// 配额字节数。0 表示不限额（§4.9 "configurable to unlimited"），
    /// 此时不安装 `QuotaManager`，GC 完全不运行。
    pub quota_bytes: u64,
    /// 追加到默认忽略列表之后的用户模式。
    pub ignore_patterns: Vec<String>,
    /// 是否做二进制探测（ELF/PE/Mach-O magic + null 字节比例）。
    pub binary_detection: bool,
    /// 每路径 debounce 窗口。
    pub debounce: Duration,
    /// 频率熔断阈值（次/秒）。
    pub circuit_breaker_writes_per_second: f64,
    /// git commit hook 行为。
    pub git_commit_hook: GitCommitHookMode,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            quota_mode: QuotaMode::PerProject,
            quota_bytes: DEFAULT_QUOTA_BYTES,
            ignore_patterns: Vec::new(),
            binary_detection: true,
            debounce: DEFAULT_DEBOUNCE,
            circuit_breaker_writes_per_second: DEFAULT_CIRCUIT_BREAKER_K,
            git_commit_hook: GitCommitHookMode::Clear,
        }
    }
}

impl SnapshotConfig {
    /// 按配置构造 quota manager。
    ///
    /// `quota_bytes == 0` 表示不限额 → `None`，引擎不会运行 GC。
    /// `QuotaMode::Global` 让所有引擎共用进程级用量账本，因此任一 session
    /// 的 GC 判断的是全局总量而不是自己那份。
    pub fn quota_manager(&self) -> Option<QuotaManager> {
        if self.quota_bytes == 0 {
            return None;
        }
        Some(match self.quota_mode {
            QuotaMode::PerProject => QuotaManager::new(self.quota_bytes),
            QuotaMode::Global => {
                QuotaManager::with_shared_ledger(self.quota_bytes, global_quota_ledger())
            }
        })
    }
}

/// `QuotaMode::Global` 用的进程级账本。
///
/// 每个引擎只能删自己的节点，所以"共享配额"实现为：各引擎把自己的实际占用
/// 报进账本，GC 时以全局总量（而非自身占用）判断是否超额。
fn global_quota_ledger() -> Arc<GlobalQuotaLedger> {
    static LEDGER: OnceLock<Arc<GlobalQuotaLedger>> = OnceLock::new();
    Arc::clone(LEDGER.get_or_init(GlobalQuotaLedger::new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_documented_settings_defaults() {
        let config = SnapshotConfig::default();
        assert!(config.enabled);
        assert_eq!(config.quota_bytes, 500 * 1024 * 1024);
        assert_eq!(config.debounce, Duration::from_millis(500));
        assert!(config.binary_detection);
        assert_eq!(config.circuit_breaker_writes_per_second, 10.0);
        assert_eq!(config.git_commit_hook, GitCommitHookMode::Clear);
        assert_eq!(config.quota_mode, QuotaMode::PerProject);
    }

    #[test]
    fn zero_quota_means_unlimited() {
        let config = SnapshotConfig {
            quota_bytes: 0,
            ..SnapshotConfig::default()
        };
        assert!(config.quota_manager().is_none());
    }

    #[test]
    fn global_mode_shares_one_ledger_across_engines() {
        let config = SnapshotConfig {
            quota_mode: QuotaMode::Global,
            quota_bytes: 4096,
            ..SnapshotConfig::default()
        };
        let first = config.quota_manager().expect("quota installed");
        let second = config.quota_manager().expect("quota installed");

        // 两个引擎各报 3000 字节：任一引擎看到的预算用量是 6000（全局），
        // 而 per-project 模式下各自只会看到 3000。
        assert_eq!(first.budget_usage(3000), 3000);
        assert_eq!(second.budget_usage(3000), 6000);

        let per_project = SnapshotConfig {
            quota_mode: QuotaMode::PerProject,
            quota_bytes: 4096,
            ..SnapshotConfig::default()
        }
        .quota_manager()
        .expect("quota installed");
        assert_eq!(per_project.budget_usage(3000), 3000);
    }
}
