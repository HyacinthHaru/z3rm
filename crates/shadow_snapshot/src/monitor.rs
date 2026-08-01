//! Monitor：worktree 事件订阅 + ignore filter + 频率熔断器
//!
//! 单订阅，不重复监听。
//! 默认忽略列表 + .z3rmignore + .gitignore。
//! 二进制文件检测（ELF, PE, Mach-O magic）。
//! 频率熔断：K writes/sec → suspend 2s idle。
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use notify::Watcher;
use parking_lot::Mutex;

use crate::version_tree::SnapshotTrigger;

/// 默认忽略模式
const DEFAULT_IGNORE: &[&str] = &[
    ".git/",
    "node_modules/",
    "*.pyc",
    "__pycache__/",
    "*.o",
    "*.so",
    "*.dylib",
    "*.dll",
    "*.class",
    "*.exe",
    "target/",
    "build/",
    "*.log",
    "*.tmp",
    "*.swp",
    "*~",
    ".DS_Store",
    "Thumbs.db",
];

/// 二进制文件 magic 签名
const ELF_MAGIC: &[u8] = b"\x7fELF";
const PE_MAGIC: &[u8] = b"MZ";
const MACHO_MAGIC: [u8; 4] = [0xfe, 0xed, 0xfa, 0xce];

/// 频率熔断器参数
const CIRCUIT_K: f64 = 50.0; // 每秒最大写入次数
const CIRCUIT_SUSPEND: Duration = Duration::from_secs(2); // 熔断后暂停 2 秒

/// 文件变更事件
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// 文件路径
    pub path: PathBuf,
    /// 事件类型
    pub kind: EventKind,
    /// 时间戳
    pub timestamp: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// 工作树监控器
pub struct Monitor {
    /// 忽略路径过滤器
    ignore_filter: IgnoreFilter,
    /// 频率熔断器
    circuit_breaker: Mutex<CircuitBreaker>,
    /// 事件回调
    on_event: Box<dyn Fn(FileEvent) -> SnapshotTrigger + Send + Sync>,
}

// Safety: Monitor fields are Send+Sync safe:
// - IgnoreFilter: PathBuf + Mutex<Vec<String>>
// - circuit_breaker: Mutex<CircuitBreaker>
// - on_event: Box<dyn Fn(...) + Send + Sync>
unsafe impl Send for Monitor {}
unsafe impl Sync for Monitor {}

impl Monitor {
    /// 创建监控器
    ///
    /// worktree_root: 工作树根目录
    /// on_event: 事件回调，返回应触发的 SnapshotTrigger
    pub fn new(
        worktree_root: impl Into<PathBuf>,
        on_event: impl Fn(FileEvent) -> SnapshotTrigger + Send + Sync + 'static,
    ) -> Self {
        Self {
            ignore_filter: IgnoreFilter::new(worktree_root),
            circuit_breaker: Mutex::new(CircuitBreaker::new()),
            on_event: Box::new(on_event),
        }
    }

    /// 处理文件变更事件
    ///
    /// 1. 检查忽略规则
    /// 2. 检查二进制文件
    /// 3. 检查频率熔断
    /// 4. 触发快照
    pub fn handle_event(&self, event: FileEvent) -> Option<SnapshotTrigger> {
        // 1. 忽略规则检查
        if self.ignore_filter.should_ignore(&event.path) {
            return None;
        }

        // 2. 二进制文件检测
        if Self::is_binary_file(&event.path) {
            return None;
        }

        // 3. 频率熔断检查
        let mut cb = self.circuit_breaker.lock();
        if cb.check(&event.path) {
            return None;
        }

        // 4. 触发快照
        let trigger = (self.on_event)(event.clone());
        Some(trigger)
    }

    /// 检测文件是否为二进制
    ///
    /// 检查 ELF magic、PE magic、Mach-O magic
    pub fn is_binary_file(path: &Path) -> bool {
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };

        let mut header = [0u8; 20];
        let Ok(n) = file.read(&mut header) else {
            return false;
        };

