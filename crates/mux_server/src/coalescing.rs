//! §3.3 / §16.3 自适应输出合并 (adaptive coalescing)
//!
//! §16.3 定义三档, 判据是 PTY 输出的**字节速率**与键盘活跃与否, 不是输出事件次数:
//! - Interactive: 键盘活跃 且 输出 < 4KB/s → 0ms (击键回显不额外排队)
//! - Normal: → 2ms
//! - High-throughput: > 100KB/s 且持续 > 500ms → 8-16ms, 并丢弃中间帧通知
//!
//! 设计: 每个 pane 的 PTY read 线程独占一个合并器实例。键盘活跃信号来自
//! 另一个线程 (连接任务里的 `Pane::write_input`), 因此用 `KeyboardActivity`
//! 共享句柄传递, 而不是记在合并器内部。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// §16.3 合并档位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoalescingTier {
    /// 键盘活跃且输出低于 `INTERACTIVE_BYTE_RATE_CEILING`: 零附加延迟。
    Interactive,
    /// 默认档。
    Normal,
    /// 持续高吞吐: 拉长窗口并丢弃中间帧, 保护通知通道。
    HighThroughput,
}

/// §16.3 Interactive 档上限: PTY 输出速率必须低于 4KB/s。
const INTERACTIVE_BYTE_RATE_CEILING: f64 = 4.0 * 1024.0;

/// §16.3 High-throughput 档下限: PTY 输出速率达到 100KB/s。
const HIGH_THROUGHPUT_BYTE_RATE_FLOOR: f64 = 100.0 * 1024.0;

/// §16.3 高吞吐必须**连续**超过这个时长才切档。短促的一次 `ls` 输出不该把
/// 后续击键推进 8ms 窗口, 所以瞬时尖峰不算数。
const HIGH_THROUGHPUT_SUSTAIN: Duration = Duration::from_millis(500);

/// 字节速率的滑动测量窗口。速率 = 窗口内字节数 / 窗口时长 (固定分母), 这样
/// 单个样本也不会出现除零, 且速率对字节数单调。200ms 足够短, 使得一次 `cat`
/// 结束后大约两百毫秒内就能退回 Normal 档。
const RATE_WINDOW: Duration = Duration::from_millis(200);

/// 一次击键之后 pane 被视为 "keyboard active" 的时长。正常打字节奏 (含 shell
/// 回显往返) 的击键间隔远小于 1s, 所以 1s 能覆盖一整段连续输入, 而用户停手后
/// 也能迅速退回 Normal 档。
const KEYBOARD_ACTIVE_WINDOW: Duration = Duration::from_millis(1_000);

/// §16.3 Interactive: 不合并。§15.5 要求本地击键→上屏 p95 < 16ms, 回显路径
/// 上任何强制排队都是纯损耗。
const INTERACTIVE_DELAY: Duration = Duration::ZERO;

/// §16.3 Normal 档窗口。
const NORMAL_DELAY: Duration = Duration::from_millis(2);

/// §16.3 允许 8-16ms。取下限 8ms: 每 pane 的 PaneDirty 上限仍被压到 ~125/s
/// (足以保护通知通道), 但万一用户在刷屏的 pane 里打字, 附加延迟只吃掉半个
/// 60fps 帧预算, 给 §15.5 的 16ms p95 留出余量。
const HIGH_THROUGHPUT_DELAY: Duration = Duration::from_millis(8);

/// §16.3 跨线程的 "keyboard active" 信号。
///
/// 击键路径 (`Pane::write_input`) 跑在连接任务上, 合并器跑在 pane 的 PTY read
/// 线程上, 因此最近一次用户输入的时间戳必须显式共享。
#[derive(Clone, Default)]
pub struct KeyboardActivity {
    last_input: Arc<parking_lot::Mutex<Option<Instant>>>,
}

impl KeyboardActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次用户输入 (send_input / paste 都算)。
    pub fn note_input(&self) {
        self.note_input_at(Instant::now());
    }

    pub fn note_input_at(&self, at: Instant) {
        *self.last_input.lock() = Some(at);
    }

    /// `now` 时刻 pane 是否仍处于键盘活跃状态。
    pub fn is_active_at(&self, now: Instant) -> bool {
        self.last_input
            .lock()
            .is_some_and(|at| now.saturating_duration_since(at) < KEYBOARD_ACTIVE_WINDOW)
    }
}

