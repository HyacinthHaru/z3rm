//! §16.6 UDP 可靠传输: 客户端与服务端。
//!
//! 服务端: 无状态漫游 (每包更新客户端地址)。
//! 客户端: 固定目标地址。

use std::net::SocketAddr;

use anyhow::Result;
use tokio::net::UdpSocket;

use super::crypto::PacketCodec;
use super::rtt::{HeartbeatManager, RttEstimator};

// §16.6 MTU: 1280 字节 (IPv6 最小 MTU，适配路径 MTU 发现)。
pub const MTU: usize = 1280;

// §16.6 UDP 接收缓冲区大小。
const RECV_BUF_SIZE: usize = 65536;

/// §16.6 UDP 服务端: 支持无状态漫游。
///
/// 每次收到有效数据包时，自动更新客户端地址。
/// 客户端更换 IP (漫游) 后，服务端自动切换到新地址。
pub struct UdpServer {
    /// §16.6 UDP 套接字。
    socket: UdpSocket,
    /// §16.6 当前客户端地址 (每次收包更新)。
    client_addr: parking_lot::Mutex<SocketAddr>,
    /// §16.6 AEAD 编解码器。
    codec: PacketCodec,
    /// §16.6 RTT 估计器。
    rtt: parking_lot::Mutex<RttEstimator>,
    /// §16.6 心跳管理器。
    heartbeat: parking_lot::Mutex<HeartbeatManager>,
}

impl UdpServer {
    /// §16.6 创建新的 UDP 服务端，绑定到指定地址。
    pub async fn bind(local_addr: SocketAddr, session_key: [u8; 32]) -> Result<Self> {
        let socket = UdpSocket::bind(local_addr).await?;
        Ok(Self {
            socket,
            client_addr: parking_lot::Mutex::new(local_addr),
            codec: PacketCodec::new(session_key),
            rtt: parking_lot::Mutex::new(RttEstimator::new()),
            heartbeat: parking_lot::Mutex::new(HeartbeatManager::new()),
        })
    }

    /// §16.6 接收数据包: 解密 + 更新客户端地址 (漫游)。
    ///
    /// 返回 (明文数据, 发送者地址)。
    pub async fn recv(&self) -> Result<(Vec<u8>, SocketAddr)> {
        let mut buf = vec![0u8; RECV_BUF_SIZE];
        let (len, addr) = self.socket.recv_from(&mut buf).await?;

        // §16.6 无状态 roaming: 更新客户端地址为最新数据包来源。
        *self.client_addr.lock() = addr;

        let plaintext = self.codec.decrypt(&buf[..len])?;

        // §16.6 标记活动，重置心跳计时。
        self.heartbeat.lock().on_activity();

        Ok((plaintext, addr))
    }

    /// §16.6 向当前客户端地址发送加密数据包。
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        let addr = *self.client_addr.lock();
        let encrypted = self.codec.encrypt(data);
        self.socket.send_to(&encrypted, addr).await?;

        // §16.6 标记活动。
        self.heartbeat.lock().on_activity();
        Ok(())
    }

    /// §16.6 向指定地址发送加密数据包 (用于心跳探测)。
    pub async fn send_to(&self, data: &[u8], addr: SocketAddr) -> Result<()> {
        let encrypted = self.codec.encrypt(data);
        self.socket.send_to(&encrypted, addr).await?;
        self.heartbeat.lock().on_activity();
        Ok(())
    }

    /// §16.6 获取当前客户端地址。
    pub fn client_addr(&self) -> SocketAddr {
        *self.client_addr.lock()
    }

    /// §16.6 获取本地绑定地址。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// §16.6 获取 RTT 估计器引用。
    pub fn rtt_estimator(&self) -> &parking_lot::Mutex<RttEstimator> {
        &self.rtt
    }
}

/// §16.6 UDP 客户端: 固定目标地址。
///
/// 客户端连接到指定服务端地址，使用 AEAD 加密传输。
pub struct UdpClient {
    /// §16.6 UDP 套接字。
    socket: UdpSocket,
    /// §16.6 服务端地址。
    server_addr: SocketAddr,
    /// §16.6 AEAD 编解码器。
    codec: PacketCodec,
    /// §16.6 RTT 估计器。
    rtt: RttEstimator,
    /// §16.6 心跳管理器。
    heartbeat: HeartbeatManager,
    /// §16.6 发送序列号 (用于 RTT 测量)。
    send_seq: u32,
    /// §16.6 待测序列: seq → send_time 映射。
    pending: std::collections::HashMap<u32, std::time::Instant>,
}

impl UdpClient {
    /// §16.6 创建新的 UDP 客户端，连接到服务端。
    pub async fn connect(server_addr: SocketAddr, session_key: [u8; 32]) -> Result<Self> {
        // §16.6 客户端绑定到任意可用端口。
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            server_addr,
            socket,
            codec: PacketCodec::new(session_key),
            rtt: RttEstimator::new(),
            heartbeat: HeartbeatManager::new(),
            send_seq: 0,
            pending: std::collections::HashMap::new(),
        })
    }

    /// §16.6 发送明文数据到服务端。
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let encrypted = self.codec.encrypt(data);
        self.socket.send_to(&encrypted, self.server_addr).await?;
        self.heartbeat.on_activity();
        Ok(())
    }

    /// §16.6 发送带序列号的数据，用于 RTT 测量。
    pub async fn send_seq(&mut self, data: &[u8]) -> Result<u32> {
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);

        // §16.6 记录发送时间用于 RTT 计算。
        self.pending.insert(seq, std::time::Instant::now());

        let encrypted = self.codec.encrypt(data);
        self.socket.send_to(&encrypted, self.server_addr).await?;
        self.heartbeat.on_activity();
        Ok(seq)
    }

    /// §16.6 接收来自服务端的数据包并解密。
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; RECV_BUF_SIZE];
        let (len, _addr) = self.socket.recv_from(&mut buf).await?;
        let plaintext = self.codec.decrypt(&buf[..len])?;
        self.heartbeat.on_activity();
        Ok(plaintext)
    }

    /// §16.6 处理 RTT 回复: 根据序列号计算 RTT。
    pub fn process_rtt_reply(&mut self, seq: u32) {
        if let Some(send_time) = self.pending.remove(&seq) {
            let elapsed = send_time.elapsed().as_millis() as f64;
            self.rtt.record_rtt(elapsed);
        }
    }

    /// §16.6 获取 RTT 估计器。
    pub fn rtt_estimator(&self) -> &RttEstimator {
        &self.rtt
    }

    /// §16.6 获取心跳管理器。
    pub fn heartbeat_manager(&self) -> &HeartbeatManager {
        &self.heartbeat
    }

    /// §16.6 获取本地绑定地址。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
