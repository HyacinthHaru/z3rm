//! §16.6 RTT 估计 + 帧率控制 + 心跳机制。
//!
//! RTT 估计沿用 TCP 的 SRTT/RTTVAR 算法 (RFC 6298), 采样方式沿用 mosh: 每个数据报都
//! 带一个 16 位毫秒时间戳和一个"回显对端时间戳"字段, 收到回显即得一个 RTT 采样。
//! 这样不需要维护"待确认包"表, 也不需要专门的探测包。

use std::time::{Duration, Instant};

// §16.6 RTO 下限: 50ms (不采用 TCP 的 1s, 适配终端场景)。
pub const RTO_MIN: Duration = Duration::from_millis(50);

// §16.6 RTO 上限: 1000ms。
pub const RTO_MAX: Duration = Duration::from_millis(1000);

// §16.6 心跳间隔: 3000ms (无发送时补一个心跳包)。
pub const ACK_INTERVAL: Duration = Duration::from_millis(3000);

// §16.6 服务器关联超时: 40s (无接收后判定断开)。
pub const SERVER_ASSOCIATION_TIMEOUT: Duration = Duration::from_secs(40);

// §16.6 帧率控制: 最小发送间隔 20ms。
pub const SEND_INTERVAL_MIN: Duration = Duration::from_millis(20);

// §16.6 帧率控制: 最大发送间隔 250ms。
pub const SEND_INTERVAL_MAX: Duration = Duration::from_millis(250);

// §16.6 16 位毫秒时钟的模。
pub const TIMESTAMP_MODULUS: u64 = 1 << 16;

// §16.6 超过该值的 RTT 采样按无效丢弃 (与 mosh 一致)。
//
// 16 位毫秒时钟每 65.5 秒回绕一次, 因此一个"看起来很大"的差值既可能是真的慢, 也可能是
// 时钟回绕后的假象。挂起/恢复或长时间调度延迟都会造成这种采样, 采信它会把 SRTT 污染很久。
pub const MAX_RTT_SAMPLE_MS: u64 = 5_000;

// §16.6 收到对端时间戳后最多持有多久仍值得回显。
//
// 超过这个时间说明本端自己卡住了, 回显出去只会让对端把本端的停顿算进 RTT。
pub const MAX_TIMESTAMP_HOLD: Duration = Duration::from_millis(1000);

/// §16.6 RTT 估计器对外的只读快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RttSnapshot {
    pub srtt: Duration,
    pub rttvar: Duration,
    pub rto: Duration,
    pub send_interval: Duration,
    /// §16.6 已采纳的 RTT 采样数; 为 0 表示 SRTT 仍是初始猜测值。
    pub samples: u64,
}

/// §16.6 TCP 风格 RTT 估计器 (RFC 6298 / mosh)。
///
/// 平滑 RTT (SRTT) 和 RTT 方差 (RTTVAR) 用于计算重传超时 (RTO)。
/// RTO = SRTT + max(4*RTTVAR, G) where G = 50ms。
pub struct RttEstimator {
    /// §16.6 平滑 RTT (初始值: RTO_MIN), 毫秒。
    srtt: f64,
    /// §16.6 RTT 方差估计 (初始值: RTO_MIN / 2), 毫秒。
    rttvar: f64,
    /// §16.6 已采纳的采样数。
    samples: u64,
}

impl RttEstimator {
    /// §16.6 创建新的 RTT 估计器。
    pub fn new() -> Self {
        Self {
            srtt: RTO_MIN.as_millis() as f64,
            rttvar: (RTO_MIN.as_millis() as f64) / 2.0,
            samples: 0,
        }
    }

    /// §16.6 记录一次 RTT 采样 (毫秒)。
    ///
    /// RFC 6298 更新公式:
    /// - 首次采样: SRTT = sample, RTTVAR = sample / 2
    /// - 后续:    RTTVAR = (1 - beta) * RTTVAR + beta * |SRTT - sample|
    ///            SRTT   = (1 - alpha) * SRTT + alpha * sample
    ///            alpha = 1/8, beta = 1/4
    pub fn record_rtt(&mut self, sample_ms: f64) {
        // NaN 会顺着 SRTT 污染此后所有计算, 负值则来自时钟异常; 两者都不是采样。
        if !sample_ms.is_finite() || sample_ms < 0.0 {
            return;
        }
        if self.samples == 0 {
            self.srtt = sample_ms;
            self.rttvar = sample_ms / 2.0;
        } else {
            let deviation = (self.srtt - sample_ms).abs();
            self.rttvar = (3.0 / 4.0) * self.rttvar + (1.0 / 4.0) * deviation;
            self.srtt = (7.0 / 8.0) * self.srtt + (1.0 / 8.0) * sample_ms;
        }
        self.samples = self.samples.saturating_add(1);
    }

    /// §16.6 获取当前 RTO (重传超时)。
    /// RTO = SRTT + max(4 * RTTVAR, G), 限制在 [RTO_MIN, RTO_MAX] 内。
    pub fn rto(&self) -> Duration {
        let rto_ms = self.srtt + (4.0 * self.rttvar).max(RTO_MIN.as_millis() as f64);
        let rto_ms = rto_ms
            .max(RTO_MIN.as_millis() as f64)
            .min(RTO_MAX.as_millis() as f64);
        Duration::from_millis(rto_ms as u64)
    }