/// §16.3 自适应合并器: 按字节速率 + 键盘活跃度选择档位, 并按档位窗口给
/// PaneDirty 通知定速。
pub struct AdaptiveCoalescer {
    /// `RATE_WINDOW` 内的 (时刻, 字节数) 样本, 最旧的在前。
    samples: std::collections::VecDeque<(Instant, u64)>,
    /// `samples` 内字节数之和。
    windowed_bytes: u64,
    /// 速率首次达到高吞吐下限且此后没有跌回的时刻; 用于 §16.3 的 500ms 持续判定。
    high_rate_since: Option<Instant>,
    tier: CoalescingTier,
    current_delay: Duration,
    keyboard: KeyboardActivity,
    /// 上一条被放行的 PaneDirty 时刻。
    last_emit: Option<Instant>,
    /// §16.3 被丢弃的中间帧通知数 (仅用于观测)。
    dropped_frames: u64,
}

impl AdaptiveCoalescer {
    /// 创建合并器。初始档位是 Normal: 新 pane 既没观测到高吞吐, 也没有击键。
    pub fn new() -> Self {
        Self::with_keyboard_activity(KeyboardActivity::new())
    }

    /// 用 pane 共享的键盘活跃句柄创建合并器。
    pub fn with_keyboard_activity(keyboard: KeyboardActivity) -> Self {
        Self {
            samples: std::collections::VecDeque::new(),
            windowed_bytes: 0,
            high_rate_since: None,
            tier: CoalescingTier::Normal,
            current_delay: NORMAL_DELAY,
            keyboard,
            last_emit: None,
            dropped_frames: 0,
        }
    }

    /// 记录一批 PTY 输出字节, 重新分档, 返回当前批处理延迟。
    pub fn on_output(&mut self, byte_count: usize) -> Duration {
        self.on_output_at(Instant::now(), byte_count)
    }

    /// `on_output` 的可注入时间版本, 让档位判定可以被确定性地测试。
    pub fn on_output_at(&mut self, now: Instant, byte_count: usize) -> Duration {
        self.samples.push_back((now, byte_count as u64));
        self.windowed_bytes = self.windowed_bytes.saturating_add(byte_count as u64);
        self.evict_expired(now);
        self.classify(now);
        self.current_delay
    }

    /// 当前批处理延迟。
    pub fn delay(&self) -> Duration {
        self.current_delay
    }

    /// 当前档位。
    pub fn tier(&self) -> CoalescingTier {
        self.tier
    }

    /// 最近 `RATE_WINDOW` 内观测到的字节数换算的每秒速率。
    pub fn byte_rate(&self) -> f64 {
        self.windowed_bytes as f64 / RATE_WINDOW.as_secs_f64()
    }

    /// §16.3 被丢弃的中间帧通知累计数。
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// 判定一次新发布的 generation 能否立即广播 PaneDirty。
    ///
    /// 返回 `false` 表示这一帧的通知被丢弃 —— §15.4 明确 PaneDirty 是
    /// at-most-once, 客户端下次重绘会用 `fetch_grid_update` 直接拉到最新
    /// generation, 所以丢中间帧不丢状态。调用方应把它记成待补发, 由
    /// `admit_deferred_frame` 在窗口到期后补一条, 保证一段输出的**最后**一帧
    /// 总能送达。
    pub fn admit_frame(&mut self, now: Instant, force: bool) -> bool {
        if force || self.window_elapsed(now) {
            self.last_emit = Some(now);
            true
        } else {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
            false
        }
    }

    /// 补发一条之前被推迟的 PaneDirty。与 `admit_frame` 不同, 这里的失败只是
    /// "窗口还没到", 不是新的丢帧, 因此不计入 `dropped_frames`。
    pub fn admit_deferred_frame(&mut self, now: Instant) -> bool {
        if self.window_elapsed(now) {
            self.last_emit = Some(now);
            true
        } else {
            false
        }
    }

