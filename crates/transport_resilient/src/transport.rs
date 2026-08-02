//! §16.6 UDP 数据报会话。
//!
//! [`UdpSession`] 是本 crate 唯一的端点类型, 两个构造函数决定它扮演哪一端:
//!
//! * [`UdpSession::connect`] —— 客户端: 目标地址在连接时固定, 发送方向为
//!   `ClientToServer`。
//! * [`UdpSession::bind`] —— 服务端: 对端地址初始为"未知", 每收到一个**通过认证**的
//!   数据报就更新为该数据报的来源 (无状态漫游), 发送方向为 `ServerToClient`。
//!
//! 之所以不导出 `Direction` 让调用方自己选, 是因为一个会话密钥的两端必须使用不同方向,
//! 否则 nonce 空间重叠、AES-GCM 立刻失效 (见 `crypto` 模块的 nonce 唯一性论证)。把方向
//! 藏进构造函数, 这个约束就不可能被调用方破坏。

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context as _, Result};
use tokio::net::UdpSocket;

use super::crypto::{Direction, KEY_SIZE, PacketCodec};
use super::packet::{
    DATAGRAM_HEADER_LEN, DatagramHeader, MAX_FRAGMENT_PAYLOAD, MTU, Reassembler, check_message_len,
    reassembly_timeout,
};
use super::rtt::{HeartbeatManager, RttEstimator, RttSnapshot, TimestampTracker};

// §16.6 UDP 接收缓冲区大小。
//
// 本协议自己发出的数据报永不超过 MTU, 更大的数据报只能来自别人, 会在 AEAD 认证时被拒;
// 因此按 MTU 量级取缓冲即可, 不必为每次 recv 准备 64 KiB。
const RECV_BUF_SIZE: usize = 2048;

const _: () = assert!(RECV_BUF_SIZE >= MTU);

struct SessionState {
    reassembler: Reassembler,
    rtt: RttEstimator,
    timestamps: TimestampTracker,
    heartbeat: HeartbeatManager,
    /// §16.6 下一条消息的 id。
    ///
    /// u32 单调递增后回绕: 结合重组超时, 只有在超时窗口内发出 2^32 条消息才会撞号,
    /// 任何真实链路都做不到。
    next_message_id: u32,
}

/// §16.6 一个加密 UDP 数据报会话。
///
/// 交付语义: 消息**至多一次**、可能丢失、可能乱序。分片消息是全有或全无 —— 缺片的消息
/// 绝不会以截断形式交付给上层, 重组缓冲超时后即被丢弃。可靠性与顺序由上层负责。
pub struct UdpSession {
    socket: UdpSocket,
    codec: PacketCodec,
    peer: parking_lot::Mutex<Option<SocketAddr>>,
    /// §16.6 是否允许对端地址随认证过的数据报迁移 (服务端漫游)。
    roaming: bool,
    state: parking_lot::Mutex<SessionState>,
}

impl UdpSession {
    /// §16.6 客户端: 绑定本地临时端口并固定服务端地址。
    pub async fn connect(server_addr: SocketAddr, session_key: [u8; KEY_SIZE]) -> Result<Self> {
        // 绑定的地址族必须跟目标一致, 否则 IPv6 服务端会得到一个无法发包的 socket。
        let bind_addr: SocketAddr = if server_addr.is_ipv4() {
            "0.0.0.0:0".parse()?
        } else {
            "[::]:0".parse()?
        };
        let socket = UdpSocket::bind(bind_addr).await?;
        Ok(Self::new(
            socket,
            session_key,
            Direction::ClientToServer,
            Some(server_addr),
            false,
        ))
    }

    /// §16.6 服务端: 绑定监听地址, 对端地址等第一个认证通过的数据报确定。
    pub async fn bind(local_addr: SocketAddr, session_key: [u8; KEY_SIZE]) -> Result<Self> {
        let socket = UdpSocket::bind(local_addr).await?;
        Ok(Self::new(
            socket,
            session_key,
            Direction::ServerToClient,
            None,
            true,
        ))
    }