    /// §16.6 获取当前 SRTT。
    pub fn srtt(&self) -> Duration {
        Duration::from_millis(self.srtt as u64)
    }

    /// §16.6 获取当前 RTTVAR。
    pub fn rttvar(&self) -> Duration {
        Duration::from_millis(self.rttvar as u64)
    }

    /// §16.6 已采纳的 RTT 采样数。
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// §16.6 计算帧率控制发送间隔。
    ///
    /// interval = clamp(SRTT / 2, 20ms, 250ms)
    /// 控制服务器向客户端推送网格更新的频率。
    pub fn send_interval(&self) -> Duration {
        let interval_ms = (self.srtt / 2.0)
            .max(SEND_INTERVAL_MIN.as_millis() as f64)
            .min(SEND_INTERVAL_MAX.as_millis() as f64);
        Duration::from_millis(interval_ms as u64)
    }

    /// §16.6 只读快照。
    pub fn snapshot(&self) -> RttSnapshot {
        RttSnapshot {
            srtt: self.srtt(),
            rttvar: self.rttvar(),
            rto: self.rto(),
            send_interval: self.send_interval(),
            samples: self.samples,
        }
    }
}

/// §16.6 mosh 风格的时间戳交换状态。
///
/// 每个发出的数据报都带本端的 16 位毫秒时钟, 并在可能时回显最近一次收到的对端时钟。
/// 对端收到回显后用 `now16 - reply` 就得到一次 RTT 采样, 无需任何待确认包表。
///
/// 回显值会加上本端的持有时长, 从而把本端的处理延迟从对端测得的 RTT 里扣掉。
pub struct TimestampTracker {
    epoch: Instant,
    peer_timestamp: Option<(u16, Instant)>,
}

impl TimestampTracker {
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub fn new_at(epoch: Instant) -> Self {
        Self {
            epoch,
            peer_timestamp: None,
        }
    }

    /// §16.6 本端 16 位毫秒时钟的当前值。
    pub fn now16_at(&self, now: Instant) -> u16 {
        (now.saturating_duration_since(self.epoch).as_millis() as u64 % TIMESTAMP_MODULUS) as u16
    }

    /// §16.6 记下刚收到的对端时间戳。
    pub fn record_peer_timestamp_at(&mut self, timestamp: u16, now: Instant) {
        self.peer_timestamp = Some((timestamp, now));
    }

    /// §16.6 取出待回显的对端时间戳 (每个时间戳只回显一次)。
    pub fn take_reply_at(&mut self, now: Instant) -> Option<u16> {
        let (timestamp, received_at) = self.peer_timestamp?;
        let held = now.saturating_duration_since(received_at);
        self.peer_timestamp = None;
        if held > MAX_TIMESTAMP_HOLD {
            return None;
        }
        Some(timestamp.wrapping_add((held.as_millis() as u64 % TIMESTAMP_MODULUS) as u16))
    }

    /// §16.6 由对端回显算出一次 RTT 采样 (毫秒), 无效采样返回 `None`。
    pub fn rtt_sample_at(&self, reply: u16, now: Instant) -> Option<f64> {
        let elapsed = self.now16_at(now).wrapping_sub(reply) as u64;
        (elapsed <= MAX_RTT_SAMPLE_MS).then(|| elapsed as f64)
    }
}

/// §16.6 心跳管理器。
///
/// 发送与接收分开记时: 心跳该不该发只取决于本端多久没发过东西, 关联算不算超时只取决于
/// 多久没收到过东西。早先的实现只有一个"最后活动时间", 于是本端不停发送就能让一个早已
/// 消失的对端永远不超时。
pub struct HeartbeatManager {
    last_send: Instant,
    last_receive: Instant,
}

impl HeartbeatManager {
    /// §16.6 创建新的心跳管理器。
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub fn new_at(now: Instant) -> Self {
        Self {
            last_send: now,
            last_receive: now,
        }
    }

    /// §16.6 标记一次发送。
    pub fn on_send(&mut self) {
        self.on_send_at(Instant::now());
    }

    pub fn on_send_at(&mut self, now: Instant) {
        self.last_send = now;
    }

    /// §16.6 标记一次 (通过认证的) 接收。
    pub fn on_receive(&mut self) {
        self.on_receive_at(Instant::now());
    }

    pub fn on_receive_at(&mut self, now: Instant) {
        self.last_receive = now;
    }

    /// §16.6 距离上次发送经过的时间。
    pub fn since_last_send_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_send)
    }

    /// §16.6 距离上次接收经过的时间。
    pub fn since_last_receive_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_receive)
    }

    /// §16.6 是否该补一个心跳包 (距上次发送 ≥ ACK_INTERVAL)。
    pub fn needs_heartbeat_at(&self, now: Instant) -> bool {
        self.since_last_send_at(now) >= ACK_INTERVAL
    }

    /// §16.6 关联是否已超时 (距上次接收 ≥ SERVER_ASSOCIATION_TIMEOUT)。
    pub fn association_expired_at(&self, now: Instant) -> bool {
        self.since_last_receive_at(now) >= SERVER_ASSOCIATION_TIMEOUT
    }

    pub fn needs_heartbeat(&self) -> bool {
        self.needs_heartbeat_at(Instant::now())
    }

    pub fn association_expired(&self) -> bool {
        self.association_expired_at(Instant::now())
    }
}
