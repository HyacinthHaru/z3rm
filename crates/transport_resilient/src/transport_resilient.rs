//! # transport_resilient
//!
//! §16.6 mosh 风格的 UDP 可靠传输层。
//!
//! 核心特性:
//! - 每包 AEAD 加密 (AES-256-GCM)。
//! - 无状态漫游: 服务端自动跟踪客户端来源地址。
//! - RTT 估计 + 帧率控制 (RFC 6298)。
//! - 心跳机制: 3s 无活动发送心跳，40s 超时断开。
//! - 分片传输: MTU = 1280 字节。

pub mod crypto;
pub mod rtt;
pub mod transport;

#[cfg(test)]
mod tests;

// §16.6 导出核心类型。
pub use crypto::PacketCodec;
pub use rtt::{HeartbeatManager, RttEstimator};
pub use transport::{UdpClient, UdpServer};

// §16.6 导出常量。
pub use crypto::{KEY_SIZE, NONCE_SIZE, REPLAY_WINDOW_SIZE};
pub use rtt::{
    ACK_INTERVAL, RTO_MAX, RTO_MIN, SEND_INTERVAL_MAX, SEND_INTERVAL_MIN,
    SERVER_ASSOCIATION_TIMEOUT,
};
pub use transport::MTU;

use anyhow::Result;
use mux_protocol::proto::Envelope;
use mux_protocol::{frame, unframe};

/// §16.6 UDP 可靠传输封装。
///
/// 将 mux_protocol Envelope 序列化为长度前缀帧，
/// 经过 AEAD 加密后通过 UDP 发送。
/// 大包自动分片 (MTU = 1280)。
pub struct UdpResilientTransport {
    /// §16.6 底层 UDP 客户端。
    inner: transport::UdpClient,
}

impl UdpResilientTransport {
    /// §16.6 连接到 UDP 服务端。
    pub async fn connect(
        server_addr: std::net::SocketAddr,
        session_key: [u8; KEY_SIZE],
    ) -> Result<Self> {
        let inner = transport::UdpClient::connect(server_addr, session_key).await?;
        Ok(Self { inner })
    }

    /// §16.6 发送 Envelope: 帧化 → 分片 → 加密 → UDP 发送。
    pub async fn send(&mut self, msg: &Envelope) -> Result<()> {
        // §16.6 长度前缀帧化。
        let framed = frame(msg)?;

        if framed.len() <= MTU {
            // §16.6 小包直接发送。
            self.inner.send(&framed).await
        } else {
            // §16.6 大包分片发送 (MTU 限制)。
            let total_len = framed.len();
            let mut offset = 0;

            while offset < total_len {
                let chunk_end = std::cmp::min(offset + MTU, total_len);
                let chunk = &framed[offset..chunk_end];
                self.inner.send(chunk).await?;
                offset = chunk_end;
            }
            Ok(())
        }
    }

    /// §16.6 接收 Envelope: UDP 接收 → 解密 → 帧解码。
    pub async fn recv(&mut self) -> Result<Envelope> {
        let plaintext = self.inner.recv().await?;
        let (msg, _) = unframe(&plaintext)?;
        Ok(msg)
    }

    /// §16.6 获取 RTT 估计器引用。
    pub fn rtt_estimator(&self) -> &rtt::RttEstimator {
        self.inner.rtt_estimator()
    }

    /// §16.6 获取心跳管理器引用。
    pub fn heartbeat_manager(&self) -> &rtt::HeartbeatManager {
        self.inner.heartbeat_manager()
    }

    /// §16.6 获取本地地址。
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }
}
