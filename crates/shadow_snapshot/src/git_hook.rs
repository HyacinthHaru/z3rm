//! §4.9 Git commit hook integration
//!
//! 检测 worktree 上发生的 git commit，驱动 `QuotaManager::on_git_commit`：
//! commit 之前的 delta 已经进了 git history，shadow snapshot 不必再为它们
//! 付配额，于是标记为 gc-eligible，下一轮 GC 优先回收。
//!
//! 检测方式是读 `.git` 里 HEAD 解析出的 commit id 并比对，而不是安装 git
//! hook 脚本：hook 脚本会被用户的 `core.hooksPath` / husky 之类的工具覆盖，
//! 而且远程 worktree 上我们未必有写 `.git` 的权限。文件监听是只读的、
//! 幂等的，并且对 `git commit --amend`、`rebase`、`merge` 一样有效。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::Watcher;
use parking_lot::Mutex;

/// 解析 worktree 根目录对应的 git 目录。
///
/// `.git` 通常是目录；在 submodule / linked worktree 中它是一行文本
/// `gitdir: <path>`，必须跟随过去，否则监听到的是一个不会变化的文件。
pub fn resolve_git_dir(worktree_root: &Path) -> Option<PathBuf> {
    let dot_git = worktree_root.join(".git");
    let metadata = std::fs::metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    let target_path = PathBuf::from(target);
    let resolved = if target_path.is_absolute() {
        target_path
    } else {
        worktree_root.join(target_path)
    };
    resolved.is_dir().then_some(resolved)
}

/// 读取 HEAD 当前指向的 commit id。
///
/// HEAD 要么是 `ref: refs/heads/<name>`（读 loose ref，缺失时回落到
/// `packed-refs`），要么是 detached HEAD 的裸 object id。
fn read_head_commit(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:") else {
        return (!head.is_empty()).then(|| head.to_string());
    };
    let reference = reference.trim();
    if let Ok(loose) = std::fs::read_to_string(git_dir.join(reference)) {
        let loose = loose.trim();
        if !loose.is_empty() {
            return Some(loose.to_string());
        }
    }
    read_packed_ref(git_dir, reference)
}

/// 在 `packed-refs` 中查找 ref。行格式是 `<oid> <refname>`；以 `#` 开头的是
/// 头部注释，以 `^` 开头的是被 peel 的 tag object，两者都跳过。
fn read_packed_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    for line in packed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let (object_id, name) = line.split_once(' ')?;
        if name.trim() == reference {
            return Some(object_id.to_string());
        }
    }
    None
}

/// 跟踪一个 worktree 的 HEAD commit，检测 commit 是否发生。
pub struct GitCommitTracker {
    git_dir: PathBuf,
    last_commit: Mutex<Option<String>>,
}

