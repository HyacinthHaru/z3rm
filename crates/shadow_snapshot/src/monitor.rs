//! Monitor：worktree 事件订阅 + ignore filter + 频率熔断器
//!
//! 单订阅，不重复监听。
//! 默认忽略列表 + .z3rmignore + .gitignore。
//! 二进制文件检测（ELF, PE, Mach-O magic）。
//! 频率熔断：K writes/sec → suspend 2s idle。
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::sync::{mpsc, Arc};

use anyhow::Result;
use notify::Watcher;
use parking_lot::Mutex;

use crate::config::SnapshotConfig;
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
const CIRCUIT_WINDOW: Duration = Duration::from_secs(1); // 计数窗口 1 秒
const CIRCUIT_SUSPEND: Duration = Duration::from_secs(2); // 熔断后需安静 2 秒

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
    /// 写入后关闭文件（inotify `IN_CLOSE_WRITE`）。§4.7 要求 close 强制 flush
    /// 一个版本，因此它和 Modified 是两种事件而不是同一种。
    Closed,
    Deleted,
    Renamed,
}

/// 工作树监控器
pub struct Monitor {
    /// 忽略路径过滤器
    ignore_filter: IgnoreFilter,
    /// 频率熔断器
    circuit_breaker: Mutex<CircuitBreaker>,
    /// 是否做二进制探测（`shadow_snapshot.binary_detection`）
    binary_detection: bool,
    /// 事件回调
    on_event: Box<dyn Fn(FileEvent) -> Result<SnapshotTrigger> + Send + Sync>,
}

impl Monitor {
    /// 用默认配置创建监控器
    ///
    /// worktree_root: 工作树根目录
    /// on_event: 事件回调，返回应触发的 SnapshotTrigger
    pub fn new(
        worktree_root: impl Into<PathBuf>,
        on_event: impl Fn(FileEvent) -> Result<SnapshotTrigger> + Send + Sync + 'static,
    ) -> Self {
        Self::with_config(worktree_root, &SnapshotConfig::default(), on_event)
    }

    /// 用用户设置创建监控器（§4.7）。
    ///
    /// `ignore_patterns` 追加在默认忽略列表与项目 ignore 文件之后，
    /// `binary_detection` 决定是否做 magic/null-byte 探测，
    /// `circuit_breaker_writes_per_second` 是单文件的每秒写入上限。
    pub fn with_config(
        worktree_root: impl Into<PathBuf>,
        config: &SnapshotConfig,
        on_event: impl Fn(FileEvent) -> Result<SnapshotTrigger> + Send + Sync + 'static,
    ) -> Self {
        let ignore_filter = IgnoreFilter::new(worktree_root);
        for pattern in &config.ignore_patterns {
            ignore_filter.add_pattern(pattern);
        }
        Self {
            ignore_filter,
            circuit_breaker: Mutex::new(CircuitBreaker::new(
                config.circuit_breaker_writes_per_second,
            )),
            binary_detection: config.binary_detection,
            on_event: Box::new(on_event),
        }
    }