    fn new(
        socket: UdpSocket,
        session_key: [u8; KEY_SIZE],
        send_direction: Direction,
        peer: Option<SocketAddr>,
        roaming: bool,
    ) -> Self {
        let now = Instant::now();
        Self {
            socket,
            codec: PacketCodec::new(session_key, send_direction),
            peer: parking_lot::Mutex::new(peer),
            roaming,
            state: parking_lot::Mutex::new(SessionState {
                reassembler: Reassembler::new(),
                rtt: RttEstimator::new(),
                timestamps: TimestampTracker::new_at(now),
                heartbeat: HeartbeatManager::new_at(now),
                next_message_id: 0,
            }),
        }
    }

    /// §16.6 发送一条上层消息: 分片 → 加密 → 逐个数据报发出。
    pub async fn send_message(&self, payload: &[u8]) -> Result<()> {
        self.send_fragmented(payload, false).await
    }

    /// §16.6 发送一个心跳包。它不携带上层数据, 接收端不会把它交付给上层。
    pub async fn send_heartbeat(&self) -> Result<()> {
        self.send_fragmented(&[], true).await
    }

    /// §16.6 距上次发送超过 `ACK_INTERVAL` 时补一个心跳包, 返回是否真的发了。
    ///
    /// 对端地址未知时不发 (服务端在收到第一个包之前无处可发)。
    pub async fn send_heartbeat_if_needed(&self) -> Result<bool> {
        if !self.needs_heartbeat() || self.peer_addr().is_none() {
            return Ok(false);
        }
        self.send_heartbeat().await?;
        Ok(true)
    }

    async fn send_fragmented(&self, payload: &[u8], keepalive: bool) -> Result<()> {
        let peer = (*self.peer.lock()).context(
            "peer address is still unknown; refusing to send before an authenticated datagram arrives",
        )?;

        check_message_len(payload.len())?;
        let mut fragments: Vec<&[u8]> = payload.chunks(MAX_FRAGMENT_PAYLOAD).collect();
        if fragments.is_empty() {
            // 空消息 (心跳) 仍然要占一个分片, 否则没有数据报可发。
            fragments.push(&[]);
        }
        let fragment_count =
            u16::try_from(fragments.len()).context("fragment count overflows u16")?;

        let now = Instant::now();
        let (message_id, timestamp, timestamp_reply) = {
            let mut state = self.state.lock();
            let message_id = state.next_message_id;
            state.next_message_id = state.next_message_id.wrapping_add(1);
            let timestamp = state.timestamps.now16_at(now);
            let timestamp_reply = state.timestamps.take_reply_at(now);
            (message_id, timestamp, timestamp_reply)
        };

        // 先把全部分片加密好再碰 socket: 中途加密失败时对端不会收到半条消息。
        let mut datagrams = Vec::with_capacity(fragments.len());
        for (index, fragment) in fragments.iter().enumerate() {
            let header = DatagramHeader {
                message_id,
                fragment_index: u16::try_from(index).context("fragment index overflows u16")?,
                fragment_count,
                timestamp,
                timestamp_reply,
                keepalive,
            };
            let mut plaintext = Vec::with_capacity(DATAGRAM_HEADER_LEN + fragment.len());
            plaintext.extend_from_slice(&header.encode());
            plaintext.extend_from_slice(fragment);
            datagrams.push(self.codec.encrypt(&plaintext)?);
        }

        for datagram in &datagrams {
            self.socket.send_to(datagram, peer).await?;
        }
        self.state.lock().heartbeat.on_send();
        Ok(())
    }