impl GitCommitTracker {
    /// worktree 不在 git 仓库中时返回 `None`（shadow snapshot 本来就要能在
    /// 非 git 目录工作，§4.1）。构造时把当前 commit 记为基线，避免刚启动就
    /// 误报一次 commit。
    pub fn new(worktree_root: &Path) -> Option<Self> {
        let git_dir = resolve_git_dir(worktree_root)?;
        let last_commit = read_head_commit(&git_dir);
        Some(Self {
            git_dir,
            last_commit: Mutex::new(last_commit),
        })
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// HEAD 指向的 commit 自上次 poll 后变化时返回新的 commit id。
    ///
    /// 未初始化仓库（还没有任何 commit）读不出 id，返回 `None`，
    /// 首个 commit 出现时才会触发一次。
    pub fn poll(&self) -> Option<String> {
        let current = read_head_commit(&self.git_dir)?;
        let mut last = self.last_commit.lock();
        if last.as_deref() == Some(current.as_str()) {
            return None;
        }
        *last = Some(current.clone());
        Some(current)
    }
}

/// `watch_git_commits` 返回的句柄。drop 即停止监听。
pub struct GitCommitWatcher {
    watcher: Option<notify::RecommendedWatcher>,
}

impl Drop for GitCommitWatcher {
    fn drop(&mut self) {
        drop(self.watcher.take());
    }
}

/// 监听 git 目录，检测到新的 HEAD commit 时调用 `on_commit`。
///
/// 只监听 git 目录本身（非递归，覆盖 `HEAD` / `COMMIT_EDITMSG` / `index`）
/// 与 `refs/`（递归，覆盖分支 tip 更新）。刻意不递归监听整个 `.git`：
/// `objects/` 在 fetch / gc 时会产生成千上万个事件，而它们和 HEAD 无关。
pub fn watch_git_commits(
    tracker: Arc<GitCommitTracker>,
    on_commit: impl Fn(String) + Send + 'static,
) -> io::Result<GitCommitWatcher> {
    let mut watcher = notify::recommended_watcher({
        let tracker = Arc::clone(&tracker);
        move |result: notify::Result<notify::Event>| match result {
            Ok(_event) => {
                // 事件只是"该重新读一次"的信号；权威状态是 HEAD 文件本身，
                // 所以这里不解析事件内容，直接 poll。
                if let Some(commit) = tracker.poll() {
                    on_commit(commit);
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "git commit watcher event error");
            }
        }
    })
    .map_err(io::Error::other)?;

    watcher
        .watch(tracker.git_dir(), notify::RecursiveMode::NonRecursive)
        .map_err(io::Error::other)?;
    let refs_dir = tracker.git_dir().join("refs");
    if refs_dir.is_dir() {
        if let Err(error) = watcher.watch(&refs_dir, notify::RecursiveMode::Recursive) {
            // refs/ 监听失败只降低灵敏度（仍有 COMMIT_EDITMSG / index 事件
            // 触发 poll），不该让整个 git 集成失败。
            tracing::warn!(error = %error, "git commit watcher: refs/ not watched");
        }
    }

    Ok(GitCommitWatcher {
        watcher: Some(watcher),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个最小可用的 git 目录布局：HEAD → refs/heads/main → oid。
    fn write_repository(root: &Path, commit: &str) {
        let git_dir = root.join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("create refs/heads");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(git_dir.join("refs/heads/main"), format!("{commit}\n"))
            .expect("write branch tip");
    }

    #[test]
    fn tracker_detects_new_commit_on_branch_tip() {
        let directory = tempfile::tempdir().expect("temp dir");
        write_repository(directory.path(), "1111111111111111111111111111111111111111");

        let tracker = GitCommitTracker::new(directory.path()).expect("git repository");
        assert!(
            tracker.poll().is_none(),
            "baseline must not report a commit"
        );

        write_repository(directory.path(), "2222222222222222222222222222222222222222");

        assert_eq!(
            tracker.poll().as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
        assert!(
            tracker.poll().is_none(),
            "same commit must report only once"
        );
    }

    #[test]
    fn tracker_reads_detached_head() {
        let directory = tempfile::tempdir().expect("temp dir");
        let git_dir = directory.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("create .git");
        std::fs::write(
            git_dir.join("HEAD"),
            "3333333333333333333333333333333333333333\n",
        )
        .expect("write detached HEAD");

        let tracker = GitCommitTracker::new(directory.path()).expect("git repository");
        assert!(tracker.poll().is_none());

        std::fs::write(
            git_dir.join("HEAD"),
            "4444444444444444444444444444444444444444\n",
        )
        .expect("move detached HEAD");
        assert_eq!(
            tracker.poll().as_deref(),
            Some("4444444444444444444444444444444444444444")
        );
    }

    #[test]
    fn tracker_falls_back_to_packed_refs() {
        let directory = tempfile::tempdir().expect("temp dir");
        let git_dir = directory.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("create .git");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(
            git_dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n\
             5555555555555555555555555555555555555555 refs/heads/main\n\
             ^6666666666666666666666666666666666666666\n",
        )
        .expect("write packed-refs");

        let tracker = GitCommitTracker::new(directory.path()).expect("git repository");
        assert_eq!(
            *tracker.last_commit.lock(),
            Some("5555555555555555555555555555555555555555".to_string())
        );
    }

    #[test]
    fn tracker_follows_gitdir_pointer_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let worktree = directory.path().join("worktree");
        let real_git_dir = directory.path().join("real-git");
        std::fs::create_dir_all(&worktree).expect("create worktree");
        std::fs::create_dir_all(real_git_dir.join("refs/heads")).expect("create real git dir");
        std::fs::write(real_git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(
            real_git_dir.join("refs/heads/main"),
            "7777777777777777777777777777777777777777\n",
        )
        .expect("write branch tip");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", real_git_dir.display()),
        )
        .expect("write gitdir pointer");

        let tracker = GitCommitTracker::new(&worktree).expect("linked worktree resolves");
        assert_eq!(tracker.git_dir(), real_git_dir);
    }

    #[test]
    fn non_git_directory_has_no_tracker() {
        let directory = tempfile::tempdir().expect("temp dir");
        assert!(GitCommitTracker::new(directory.path()).is_none());
    }
}