        if n >= 4 {
            // ELF
            if header.starts_with(ELF_MAGIC) {
                return true;
            }
            // Mach-O
            if header[..4] == MACHO_MAGIC {
                return true;
            }
        }

        if n >= 2 && header.starts_with(PE_MAGIC) {
            return true;
        }

        // 额外检查：文件前 512 字节中 null 字节比例
        let mut content = Vec::new();
        if file.read_to_end(&mut content).is_ok() && !content.is_empty() {
            let null_count = content.iter().filter(|&&b| b == 0).count();
            if null_count as f64 / content.len() as f64 > 0.1 {
                return true;
            }
        }

        false
    }

    /// 添加自定义忽略模式
    pub fn add_ignore_pattern(&self, pattern: &str) {
        self.ignore_filter.add_pattern(pattern);
    }
    /// Start watching a directory using the notify crate.
    ///
    /// Returns a handle that can be dropped to stop watching.
    /// The watcher runs on a background thread and dispatches
    /// filtered events through this monitor's pipeline.
    pub fn watch_directory(self: &Arc<Self>, root: PathBuf) -> std::io::Result<WatchHandle> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        watcher
            .watch(&root, notify::RecursiveMode::Recursive)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Clone Arc<Self> into the background thread. The thread processes
        // events through this monitor's ignore-filter + circuit-breaker pipeline.
        let monitor = self.clone();
        std::thread::Builder::new()
            .name("fs-watcher".into())
            .spawn(move || {
                for event in rx.iter() {
                    for path in event.paths {
                        let kind = match event.kind {
                            notify::EventKind::Create(_) => EventKind::Created,
                            notify::EventKind::Modify(_) => EventKind::Modified,
                            notify::EventKind::Remove(_) => EventKind::Deleted,
                            notify::EventKind::Any => EventKind::Modified,
                            _ => continue,
                        };
                        let file_event = FileEvent {
                            path,
                            kind,
                            timestamp: Instant::now(),
                        };
                        let _ = monitor.handle_event(file_event);
                    }
                }
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        Ok(WatchHandle {
            watcher: Some(watcher),
        })
    }
}

/// Handle returned by `Monitor::watch_directory`. Drop to stop watching.
pub struct WatchHandle {
    watcher: Option<notify::RecommendedWatcher>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        // notify::RecommendedWatcher stops watching on Drop.
        // Dropping the watcher also closes the channel sender,
        // which causes the background thread to exit.
        drop(self.watcher.take());
    }
}

/// 忽略路径过滤器
pub struct IgnoreFilter {
    /// 工作树根目录
    worktree_root: PathBuf,
    /// 忽略模式列表
    patterns: Mutex<Vec<String>>,
}

impl IgnoreFilter {
    fn new(worktree_root: impl Into<PathBuf>) -> Self {
        let root = worktree_root.into();
        let mut patterns = Vec::new();
        for p in DEFAULT_IGNORE {
            patterns.push(p.to_string());
        }
        // §4.7 honor project ignore files. `.z3rmignore` overrides; `.gitignore`
        // is the conventional project ignore set. Both are best-effort: a
        // missing/unreadable file is silently skipped (logged at debug).
        Self::load_ignore_file(&root, ".z3rmignore", &mut patterns);
        Self::load_ignore_file(&root, ".gitignore", &mut patterns);
        Self {
            worktree_root: root,
            patterns: Mutex::new(patterns),
        }
    }

    /// Read a gitignore-style ignore file from `root/<name>` and append each
    /// non-comment, non-empty line as a pattern. I/O errors are debug-logged
    /// and skipped — a missing file is the common case, not an error.
    fn load_ignore_file(root: &Path, name: &str, patterns: &mut Vec<String>) {
        let path = root.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    patterns.push(trimmed.to_string());
                }
                tracing::debug!(file = name, "loaded project ignore file");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(file = name, error = %error, "skip unreadable ignore file");
            }
        }
    }

    fn add_pattern(&self, pattern: &str) {
        self.patterns.lock().push(pattern.to_string());
    }

    fn should_ignore(&self, path: &Path) -> bool {
        let patterns = self.patterns.lock();

        for pattern in patterns.iter() {
            if Self::matches_pattern(path, pattern) {
                return true;
            }
        }

        false
    }

    /// 简单模式匹配
    fn matches_pattern(path: &Path, pattern: &str) -> bool {
        let path_str = match path.to_str() {
            Some(s) => s,
            None => return false,
        };

        if pattern.ends_with('/') {
            // 目录匹配
            let dir_pattern = pattern.trim_end_matches('/');
            path_str.contains(dir_pattern)
        } else if pattern.starts_with("*") {
            // 后缀匹配
            let suffix = &pattern[1..];
            path_str.ends_with(suffix)
        } else {
            path_str.contains(pattern)
        }
    }
}