    /// 重置统计 (用于 pane 重置)。
    pub fn reset(&mut self) {
        self.samples.clear();
        self.windowed_bytes = 0;
        self.high_rate_since = None;
        self.tier = CoalescingTier::Normal;
        self.current_delay = NORMAL_DELAY;
        self.last_emit = None;
        self.dropped_frames = 0;
    }

    fn window_elapsed(&self, now: Instant) -> bool {
        self.current_delay.is_zero()
            || self
                .last_emit
                .is_none_or(|at| now.saturating_duration_since(at) >= self.current_delay)
    }

    fn evict_expired(&mut self, now: Instant) {
        while let Some(&(at, bytes)) = self.samples.front() {
            if now.saturating_duration_since(at) >= RATE_WINDOW {
                self.windowed_bytes = self.windowed_bytes.saturating_sub(bytes);
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// §16.3 分档。高吞吐优先于 Interactive: 速率判据本身互斥, 但先判高吞吐
    /// 才能保证刷屏期间的击键不会把窗口拉回 0ms。
    fn classify(&mut self, now: Instant) {
        let rate = self.byte_rate();
        if rate >= HIGH_THROUGHPUT_BYTE_RATE_FLOOR {
            self.high_rate_since.get_or_insert(now);
        } else {
            self.high_rate_since = None;
        }
        let sustained = self
            .high_rate_since
            .is_some_and(|since| now.saturating_duration_since(since) > HIGH_THROUGHPUT_SUSTAIN);

        self.tier = if sustained {
            CoalescingTier::HighThroughput
        } else if rate < INTERACTIVE_BYTE_RATE_CEILING && self.keyboard.is_active_at(now) {
            CoalescingTier::Interactive
        } else {
            CoalescingTier::Normal
        };
        self.current_delay = match self.tier {
            CoalescingTier::Interactive => INTERACTIVE_DELAY,
            CoalescingTier::Normal => NORMAL_DELAY,
            CoalescingTier::HighThroughput => HIGH_THROUGHPUT_DELAY,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 使窗口速率**恰好达到** `bytes_per_second` 的最小字节数。返回值减一
    /// 必然严格低于目标速率, 所以档位边界可以被精确测到, 不受浮点舍入干扰。
    fn bytes_for_rate(bytes_per_second: f64) -> usize {
        let window_seconds = RATE_WINDOW.as_secs_f64();
        let mut bytes = (bytes_per_second * window_seconds).ceil() as usize;
        while (bytes as f64) / window_seconds < bytes_per_second {
            bytes += 1;
        }
        bytes
    }

    #[test]
    fn new_coalescer_starts_in_normal_tier() {
        let coalescer = AdaptiveCoalescer::new();
        assert_eq!(coalescer.tier(), CoalescingTier::Normal);
        assert_eq!(coalescer.delay(), NORMAL_DELAY);
    }

    /// §16.3 键盘活跃 + 低速率 → Interactive/0ms。§15.5 的击键回显预算依赖它。
    #[test]
    fn keyboard_active_and_low_rate_is_interactive_zero_delay() {
        let keyboard = KeyboardActivity::new();
        let mut coalescer = AdaptiveCoalescer::with_keyboard_activity(keyboard.clone());
        let start = Instant::now();

        keyboard.note_input_at(start);
        let delay = coalescer.on_output_at(start + Duration::from_millis(1), 8);

        assert_eq!(coalescer.tier(), CoalescingTier::Interactive);
        assert_eq!(delay, Duration::ZERO);
    }

    /// §16.3 Interactive 的 4KB/s 边界。
    #[test]
    fn interactive_tier_boundary_is_four_kilobytes_per_second() {
        let keyboard = KeyboardActivity::new();
        let start = Instant::now();

        let mut below = AdaptiveCoalescer::with_keyboard_activity(keyboard.clone());
        keyboard.note_input_at(start);
        below.on_output_at(start, bytes_for_rate(INTERACTIVE_BYTE_RATE_CEILING) - 1);
        assert!(below.byte_rate() < INTERACTIVE_BYTE_RATE_CEILING);
        assert_eq!(below.tier(), CoalescingTier::Interactive);
        assert_eq!(below.delay(), Duration::ZERO);

        let mut at_ceiling = AdaptiveCoalescer::with_keyboard_activity(keyboard.clone());
        at_ceiling.on_output_at(start, bytes_for_rate(INTERACTIVE_BYTE_RATE_CEILING));
        assert!(at_ceiling.byte_rate() >= INTERACTIVE_BYTE_RATE_CEILING);
        assert_eq!(
            at_ceiling.tier(),
            CoalescingTier::Normal,
            "at 4KB/s the pane is no longer interactive even with the keyboard active"
        );
        assert_eq!(at_ceiling.delay(), NORMAL_DELAY);
    }

    /// 没有击键时, 再低的速率也只是 Normal — Interactive 是 AND 条件。
    #[test]
    fn low_rate_without_keyboard_is_normal_two_milliseconds() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();

        let delay = coalescer.on_output_at(start, 16);

        assert_eq!(coalescer.tier(), CoalescingTier::Normal);
        assert_eq!(delay, Duration::from_millis(2));
    }

    #[test]
    fn keyboard_activity_expires_and_drops_back_to_normal() {
        let keyboard = KeyboardActivity::new();
        let mut coalescer = AdaptiveCoalescer::with_keyboard_activity(keyboard.clone());
        let start = Instant::now();
        keyboard.note_input_at(start);

        coalescer.on_output_at(start + KEYBOARD_ACTIVE_WINDOW - Duration::from_millis(1), 8);
        assert_eq!(coalescer.tier(), CoalescingTier::Interactive);

        coalescer.on_output_at(start + KEYBOARD_ACTIVE_WINDOW, 8);
        assert_eq!(coalescer.tier(), CoalescingTier::Normal);
    }

    /// §16.3 高吞吐必须**持续** 500ms 以上才切档。
    #[test]
    fn high_throughput_requires_five_hundred_milliseconds_sustained() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();
        let chunk = bytes_for_rate(HIGH_THROUGHPUT_BYTE_RATE_FLOOR * 1.5);

        // 每 50ms 一批, 从第一批起速率就在下限之上。
        for step in 0..=8u32 {
            let now = start + Duration::from_millis(u64::from(step) * 50);
            coalescer.on_output_at(now, chunk);
            assert!(coalescer.byte_rate() >= HIGH_THROUGHPUT_BYTE_RATE_FLOOR);
            assert_eq!(
                coalescer.tier(),
                CoalescingTier::Normal,
                "still Normal at {}ms of sustained high rate",
                step * 50
            );
        }

        // 恰好 500ms 还不够 (§16.3 是严格 "> 500ms")。
        coalescer.on_output_at(start + HIGH_THROUGHPUT_SUSTAIN, chunk);
        assert_eq!(coalescer.tier(), CoalescingTier::Normal);

        coalescer.on_output_at(
            start + HIGH_THROUGHPUT_SUSTAIN + Duration::from_millis(1),
            chunk,
        );
        assert_eq!(coalescer.tier(), CoalescingTier::HighThroughput);
        assert_eq!(coalescer.delay(), HIGH_THROUGHPUT_DELAY);
        assert!(coalescer.delay() >= Duration::from_millis(8));
        assert!(coalescer.delay() <= Duration::from_millis(16));
    }

    /// 100KB/s 下限边界: 差一个字节就不开始计持续时间。每批间隔一整个速率窗口,
    /// 这样窗口里始终只有一个样本, 速率就等于该批的换算值。
    #[test]
    fn high_throughput_floor_is_one_hundred_kilobytes_per_second() {
        let start = Instant::now();
        let at_floor_chunk = bytes_for_rate(HIGH_THROUGHPUT_BYTE_RATE_FLOOR);

        let mut below = AdaptiveCoalescer::new();
        for step in 0..=12u32 {
            below.on_output_at(start + RATE_WINDOW * step, at_floor_chunk - 1);
        }
        assert!(below.byte_rate() < HIGH_THROUGHPUT_BYTE_RATE_FLOOR);
        assert_eq!(
            below.tier(),
            CoalescingTier::Normal,
            "just under 100KB/s never engages the high-throughput tier"
        );

        let mut at_floor = AdaptiveCoalescer::new();
        for step in 0..=12u32 {
            at_floor.on_output_at(start + RATE_WINDOW * step, at_floor_chunk);
        }
        assert!(at_floor.byte_rate() >= HIGH_THROUGHPUT_BYTE_RATE_FLOOR);
        assert_eq!(at_floor.tier(), CoalescingTier::HighThroughput);
    }

    /// 尖峰后速率跌回, 持续计时必须清零, 否则下一次尖峰会被误判为已持续。
    #[test]
    fn sustained_timer_resets_when_rate_falls_back() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();
        let chunk = bytes_for_rate(HIGH_THROUGHPUT_BYTE_RATE_FLOOR * 2.0);

        coalescer.on_output_at(start, chunk);
        coalescer.on_output_at(start + Duration::from_millis(100), chunk);

        // 安静一整个速率窗口, 速率跌回 0。
        coalescer.on_output_at(start + Duration::from_millis(400), 1);
        assert_eq!(coalescer.tier(), CoalescingTier::Normal);

        // 新尖峰的持续时间从这一刻重新起算, 400ms 后仍不到 500ms。
        let resumed = start + Duration::from_millis(600);
        coalescer.on_output_at(resumed, chunk);
        coalescer.on_output_at(resumed + Duration::from_millis(400), chunk);
        assert_eq!(coalescer.tier(), CoalescingTier::Normal);
    }

    /// 刷屏期间的击键不能把窗口拉回 0ms — 否则通知通道会被打爆。
    #[test]
    fn high_throughput_outranks_keyboard_activity() {
        let keyboard = KeyboardActivity::new();
        let mut coalescer = AdaptiveCoalescer::with_keyboard_activity(keyboard.clone());
        let start = Instant::now();
        let chunk = bytes_for_rate(HIGH_THROUGHPUT_BYTE_RATE_FLOOR * 2.0);

        for step in 0..=12u32 {
            let now = start + Duration::from_millis(u64::from(step) * 50);
            keyboard.note_input_at(now);
            coalescer.on_output_at(now, chunk);
        }

        assert_eq!(coalescer.tier(), CoalescingTier::HighThroughput);
        assert_eq!(coalescer.delay(), HIGH_THROUGHPUT_DELAY);
    }

    /// §16.3 高吞吐档丢弃中间帧, 窗口到期后放行一条。
    #[test]
    fn high_throughput_drops_intermediate_frames() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();
        let chunk = bytes_for_rate(HIGH_THROUGHPUT_BYTE_RATE_FLOOR * 2.0);
        for step in 0..=12u32 {
            coalescer.on_output_at(start + Duration::from_millis(u64::from(step) * 50), chunk);
        }
        assert_eq!(coalescer.tier(), CoalescingTier::HighThroughput);

        let burst = start + Duration::from_secs(1);
        assert!(coalescer.admit_frame(burst, false), "first frame passes");
        assert_eq!(coalescer.dropped_frames(), 0);

        for offset in 1..HIGH_THROUGHPUT_DELAY.as_millis() as u64 {
            assert!(
                !coalescer.admit_frame(burst + Duration::from_millis(offset), false),
                "frame at +{offset}ms is inside the coalescing window"
            );
        }
        assert_eq!(
            coalescer.dropped_frames(),
            HIGH_THROUGHPUT_DELAY.as_millis() as u64 - 1
        );

        assert!(coalescer.admit_frame(burst + HIGH_THROUGHPUT_DELAY, false));
    }

    /// Interactive 档不合并也不丢帧。
    #[test]
    fn interactive_tier_never_drops_frames() {
        let keyboard = KeyboardActivity::new();
        let mut coalescer = AdaptiveCoalescer::with_keyboard_activity(keyboard.clone());
        let start = Instant::now();
        keyboard.note_input_at(start);
        coalescer.on_output_at(start, 8);
        assert_eq!(coalescer.tier(), CoalescingTier::Interactive);

        for offset in 0..10u64 {
            assert!(coalescer.admit_frame(start + Duration::from_micros(offset), false));
        }
        assert_eq!(coalescer.dropped_frames(), 0);
    }

    /// 被推迟的帧补发时不重复计入丢帧。
    #[test]
    fn deferred_frame_release_is_not_counted_as_a_drop() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();
        coalescer.on_output_at(start, 16);
        assert_eq!(coalescer.tier(), CoalescingTier::Normal);

        assert!(coalescer.admit_frame(start, false));
        assert!(!coalescer.admit_frame(start + Duration::from_micros(500), false));
        assert_eq!(coalescer.dropped_frames(), 1);

        assert!(!coalescer.admit_deferred_frame(start + Duration::from_micros(600)));
        assert_eq!(coalescer.dropped_frames(), 1, "deferral is not a new drop");

        assert!(coalescer.admit_deferred_frame(start + NORMAL_DELAY));
        assert_eq!(coalescer.dropped_frames(), 1);
    }

