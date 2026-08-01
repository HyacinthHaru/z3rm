//! # transport_resilient
//!
//! §16.6 mosh 风格的**加密 UDP 数据报传输层**。
//!
//! 这里的 "resilient" 指的是链路层面的韧性 —— 客户端换 IP、丢包、断流都不会毁掉会话 ——
//! 而**不是**可靠交付。本 crate 不做 ACK、不重传、不排序、没有发送窗口。
//!
//! 提供:
//! - 每包 AEAD 加密 (AES-256-GCM); 收发两个方向使用互不相交的 nonce 空间。
//! - 重放保护: 128 包滑动窗口, 且只在包通过认证之后才推进。
//! - 无状态漫游: 服务端在数据报**通过认证之后**才把对端地址迁移到新来源。
//! - 分片与重组: MTU = 1280 字节, 重组带超时与内存上限。
//! - RTT 估计 (RFC 6298) 与帧率控制, 采样通过每包时间戳/回显完成。
//! - 心跳: 3s 没发过东西就补一个心跳包, 40s 没收到东西即判定关联超时。
//!
//! **不提供** (由上层负责):
//! - 可靠交付: 消息可能丢失, 本层不会重传, 也没有 ACK。
//! - 顺序: 消息可能乱序到达。单条消息内部的分片重组是有序的, 消息之间不是。
//! - 去重之外的流控: 除了重放窗口, 没有发送窗口或拥塞控制。
//!
//! 分片消息是全有或全无: 缺片的消息绝不会以截断形式交付, 超时后整条丢弃。
//!
//! 这个取舍与 mosh 一致, 也与 spec 的"权威重连"模型一致 (§15.4, §15.12): 断流之后正确的
//! 恢复方式是向服务端重新拉一份权威快照, 而不是把旧字节补发一遍。因此在这一层实现字节流
//! 可靠性只会与上层的重同步语义重复且互相打架。
//!
//! 注意: 本 crate 目前尚未接入主流程 —— spec §3.8 把 UDP resilient transport 标记为
//! deferred。它是一个自洽、可独立测试的库, 等接线时可用。

pub mod crypto;
pub mod packet;
pub mod rtt;
pub mod transport;

#[cfg(test)]
mod tests;

// §16.6 导出核心类型。
pub use crypto::{Direction, PacketCodec, PacketWindow};
pub use packet::{DatagramHeader, Reassembler};
pub use rtt::{HeartbeatManager, RttEstimator, RttSnapshot, TimestampTracker};
pub use transport::UdpSession;

// §16.6 导出常量。
pub use crypto::{
    GCM_TAG_LEN, KEY_SIZE, MAX_SEQUENCE, NONCE_SIZE, PACKET_OVERHEAD, REPLAY_WINDOW_SIZE,
};
pub use packet::{
    DATAGRAM_HEADER_LEN, MAX_FRAGMENT_PAYLOAD, MAX_FRAGMENTS_PER_MESSAGE, MAX_MESSAGE_SIZE,
    MAX_REASSEMBLY_BYTES, MAX_REASSEMBLY_MESSAGES, MTU, REASSEMBLY_TIMEOUT_MAX,
    REASSEMBLY_TIMEOUT_MIN,
};
pub use rtt::{
    ACK_INTERVAL, RTO_MAX, RTO_MIN, SEND_INTERVAL_MAX, SEND_INTERVAL_MIN,
    SERVER_ASSOCIATION_TIMEOUT,
};

use anyhow::{Result, bail};
use mux_protocol::proto::Envelope;
use mux_protocol::{frame, unframe};

/// §16.6 在 [`UdpSession`] 之上收发 mux_protocol `Envelope`。
///
/// 每条上层消息就是一个长度前缀帧, 由会话层负责分片、加密与重组。交付语义与
/// [`UdpSession`] 相同: 至多一次、可能丢失、可能乱序。
pub struct UdpResilientTransport {
    session: UdpSession,
}

impl UdpResilientTransport {
    /// §16.6 客户端: 连接到 UDP 服务端。
    pub async fn connect(
        server_addr: std::net::SocketAddr,
        session_key: [u8; KEY_SIZE],
    ) -> Result<Self> {
        Ok(Self {
            session: UdpSession::connect(server_addr, session_key).await?,
        })
    }

    /// §16.6 服务端: 绑定监听地址。
    ///
    /// 在收到第一个认证通过的数据报之前对端地址未知, 此时 [`Self::send`] 会返回错误。
    pub async fn bind(
        local_addr: std::net::SocketAddr,
        session_key: [u8; KEY_SIZE],
    ) -> Result<Self> {
        Ok(Self {
            session: UdpSession::bind(local_addr, session_key).await?,
        })
    }

    /// §16.6 发送 Envelope: 帧化 → 分片 → 加密 → UDP 发送。
    pub async fn send(&self, msg: &Envelope) -> Result<()> {
        let framed = frame(msg)?;
        self.session.send_message(&framed).await
    }

    /// §16.6 接收 Envelope: UDP 接收 → 解密 → 重组 → 帧解码。
    pub async fn recv(&self) -> Result<Envelope> {
        let framed = self.session.recv_message().await?;
        let (msg, consumed) = unframe(&framed)?;
        // 一条消息恰好承载一个帧。有尾巴说明对端的帧化逻辑与本端不一致, 静默忽略这些
        // 字节会把协议错误藏起来。
        if consumed != framed.len() {
            bail!(
                "envelope frame left {} trailing bytes",
                framed.len() - consumed
            );
        }
        Ok(msg)
    }

    /// §16.6 底层数据报会话 (心跳、RTT、对端地址等)。
    pub fn session(&self) -> &UdpSession {
        &self.session
    }

    /// §16.6 获取本地地址。
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.session.local_addr()
    }
}