    /// §16.6 接收一条完整的上层消息。
    ///
    /// 心跳包、缺片未集齐的消息、以及任何未通过认证或格式非法的数据报都不会返回给调用方 ——
    /// 函数继续等待下一个数据报。只有 socket 本身出错才会返回 `Err`。
    pub async fn recv_message(&self) -> Result<Vec<u8>> {
        let mut buffer = [0u8; RECV_BUF_SIZE];
        loop {
            let (len, from) = self.socket.recv_from(&mut buffer).await?;
            let datagram = buffer
                .get(..len)
                .context("recv_from reported more bytes than the buffer holds")?;

            let plaintext = match self.codec.decrypt(datagram) {
                Ok(plaintext) => plaintext,
                Err(error) => {
                    // 任何人都能往 UDP socket 里灌数据报。把认证失败当成致命错误, 等于
                    // 允许链路上的任何人一发包就杀掉会话; 因此这里记录后丢弃并继续等待。
                    tracing::debug!(source = %from, error = %error, "dropping unauthenticated datagram");
                    continue;
                }
            };

            // 只有在数据报通过认证之后才迁移对端地址。早先的实现在解密之前就把地址改成
            // 来源地址, 于是任何人伪造一个源地址就能把会话的下行流量劫走。
            if self.roaming {
                *self.peer.lock() = Some(from);
            }

            let (header, fragment) = match DatagramHeader::decode(&plaintext) {
                Ok(decoded) => decoded,
                Err(error) => {
                    tracing::debug!(source = %from, error = %error, "dropping malformed datagram");
                    continue;
                }
            };

            let now = Instant::now();
            let accepted = {
                let mut state = self.state.lock();
                let SessionState {
                    reassembler,
                    rtt,
                    timestamps,
                    heartbeat,
                    ..
                } = &mut *state;
                heartbeat.on_receive_at(now);
                if let Some(reply) = header.timestamp_reply {
                    if let Some(sample) = timestamps.rtt_sample_at(reply, now) {
                        rtt.record_rtt(sample);
                    }
                }
                timestamps.record_peer_timestamp_at(header.timestamp, now);
                if header.keepalive {
                    Ok(None)
                } else {
                    reassembler.accept(&header, fragment, now, reassembly_timeout(rtt.rto()))
                }
            };

            match accepted {
                Ok(Some(message)) => return Ok(message),
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(source = %from, error = %error, "dropping unreassemblable datagram");
                    continue;
                }
            }
        }
    }

    /// §16.6 当前对端地址。服务端在收到第一个认证通过的数据报之前为 `None`。
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        *self.peer.lock()
    }

    /// §16.6 本地绑定地址。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// §16.6 本会话的发送方向。
    pub fn send_direction(&self) -> Direction {
        self.codec.send_direction()
    }

    /// §16.6 RTT / 帧率控制的只读快照。
    pub fn rtt(&self) -> RttSnapshot {
        self.state.lock().rtt.snapshot()
    }

    /// §16.6 是否该补一个心跳包。
    pub fn needs_heartbeat(&self) -> bool {
        self.state
            .lock()
            .heartbeat
            .needs_heartbeat_at(Instant::now())
    }

    /// §16.6 关联是否已超时 (40s 没收到任何认证通过的数据报)。
    pub fn association_expired(&self) -> bool {
        self.state
            .lock()
            .heartbeat
            .association_expired_at(Instant::now())
    }

    /// §16.6 距上次发送经过的时间。
    pub fn since_last_send(&self) -> std::time::Duration {
        self.state
            .lock()
            .heartbeat
            .since_last_send_at(Instant::now())
    }

    /// §16.6 距上次接收经过的时间。
    pub fn since_last_receive(&self) -> std::time::Duration {
        self.state
            .lock()
            .heartbeat
            .since_last_receive_at(Instant::now())
    }

    /// §16.6 当前在途的未完成分片消息数 (用于测试与诊断)。
    pub fn pending_reassemblies(&self) -> usize {
        self.state.lock().reassembler.pending_messages()
    }
}