    #[test]
    fn reset_returns_to_normal_tier() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();
        let chunk = bytes_for_rate(HIGH_THROUGHPUT_BYTE_RATE_FLOOR * 2.0);
        for step in 0..=12u32 {
            coalescer.on_output_at(start + Duration::from_millis(u64::from(step) * 50), chunk);
        }
        assert_eq!(coalescer.tier(), CoalescingTier::HighThroughput);

        coalescer.reset();

        assert_eq!(coalescer.tier(), CoalescingTier::Normal);
        assert_eq!(coalescer.delay(), NORMAL_DELAY);
        assert_eq!(coalescer.byte_rate(), 0.0);
        assert_eq!(coalescer.dropped_frames(), 0);
    }

    /// 速率窗口外的样本必须被剔除, 否则一次刷屏会永久拉高速率。
    #[test]
    fn rate_window_evicts_stale_samples() {
        let mut coalescer = AdaptiveCoalescer::new();
        let start = Instant::now();

        coalescer.on_output_at(start, 100_000);
        assert!(coalescer.byte_rate() > 0.0);

        coalescer.on_output_at(start + RATE_WINDOW, 0);
        assert_eq!(coalescer.byte_rate(), 0.0);
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
        Self {
            pending: HashMap::new(),
        }
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
    pub fn flush_due(
        &mut self,
        now: Instant,
    ) -> Vec<(std::path::PathBuf, shadow_snapshot::SnapshotTrigger)> {
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

    pub fn drain_all(&mut self) -> Vec<(std::path::PathBuf, shadow_snapshot::SnapshotTrigger)> {
        self.pending
            .drain()
            .map(|(path, (trigger, _last_change))| (path, trigger))
            .collect()
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
            debouncer.note(
                path.clone(),
                SnapshotTrigger::Write,
                t0 + Duration::from_millis(i),
            );
        }
        assert_eq!(debouncer.pending_count(), 1, "burst must coalesce");

        // Nothing flushes before the 500ms window elapses.
        assert!(
            debouncer
                .flush_due(t0 + Duration::from_millis(400))
                .is_empty()
        );

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
        assert!(
            debouncer
                .flush_due(t0 + Duration::from_millis(450))
                .is_empty()
        );
        debouncer.note(
            path.clone(),
            SnapshotTrigger::Write,
            t0 + Duration::from_millis(200),
        );
        debouncer.note(
            path.clone(),
            SnapshotTrigger::Write,
            t0 + Duration::from_millis(400),
        );
        assert!(
            debouncer
                .flush_due(t0 + Duration::from_millis(600))
                .is_empty(),
            "still within window of last note at 400ms (200ms < 500ms)"
        );

        // Quiet for 500ms after the last note → flush.
        let flushed = debouncer.flush_due(t0 + Duration::from_millis(901));
        assert_eq!(flushed.len(), 1);
    }

    #[test]
    fn drain_all_releases_paths_before_quiet_window() {
        let mut debouncer = PathDebouncer::new();
        let path = std::path::PathBuf::from("/tmp/shutdown.txt");
        debouncer.note(
            path.clone(),
            shadow_snapshot::SnapshotTrigger::Write,
            Instant::now(),
        );

        let drained = debouncer.drain_all();

        assert_eq!(
            drained,
            vec![(path, shadow_snapshot::SnapshotTrigger::Write)]
        );
        assert_eq!(debouncer.pending_count(), 0);
    }
}