/// 频率熔断器
///
/// K writes/sec → suspend snapshotting for that file until 2s idle.
struct CircuitBreaker {
    /// 每个文件的写入计数和时间窗口
    counts: HashMap<PathBuf, (u32, Instant)>,
}

impl CircuitBreaker {
    fn new() -> Self {
        Self {
            counts: HashMap::new(),
        }
    }

    /// 检查是否应该熔断
    ///
    /// 返回 true 表示已熔断（跳过快照）
    fn check(&mut self, path: &Path) -> bool {
        let now = Instant::now();
        let entry = self.counts.entry(path.to_path_buf()).or_insert((0, now));

        // 如果上次写入超过 2 秒，重置计数
        if now.duration_since(entry.1) > CIRCUIT_SUSPEND {
            entry.0 = 0;
            entry.1 = now;
        }

        // 递增计数
        entry.0 += 1;

        // 检查是否超过阈值
        let elapsed = now.duration_since(entry.1).as_secs_f64();
        if elapsed > 0.0 {
            let rate = entry.0 as f64 / elapsed;
            if rate > CIRCUIT_K {
                // 熔断：重置窗口
                entry.0 = 0;
                entry.1 = now;
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_detection_elf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.elf");
        std::fs::write(&path, ELF_MAGIC).unwrap();

        assert!(Monitor::is_binary_file(&path));
    }

    #[test]
    fn test_binary_detection_pe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.exe");
        std::fs::write(&path, PE_MAGIC).unwrap();

        assert!(Monitor::is_binary_file(&path));
    }

    #[test]
    fn test_text_file_not_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "Hello, World!\n").unwrap();

        assert!(!Monitor::is_binary_file(&path));
    }

    #[test]
    fn test_ignore_filter_default() {
        let filter = IgnoreFilter::new("/tmp/test");

        assert!(filter.should_ignore(&PathBuf::from("/tmp/test/.git/HEAD")));
        assert!(filter.should_ignore(&PathBuf::from("/tmp/test/node_modules/pkg/index.js")));
        assert!(filter.should_ignore(&PathBuf::from("/tmp/test/main.pyc")));
        assert!(!filter.should_ignore(&PathBuf::from("/tmp/test/src/main.rs")));
    }

    #[test]
    fn test_ignore_filter_loads_project_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".z3rmignore"),
            "# z3rm-specific\nprivate-cache/\n*.secret\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "# project-wide\ngenerated/\n*.bak\n",
        )
        .unwrap();

        let filter = IgnoreFilter::new(dir.path());

        assert!(filter.should_ignore(&dir.path().join("private-cache/data.bin")));
        assert!(filter.should_ignore(&dir.path().join("token.secret")));
        assert!(filter.should_ignore(&dir.path().join("generated/output.rs")));
        assert!(filter.should_ignore(&dir.path().join("notes.bak")));
        assert!(!filter.should_ignore(&dir.path().join("src/main.rs")));
    }

    #[test]
    fn test_circuit_breaker() {
        let mut cb = CircuitBreaker::new();
        let path = PathBuf::from("/tmp/test.txt");

        // 第一次调用应通过（elapsed=0，不检查阈值）
        assert!(!cb.check(&path));

        // 等待 2 秒重置窗口
        std::thread::sleep(Duration::from_millis(2001));

        // 窗口重置后应通过
        assert!(!cb.check(&path));
    }
}
