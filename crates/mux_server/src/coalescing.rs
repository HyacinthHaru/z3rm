//! §3.3 自适应输出合并 (adaptive coalescing)
//!
//! 根据 PTY 输出吞吐量动态调整批处理窗口:
//! - 高吞吐量: 0ms (即时推送)
//! - 中等吞吐量: 2ms
//! - 空闲: 8-16ms
//!
//! 设计: 每个 pane 维护自己的合并器实例，根据最近输出频率自适应调整延迟。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 自适应合并器: 根据最近输出频率动态调整批处理延迟。
///
/// §3.3: The PTY reader thread dynamically adjusts its batching window
/// based on recent output throughput.
pub struct AdaptiveCoalescer {
    /// 最近一次输出时间
    last_output: Option<Instant>,
    /// 输出计数 (用于计算吞吐量)
    output_count: u32,
    /// 当前批处理延迟
    current_delay: Duration,
    /// 观察窗口大小
    window: Duration,
}

impl AdaptiveCoalescer {
    /// 创建自适应合并器 (§3.3)
    ///
    /// 初始延迟: 16ms (保守默认值，适用于空闲状态)
    pub fn new() -> Self {
        Self {
            last_output: None,
            output_count: 0,
            current_delay: Duration::from_millis(16),
            window: Duration::from_millis(100),
        }
    }

    /// 记录一次输出事件，更新吞吐量估计。
    /// 返回当前的批处理延迟。
    pub fn on_output(&mut self) -> Duration {
        let now = Instant::now();
        self.output_count += 1;

        // 检查是否超出观察窗口
        if let Some(last) = self.last_output {
            if now.duration_since(last) > self.window {
                // 窗口过期，重置统计
                self.output_count = 1;
            }
        }
        self.last_output = Some(now);

        // §3.3 根据吞吐量调整延迟
        self.adjust_delay();
        self.current_delay
    }

    /// 根据最近输出频率调整批处理延迟 (§3.3)
    fn adjust_delay(&mut self) {
        // 简化的吞吐量分类:
        // - 100ms 内 >100 次输出 → 高吞吐量 (0ms)
        // - 100ms 内 10-100 次输出 → 中等 (2ms)
        // - <10 次 → 空闲 (16ms)
        match self.output_count {
            ..=9 => self.current_delay = Duration::from_millis(16),
            ..=99 => self.current_delay = Duration::from_millis(2),
            _ => self.current_delay = Duration::ZERO,
        }
    }

    /// 获取当前批处理延迟
    pub fn delay(&self) -> Duration {
        self.current_delay
    }