    /// 处理文件变更事件
    ///
    /// 1. 检查忽略规则
    /// 2. 检查二进制文件
    /// 3. 检查频率熔断
    /// 4. 触发快照
    ///
    /// `Ok(None)` 表示事件被过滤掉；`Err` 表示回调本身失败（例如 recorder
    /// 通道已经关闭），调用方必须让它可见而不是丢弃。
    pub fn handle_event(&self, event: FileEvent) -> Result<Option<SnapshotTrigger>> {
        // 1. 忽略规则检查
        if self.ignore_filter.should_ignore(&event.path) {
            return Ok(None);
        }

        // 2. 二进制文件检测
        if self.binary_detection && Self::is_binary_file(&event.path) {
            return Ok(None);
        }

        // 3. 频率熔断检查
        let mut circuit_breaker = self.circuit_breaker.lock();
        if circuit_breaker.check(&event.path) {
            return Ok(None);
        }
        drop(circuit_breaker);

        // 4. 触发快照
        (self.on_event)(event).map(Some)
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
            match res {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        // 处理线程已经退出（watcher 正在关停）。事件无处可去，
                        // 但静默丢弃会掩盖"处理线程 panic 了"这种情况。
                        tracing::debug!("fs watcher: event dropped, processing thread gone");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "fs watcher error");
                }
            }
        })
        .map_err(std::io::Error::other)?;

        watcher
            .watch(&root, notify::RecursiveMode::Recursive)
            .map_err(std::io::Error::other)?;

        // Clone Arc<Self> into the background thread. The thread processes
        // events through this monitor's ignore-filter + circuit-breaker pipeline.
        let monitor = self.clone();
        std::thread::Builder::new()
            .name("fs-watcher".into())
            .spawn(move || {
                // 回调失败通常是 recorder 通道断了，之后每个事件都会重复失败。
                // 第一次用 warn 让它可见，之后降级到 debug，避免刷爆日志。
                let mut reported_failure = false;
                for event in rx.iter() {
                    let Some(kind) = map_event_kind(&event.kind) else {
                        continue;
                    };
                    for path in event.paths {
                        let file_event = FileEvent {
                            path,
                            kind,
                            timestamp: Instant::now(),
                        };
                        let path_for_log = file_event.path.clone();
                        if let Err(error) = monitor.handle_event(file_event) {
                            if reported_failure {
                                tracing::debug!(
                                    path = %path_for_log.display(),
                                    error = %error,
                                    "fs watcher: event handling failed"
                                );
                            } else {
                                reported_failure = true;
                                tracing::warn!(
                                    path = %path_for_log.display(),
                                    error = %error,
                                    "fs watcher: event handling failed"
                                );
                            }
                        }
                    }
                }
            })
            .map_err(std::io::Error::other)?;

        Ok(WatchHandle { watcher: Some(watcher) })
    }
}

