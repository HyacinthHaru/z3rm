//! §4.9 Git commit hook integration
//!
//! 在 `git commit` 后标记 pre-commit deltas 为 gc-eligible。
//! 下一次 GC cycle 优先回收 gc-eligible nodes。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::version_tree::VersionId;
use parking_lot::Mutex;

/// Git commit hook handler (§4.9)
///
/// 在 git commit 后标记 pre-commit deltas 为 gc-eligible。
pub struct GitCommitHook {
    /// 已标记为 gc-eligible 的 version IDs
    gc_eligible: Mutex<HashSet<VersionId>>,
    /// 是否已触发 commit hook (用于检测)
    commit_triggered: AtomicBool,
}

impl GitCommitHook {
    /// 创建 git commit hook handler (§4.9)
    pub fn new() -> Self {
        Self {
            gc_eligible: Mutex::new(HashSet::new()),
            commit_triggered: AtomicBool::new(false),
        }
    }

    /// 标记指定版本为 gc-eligible (§4.9)
    ///
    /// 在 `git commit` 后调用，标记 commit 前的 deltas。
    pub fn mark_pre_commit_deltas(&self, versions: &[VersionId]) {
        let mut eligible = self.gc_eligible.lock();
        for version in versions {
            eligible.insert(*version);
        }
        self.commit_triggered.store(true, Ordering::SeqCst);
    }

    /// 检查版本是否标记为 gc-eligible
    pub fn is_gc_eligible(&self, version: &VersionId) -> bool {
        let eligible = self.gc_eligible.lock();
        eligible.contains(version)
    }

    /// 获取所有 gc-eligible 版本 (用于 GC 优先处理)
    pub fn take_eligible_versions(&self) -> HashSet<VersionId> {
        let mut eligible = self.gc_eligible.lock();
        eligible.drain().collect()
    }

    /// 重置状态
    pub fn reset(&self) {
        let mut eligible = self.gc_eligible.lock();
        eligible.clear();
        self.commit_triggered.store(false, Ordering::SeqCst);
    }

    /// 检查 commit hook 是否已触发
    pub fn is_commit_triggered(&self) -> bool {
        self.commit_triggered.load(Ordering::SeqCst)
    }
}

impl Default for GitCommitHook {
    fn default() -> Self {
        Self::new()
    }
}

/// Arc 包装，便于跨线程共享
pub type SharedGitCommitHook = Arc<GitCommitHook>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_pre_commit_deltas() {
        let hook = GitCommitHook::new();

        let versions: [VersionId; 3] = [1, 2, 3];
        hook.mark_pre_commit_deltas(&versions);

        for version in &versions {
            assert!(hook.is_gc_eligible(version));
        }
        assert!(hook.is_commit_triggered());
    }

    #[test]
    fn test_take_eligible_versions() {
        let hook = GitCommitHook::new();

        let versions: [VersionId; 2] = [1, 2];
        hook.mark_pre_commit_deltas(&versions);

        let taken = hook.take_eligible_versions();
        assert!(taken.contains(&1));
        assert!(taken.contains(&2));

        // 取走后不再标记
        assert!(!hook.is_gc_eligible(&1));
    }

    #[test]
    fn test_reset() {
        let hook = GitCommitHook::new();

        hook.mark_pre_commit_deltas(&[1]);
        hook.reset();

        assert!(!hook.is_gc_eligible(&1));
        assert!(!hook.is_commit_triggered());
    }
}