    /// 重置统计 (用于 pane 重置)
    pub fn reset(&mut self) {
        self.last_output = None;
        self.output_count = 0;
        self.current_delay = Duration::from_millis(16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_initial_delay_is_16ms() {
        let coalescer = AdaptiveCoalescer::new();
        assert_eq!(coalescer.delay(), Duration::from_millis(16));
    }

    #[test]
    fn test_low_throughput_16ms() {
        let mut coalescer = AdaptiveCoalescer::new();
        for _ in 0..5 {
            coalescer.on_output();
        }
        assert_eq!(coalescer.delay(), Duration::from_millis(16));
    }

    #[test]
    fn test_medium_throughput_2ms() {
        let mut coalescer = AdaptiveCoalescer::new();
        for _ in 0..50 {
            coalescer.on_output();
        }
        assert_eq!(coalescer.delay(), Duration::from_millis(2));
    }

    #[test]
    fn test_high_throughput_0ms() {
        let mut coalescer = AdaptiveCoalescer::new();
        for _ in 0..150 {
            coalescer.on_output();
        }
        assert_eq!(coalescer.delay(), Duration::ZERO);
    }

    #[test]
    fn test_window_reset() {
        let mut coalescer = AdaptiveCoalescer::new();
        // 累积 50 次输出 (中等)
        for _ in 0..50 {
            coalescer.on_output();
        }
        assert_eq!(coalescer.delay(), Duration::from_millis(2));

        // 等待窗口过期
        thread::sleep(coalescer.window + Duration::from_millis(1));

        // 窗口重置后，延迟回到 16ms
        coalescer.on_output();
        assert_eq!(coalescer.delay(), Duration::from_millis(16));
    }
}

/// §4.7 Filesystem path debounce: coalesce repeated writes to the same path
/// within a 500ms window into a single snapshot, so a noisy editor saving a
/// file many times per second does not produce one version per save.
///
/// The recorder calls `note` on every watcher event and `flush_due` on a timer
/// to release paths that have been quiet for `DEBOUNCE_WINDOW`. While a path
/// is being debounced, `note` updates the latest trigger/content hint, so the
/// released event always reflects the most recent fs state.
pub struct PathDebouncer {
    pending: HashMap<std::path::PathBuf, (shadow_snapshot::SnapshotTrigger, Instant)>,
}

/// §4.7 debounce window for filesystem snapshotting.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

impl PathDebouncer {
    pub fn new() -> Self {
        Self { pending: HashMap::new() }
    }

    /// Record that `path` changed with `trigger` at `now`. If the path is
    /// already pending, the trigger is refreshed to the latest event so the
    /// eventual flush reflects the newest observable state.
    pub fn note(
        &mut self,
        path: std::path::PathBuf,
        trigger: shadow_snapshot::SnapshotTrigger,
        now: Instant,
    ) {
        self.pending.insert(path, (trigger, now));
    }

    /// Return and remove paths that have been idle for ≥ `DEBOUNCE_WINDOW`.
    /// A path still receiving events stays pending until it quiets down.
    pub fn flush_due(&mut self, now: Instant) -> Vec<(std::path::PathBuf, shadow_snapshot::SnapshotTrigger)> {
        let mut released = Vec::new();
        self.pending.retain(|path, (trigger, last)| {
            if now.duration_since(*last) >= DEBOUNCE_WINDOW {
                released.push((path.clone(), *trigger));
                false
            } else {
                true
            }
        });
        released
    }

    /// Number of paths currently being debounced.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod path_debouncer_tests {
    use super::*;

    #[test]
    fn coalesces_bursts_within_window() {
        use shadow_snapshot::SnapshotTrigger;
        use std::path::PathBuf;

        let mut debouncer = PathDebouncer::new();
        let path = PathBuf::from("/tmp/burst.txt");
        let t0 = Instant::now();

        // §4.7 a burst of 5 writes within the window collapses to one pending entry.
        for i in 0..5 {
            debouncer.note(path.clone(), SnapshotTrigger::Write, t0 + Duration::from_millis(i));
        }
        assert_eq!(debouncer.pending_count(), 1, "burst must coalesce");

        // Nothing flushes before the 500ms window elapses.
        assert!(debouncer.flush_due(t0 + Duration::from_millis(400)).is_empty());

        // After 500ms quiet, exactly one flush occurs carrying the latest trigger.
        let flushed = debouncer.flush_due(t0 + Duration::from_millis(550));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].0, path);
        assert_eq!(flushed[0].1, SnapshotTrigger::Write);
        assert_eq!(debouncer.pending_count(), 0);
    }

    #[test]
    fn keeps_path_pending_until_quiet() {
        use shadow_snapshot::SnapshotTrigger;
        use std::path::PathBuf;

        let mut debouncer = PathDebouncer::new();
        let path = PathBuf::from("/tmp/chatty.txt");
        let t0 = Instant::now();

        // Writes at 0, 200, 400ms — the path never goes 500ms quiet.
        debouncer.note(path.clone(), SnapshotTrigger::Write, t0);
        assert!(debouncer.flush_due(t0 + Duration::from_millis(450)).is_empty());
        debouncer.note(path.clone(), SnapshotTrigger::Write, t0 + Duration::from_millis(200));
        debouncer.note(path.clone(), SnapshotTrigger::Write, t0 + Duration::from_millis(400));
        assert!(debouncer.flush_due(t0 + Duration::from_millis(600)).is_empty(),
            "still within window of last note at 400ms (200ms < 500ms)");

        // Quiet for 500ms after the last note → flush.
        let flushed = debouncer.flush_due(t0 + Duration::from_millis(901));
        assert_eq!(flushed.len(), 1);
    }
}