/// 把 notify 的事件类型映射到本 crate 的事件类型。
///
/// - `Access(Close(Write | Any))`：inotify 的 `IN_CLOSE_WRITE`，§4.7 的
///   "file close → force flush version"。其余 Access（Open/Read）不是变更。
/// - `Other`：部分后端（PollWatcher、某些 FSEvents 标志组合）用它承载真实
///   变更，丢掉会漏掉版本，因此按 Modified 处理——多记一次版本远好过漏记。
/// - `Any`：后端无法细分时的兜底，同样按 Modified 处理。
fn map_event_kind(kind: &notify::EventKind) -> Option<EventKind> {
    use notify::event::{AccessKind, AccessMode};
    match kind {
        notify::EventKind::Create(_) => Some(EventKind::Created),
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => Some(EventKind::Renamed),
        notify::EventKind::Modify(_) => Some(EventKind::Modified),
        notify::EventKind::Remove(_) => Some(EventKind::Deleted),
        notify::EventKind::Access(AccessKind::Close(AccessMode::Write | AccessMode::Any)) => {
            Some(EventKind::Closed)
        }
        notify::EventKind::Access(_) => None,
        notify::EventKind::Any | notify::EventKind::Other => Some(EventKind::Modified),
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

/// §4.7 每路径 debounce 队列。
///
/// 窗口来自用户设置（`shadow_snapshot.debounce_ms`），所以它是实例字段而不是
/// 常量。同一路径在窗口内的连续写入合并成一个版本；`SnapshotTrigger::Close`
/// 立即到期，对应 spec 的 "file close → force flush version"。
///
/// 队列本身不加锁：它只在单写 recorder 线程上使用（§4.3）。
pub struct DebounceQueue {
    window: Duration,
    /// path → (trigger, 到期时间)
    pending: HashMap<PathBuf, (SnapshotTrigger, Instant)>,
}

impl DebounceQueue {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// 记录一次变更。同路径重复记录会刷新 trigger 与到期时间，
    /// 于是持续写入的文件一直不会 flush，直到安静下来。
    pub fn note(&mut self, path: PathBuf, trigger: SnapshotTrigger, now: Instant) {
        let due_at = if trigger == SnapshotTrigger::Close {
            now
        } else {
            now + self.window
        };
        self.pending.insert(path, (trigger, due_at));
    }

    /// 取出所有已到期的路径。
    pub fn flush_due(&mut self, now: Instant) -> Vec<(PathBuf, SnapshotTrigger)> {
        let mut released = Vec::new();
        self.pending.retain(|path, (trigger, due_at)| {
            if *due_at <= now {
                released.push((path.clone(), *trigger));
                false
            } else {
                true
            }
        });
        released
    }

    /// 当前挂起的路径数。
    pub fn pending_count(&self) -> usize {
        self.pending.len()
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

/// 单个文件的写入频率状态
struct FileWrites {
    /// 当前计数窗口的起点
    window_start: Instant,
    /// 窗口内的事件数
    count: u32,
    /// 上一次事件时间，用于判断"安静 2 秒"
    last_event: Instant,
    /// 是否处于熔断状态
    suspended: bool,
}

/// 频率熔断器
///
/// §4.7: 同一文件在 `CIRCUIT_WINDOW`（1 秒）内写入超过 K 次 → 暂停该文件的
/// 快照，直到它安静 `CIRCUIT_SUSPEND`（2 秒）。
///
/// 计数用固定的 1 秒窗口，而不是 `count / elapsed` 的瞬时速率：两个事件相隔
/// 几微秒时瞬时速率是几十万次/秒，任何 K 都会被击穿，K 也就失去了意义。
struct CircuitBreaker {
    /// 每个文件的写入频率状态
    files: HashMap<PathBuf, FileWrites>,
    /// 每秒最大写入次数（`shadow_snapshot.frequency_circuit_breaker_k`）
    writes_per_second: f64,
}

impl CircuitBreaker {
    fn new(writes_per_second: f64) -> Self {
        Self {
            files: HashMap::new(),
            writes_per_second,
        }
    }

    /// 检查是否应该熔断
    ///
    /// 返回 true 表示已熔断（跳过快照）
    fn check(&mut self, path: &Path) -> bool {
        let now = Instant::now();
        let entry = self.files.entry(path.to_path_buf()).or_insert(FileWrites {
            window_start: now,
            count: 0,
            last_event: now,
            suspended: false,
        });

        let idle = now.duration_since(entry.last_event) >= CIRCUIT_SUSPEND;
        entry.last_event = now;

        if entry.suspended {
            if !idle {
                return true;
            }
            // 安静够久 → 解除熔断并开一个新窗口。
            entry.suspended = false;
            entry.window_start = now;
            entry.count = 0;
        }

        if now.duration_since(entry.window_start) >= CIRCUIT_WINDOW {
            entry.window_start = now;
            entry.count = 0;
        }
        entry.count += 1;

        if f64::from(entry.count) > self.writes_per_second {
            entry.suspended = true;
            tracing::debug!(
                path = %path.display(),
                writes = entry.count,
                "snapshot suspended: write frequency exceeded threshold"
            );
            return true;
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
        let mut circuit_breaker = CircuitBreaker::new(crate::config::DEFAULT_CIRCUIT_BREAKER_K);
        let path = PathBuf::from("/tmp/test.txt");

        // 第一次调用应通过（elapsed=0，不检查阈值）
        assert!(!circuit_breaker.check(&path));

        // 等待 2 秒重置窗口
        std::thread::sleep(Duration::from_millis(2001));

        // 窗口重置后应通过
        assert!(!circuit_breaker.check(&path));
    }

    /// `frequency_circuit_breaker_k` 必须真的改变熔断点：K=1 的监控器在同一
    /// 秒内的连续写入会被熔断，K 很大的监控器不会。
    #[test]
    fn configured_circuit_breaker_threshold_changes_suspension() {
        fn trips(writes_per_second: f64) -> bool {
            let mut circuit_breaker = CircuitBreaker::new(writes_per_second);
            let path = PathBuf::from("/tmp/hot.txt");
            (0..64).any(|_| circuit_breaker.check(&path))
        }

        assert!(trips(1.0), "K=1 must suspend a 64-write burst");
        assert!(!trips(100_000.0), "a huge K must never suspend");
    }

    /// `binary_detection = false` 时二进制文件也要产生事件；开启时被过滤。
    #[test]
    fn binary_detection_setting_controls_filtering() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("program");
        std::fs::write(&path, ELF_MAGIC).unwrap();

        let event = || FileEvent {
            path: path.clone(),
            kind: EventKind::Modified,
            timestamp: Instant::now(),
        };

        let detecting = Monitor::with_config(
            directory.path(),
            &SnapshotConfig::default(),
            |_event| Ok(SnapshotTrigger::Write),
        );
        assert_eq!(detecting.handle_event(event()).unwrap(), None);

        let permissive = Monitor::with_config(
            directory.path(),
            &SnapshotConfig {
                binary_detection: false,
                ..SnapshotConfig::default()
            },
            |_event| Ok(SnapshotTrigger::Write),
        );
        assert_eq!(
            permissive.handle_event(event()).unwrap(),
            Some(SnapshotTrigger::Write)
        );
    }

    /// 用户设置里的 `ignore_patterns` 必须真的参与过滤。
    #[test]
    fn configured_ignore_patterns_are_applied() {
        let directory = tempfile::tempdir().unwrap();
        let monitor = Monitor::with_config(
            directory.path(),
            &SnapshotConfig {
                ignore_patterns: vec!["*.generated.rs".to_string()],
                ..SnapshotConfig::default()
            },
            |_event| Ok(SnapshotTrigger::Write),
        );

        let ignored = FileEvent {
            path: directory.path().join("schema.generated.rs"),
            kind: EventKind::Modified,
            timestamp: Instant::now(),
        };
        let kept = FileEvent {
            path: directory.path().join("schema.rs"),
            kind: EventKind::Modified,
            timestamp: Instant::now(),
        };

        assert_eq!(monitor.handle_event(ignored).unwrap(), None);
        assert_eq!(
            monitor.handle_event(kept).unwrap(),
            Some(SnapshotTrigger::Write)
        );
    }

    /// 回调失败必须传播出来，而不是被 `handle_event` 吞掉。
    #[test]
    fn callback_failure_propagates_to_caller() {
        let directory = tempfile::tempdir().unwrap();
        let monitor = Monitor::new(directory.path(), |_event| {
            Err(anyhow::anyhow!("recorder channel closed"))
        });

        let error = monitor
            .handle_event(FileEvent {
                path: directory.path().join("a.txt"),
                kind: EventKind::Modified,
                timestamp: Instant::now(),
            })
            .expect_err("callback failure must surface");

        assert!(error.to_string().contains("recorder channel closed"));
    }

    /// §4.7 `Other` 在部分后端承载真实变更，不能丢；`Access(Close(Write))`
    /// 是 close-after-write，必须映射成独立的 Closed 事件；纯读 Access 丢弃。
    #[test]
    fn event_kind_mapping_keeps_meaningful_kinds() {
        use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};

        assert_eq!(
            map_event_kind(&notify::EventKind::Other),
            Some(EventKind::Modified)
        );
        assert_eq!(
            map_event_kind(&notify::EventKind::Any),
            Some(EventKind::Modified)
        );
        assert_eq!(
            map_event_kind(&notify::EventKind::Access(AccessKind::Close(
                AccessMode::Write
            ))),
            Some(EventKind::Closed)
        );
        assert_eq!(
            map_event_kind(&notify::EventKind::Access(AccessKind::Open(
                AccessMode::Read
            ))),
            None
        );
        assert_eq!(
            map_event_kind(&notify::EventKind::Modify(ModifyKind::Name(
                RenameMode::Both
            ))),
            Some(EventKind::Renamed)
        );
    }

    /// debounce 窗口来自设置：短窗口的队列到期更早，长窗口的仍在挂起。
    #[test]
    fn debounce_window_comes_from_configuration() {
        let start = Instant::now();
        let path = PathBuf::from("/tmp/debounced.txt");

        let mut fast = DebounceQueue::new(Duration::from_millis(50));
        let mut slow = DebounceQueue::new(Duration::from_millis(500));
        fast.note(path.clone(), SnapshotTrigger::Write, start);
        slow.note(path.clone(), SnapshotTrigger::Write, start);

        let at_100ms = start + Duration::from_millis(100);
        assert_eq!(fast.flush_due(at_100ms).len(), 1, "50ms window is due");
        assert!(slow.flush_due(at_100ms).is_empty(), "500ms window is not");
        assert_eq!(slow.pending_count(), 1);
        assert_eq!(slow.flush_due(start + Duration::from_millis(501)).len(), 1);
    }

    /// §4.7 file close → force flush：Close 事件不等 debounce 窗口。
    #[test]
    fn close_events_bypass_the_debounce_window() {
        let start = Instant::now();
        let mut queue = DebounceQueue::new(Duration::from_secs(30));
        let path = PathBuf::from("/tmp/saved.txt");

        queue.note(path.clone(), SnapshotTrigger::Write, start);
        assert!(queue.flush_due(start).is_empty());

        queue.note(path.clone(), SnapshotTrigger::Close, start);
        let flushed = queue.flush_due(start);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1, SnapshotTrigger::Close);
    }
}
