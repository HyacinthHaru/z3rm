//! §16.6 RTT 估计 + 帧率控制 + 心跳机制。
//!
//! 基于 TCP 的 SRTT/RTTVAR 算法 (RFC 6298)，
//! 适配 mosh 论文的 UDP 参数。

use std::time::{Duration, Instant};

// §16.6 RTO 下限: 50ms (不采用 TCP 的 1s，适配终端场景)。
pub const RTO_MIN: Duration = Duration::from_millis(50);

// §16.6 RTO 上限: 1000ms。
pub const RTO_MAX: Duration = Duration::from_millis(1000);

// §16.6 心跳间隔: 3000ms (无数据时发送心跳包)。
pub const ACK_INTERVAL: Duration = Duration::from_millis(3000);

// §16.6 服务器关联超时: 40s (无活动后断开)。
pub const SERVER_ASSOCIATION_TIMEOUT: Duration = Duration::from_secs(40);

// §16.6 帧率控制: 最小发送间隔 20ms。
pub const SEND_INTERVAL_MIN: Duration = Duration::from_millis(20);

// §16.6 帧率控制: 最大发送间隔 250ms。
pub const SEND_INTERVAL_MAX: Duration = Duration::from_millis(250);

/// §16.6 TCP 风格 RTT 估计器 (RFC 6298 / mosh)。
///
/// 平滑 RTT (SRTT) 和 RTT 方差 (RTTVAR) 用于计算重传超时 (RTO)。
/// RTO = SRTT + max(4*RTTVAR, G) where G = 50ms。
pub struct RttEstimator {
    /// §16.6 平滑 RTT (初始值: RTO_MIN)。
    srtt: f64, // 毫秒
    /// §16.6 RTT 方差估计 (初始值: RTO_MIN / 2)。
    rttvar: f64, // 毫秒
    /// §16.6 是否已有首次测量。
    initialized: bool,
}

impl RttEstimator {
    /// §16.6 创建新的 RTT 估计器。
    pub fn new() -> Self {
        Self {
            srtt: RTO_MIN.as_millis() as f64,
            rttvar: (RTO_MIN.as_millis() as f64) / 2.0,
            initialized: false,
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
        if !self.initialized {
            self.srtt = sample_ms;
            self.rttvar = sample_ms / 2.0;
            self.initialized = true;
        } else {
            let deviation = (self.srtt - sample_ms).abs();
            self.rttvar = (3.0 / 4.0) * self.rttvar + (1.0 / 4.0) * deviation;
            self.srtt = (7.0 / 8.0) * self.srtt + (1.0 / 8.0) * sample_ms;
        }
    }

    /// §16.6 获取当前 RTO (重传超时)。
    /// RTO = SRTT + max(4 * RTTVAR, G)
    /// 限制在 [RTO_MIN, RTO_MAX] 范围内。
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
}

/// §16.6 心跳管理器。
///
/// 跟踪最后一次活动时间，决定是否需要发送心跳。
pub struct HeartbeatManager {
    /// §16.6 最后一次收到数据的时间。
    last_activity: Instant,
}

impl HeartbeatManager {
    /// §16.6 创建新的心跳管理器。
    pub fn new() -> Self {
        Self {
            last_activity: Instant::now(),
        }
    }

    /// §16.6 标记一次活动 (收到/发送数据)。
    pub fn on_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// §16.6 距离上次活动经过的时间。
    pub fn idle_duration(&self) -> Duration {
        self.last_activity.elapsed()
    }

    /// §16.6 检查是否需要发送心跳包 (idle ≥ ACK_INTERVAL)。
    pub fn needs_heartbeat(&self) -> bool {
        self.idle_duration() >= ACK_INTERVAL
    }

    /// §16.6 检查关联是否已超时 (idle ≥ SERVER_ASSOCIATION_TIMEOUT)。
    pub fn association_expired(&self) -> bool {
        self.idle_duration() >= SERVER_ASSOCIATION_TIMEOUT
    }
}
