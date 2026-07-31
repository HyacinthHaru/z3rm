//! §16.6 数据报分帧: 每包头部、分片与重组。
//!
//! 每个数据报都带一个固定 13 字节的头部, 它位于 **AEAD 密文内部**, 因此驱动重组与
//! RTT 估计的所有字段都是被认证过的。如果头部放在密文外面, 任何链路上的攻击者不需要
//! 会话密钥就能改写 `fragment_count` 或 `message_id`, 从而破坏重组状态或撑爆重组缓冲。
//!
//! 头部布局 (全部大端):
//!
//! ```text
//! 偏移  长度  字段
//! 0     4     message_id        同一条上层消息的所有分片共用
//! 4     2     fragment_index    分片序号, 必须 < fragment_count
//! 6     2     fragment_count    分片总数, 至少为 1
//! 8     2     timestamp         发送端 16 位毫秒时钟 (mosh 风格 RTT 探测)
//! 10    2     timestamp_reply   回显对端的 timestamp, 仅当 flags 置位时有效
//! 12    1     flags             bit0 = timestamp_reply 有效, bit1 = 心跳包
//! ```
//!
//! 之所以携带 `fragment_count` 而不是"还有更多"标志位, 是因为接收端在收到**任意**
//! 一个分片时就能知道总数, 从而立刻分配定长槽位并检出缺失分片, 不必等到最后一片。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::crypto::PACKET_OVERHEAD;

// §16.6 MTU: 1280 字节 (IPv6 最小 MTU, 免受路径 MTU 发现失败的影响)。
pub const MTU: usize = 1280;

// §16.6 每包头部长度。
pub const DATAGRAM_HEADER_LEN: usize = 13;

// §16.6 单个分片可承载的最大载荷。
pub const MAX_FRAGMENT_PAYLOAD: usize = MTU - PACKET_OVERHEAD - DATAGRAM_HEADER_LEN;

// §16.6 单条消息的最大长度 (4 MiB)。
//
// 本层不重传, 丢一片整条消息就作废, 所以消息越大送达概率越低: 4 MiB 已经要 3374 个
// 分片, 在 1% 丢包率下几乎不可能完整到达。这个上限存在的意义是给重组缓冲一个硬边界,
// 上层仍应把单条消息控制在几十 KB 以内。
pub const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

// §16.6 单条消息的最大分片数。
pub const MAX_FRAGMENTS_PER_MESSAGE: usize = MAX_MESSAGE_SIZE.div_ceil(MAX_FRAGMENT_PAYLOAD);

// §16.6 重组缓冲的总内存上限 (含分片槽位本身的开销)。
pub const MAX_REASSEMBLY_BYTES: usize = 8 * 1024 * 1024;

// §16.6 同时在途的未完成消息数上限。
pub const MAX_REASSEMBLY_MESSAGES: usize = 64;

// §16.6 重组超时下界。
pub const REASSEMBLY_TIMEOUT_MIN: Duration = Duration::from_secs(1);

// §16.6 重组超时上界。
pub const REASSEMBLY_TIMEOUT_MAX: Duration = Duration::from_secs(10);

const FLAG_TIMESTAMP_REPLY: u8 = 1 << 0;
const FLAG_KEEPALIVE: u8 = 1 << 1;

const _: () = assert!(MAX_FRAGMENTS_PER_MESSAGE <= u16::MAX as usize);
const _: () = assert!(MAX_FRAGMENT_PAYLOAD > 0);

/// §16.6 由 RTO 推出的重组超时。
///
/// 本层不重传, 因此几个 RTO 之内没能补齐的分片集合以后也补不齐了 —— 及早丢掉可以
/// 防止丢包率高的链路把重组缓冲钉死。
pub fn reassembly_timeout(rto: Duration) -> Duration {
    (rto * 8).clamp(REASSEMBLY_TIMEOUT_MIN, REASSEMBLY_TIMEOUT_MAX)
}

/// §16.6 校验一条上层消息是否可以被分片发送。
pub fn check_message_len(len: usize) -> Result<()> {
    if len > MAX_MESSAGE_SIZE {
        bail!(
            "message of {} bytes exceeds the {} byte datagram message limit",
            len,
            MAX_MESSAGE_SIZE
        );
    }
    Ok(())
}

/// §16.6 一条长度为 `len` 的消息会被切成多少个分片。
pub fn fragment_count_for(len: usize) -> Result<u16> {
    check_message_len(len)?;
    let count = len.div_ceil(MAX_FRAGMENT_PAYLOAD).max(1);
    u16::try_from(count).context("fragment count overflows u16")
}

/// §16.6 每个数据报的头部。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatagramHeader {
    pub message_id: u32,
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub timestamp: u16,
    pub timestamp_reply: Option<u16>,
    pub keepalive: bool,
}

impl DatagramHeader {
    pub fn encode(&self) -> [u8; DATAGRAM_HEADER_LEN] {
        let mut out = [0u8; DATAGRAM_HEADER_LEN];
        out[0..4].copy_from_slice(&self.message_id.to_be_bytes());
        out[4..6].copy_from_slice(&self.fragment_index.to_be_bytes());
        out[6..8].copy_from_slice(&self.fragment_count.to_be_bytes());
        out[8..10].copy_from_slice(&self.timestamp.to_be_bytes());
        out[10..12].copy_from_slice(&self.timestamp_reply.unwrap_or(0).to_be_bytes());
        let mut flags = 0u8;
        if self.timestamp_reply.is_some() {
            flags |= FLAG_TIMESTAMP_REPLY;
        }
        if self.keepalive {
            flags |= FLAG_KEEPALIVE;
        }
        out[12] = flags;
        out
    }

    /// §16.6 从已解密的明文中解析头部, 返回 (头部, 分片载荷)。
    pub fn decode(plaintext: &[u8]) -> Result<(Self, &[u8])> {
        let header = plaintext
            .get(..DATAGRAM_HEADER_LEN)
            .context("datagram shorter than its header")?;
        let payload = plaintext
            .get(DATAGRAM_HEADER_LEN..)
            .context("datagram shorter than its header")?;

        let message_id = u32::from_be_bytes(header[0..4].try_into()?);
        let fragment_index = u16::from_be_bytes(header[4..6].try_into()?);
        let fragment_count = u16::from_be_bytes(header[6..8].try_into()?);
        let timestamp = u16::from_be_bytes(header[8..10].try_into()?);
        let reply = u16::from_be_bytes(header[10..12].try_into()?);
        let flags = header[12];

        if fragment_count == 0 {
            bail!("fragment count must be at least 1");
        }
        if fragment_index >= fragment_count {
            bail!(
                "fragment index {} out of range for count {}",
                fragment_index,
                fragment_count
            );
        }
        if fragment_count as usize > MAX_FRAGMENTS_PER_MESSAGE {
            bail!(
                "fragment count {} exceeds the {} fragment limit",
                fragment_count,
                MAX_FRAGMENTS_PER_MESSAGE
            );
        }
        if payload.len() > MAX_FRAGMENT_PAYLOAD {
            bail!(
                "fragment payload of {} bytes exceeds the {} byte limit",
                payload.len(),
                MAX_FRAGMENT_PAYLOAD
            );
        }

        Ok((
            Self {
                message_id,
                fragment_index,
                fragment_count,
                timestamp,
                timestamp_reply: (flags & FLAG_TIMESTAMP_REPLY != 0).then_some(reply),
                keepalive: flags & FLAG_KEEPALIVE != 0,
            },
            payload,
        ))
    }
}

struct PartialMessage {
    fragments: Vec<Option<Vec<u8>>>,
    received_count: usize,
    payload_bytes: usize,
    /// §16.6 计入 [`Reassembler::buffered_bytes`] 的字节数, 含槽位数组本身的开销。
    charged_bytes: usize,
    first_seen: Instant,
}

/// §16.6 分片重组器。
///
/// 内存有双重上限: 在途消息数 ([`MAX_REASSEMBLY_MESSAGES`]) 与总字节数
/// ([`MAX_REASSEMBLY_BYTES`]); 超限时按先到先淘汰逐出最老的未完成消息。此外每条消息
/// 都有 [`reassembly_timeout`] 的存活期, 因此对端只发首片就不再露面也无法把内存钉死。
pub struct Reassembler {
    partial: HashMap<u32, PartialMessage>,
    buffered_bytes: usize,
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            partial: HashMap::new(),
            buffered_bytes: 0,
        }
    }

    /// §16.6 收下一个分片。消息集齐时返回完整消息, 否则返回 `None`。
    ///
    /// 调用方必须先完成 AEAD 认证: 本函数假定 `header` 与 `payload` 来自对端。
    pub fn accept(
        &mut self,
        header: &DatagramHeader,
        payload: &[u8],
        now: Instant,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        self.purge_expired(now, timeout);

        if header.fragment_count == 1 {
            // 未分片的消息不进重组表, 因此常见路径没有任何 HashMap 开销。
            return Ok(Some(payload.to_vec()));
        }

        let fragment_count = header.fragment_count as usize;
        let index = header.fragment_index as usize;
        let slot_overhead = fragment_count * size_of::<Option<Vec<u8>>>();

        if !self.partial.contains_key(&header.message_id) {
            if slot_overhead > MAX_REASSEMBLY_BYTES {
                bail!("fragment table for a single message exceeds the reassembly budget");
            }
            while self.partial.len() >= MAX_REASSEMBLY_MESSAGES
                || self.buffered_bytes + slot_overhead > MAX_REASSEMBLY_BYTES
            {
                if !self.evict_oldest() {
                    break;
                }
            }
            if self.partial.len() >= MAX_REASSEMBLY_MESSAGES
                || self.buffered_bytes + slot_overhead > MAX_REASSEMBLY_BYTES
            {
                bail!("reassembly buffer exhausted");
            }
            self.partial.insert(
                header.message_id,
                PartialMessage {
                    fragments: vec![None; fragment_count],
                    received_count: 0,
                    payload_bytes: 0,
                    charged_bytes: slot_overhead,
                    first_seen: now,
                },
            );
            self.buffered_bytes += slot_overhead;
        }

        let complete = {
            let message = self
                .partial
                .get_mut(&header.message_id)
                .context("reassembly entry disappeared")?;
            if message.fragments.len() != fragment_count {
                bail!(
                    "fragment count changed from {} to {} for message {}",
                    message.fragments.len(),
                    fragment_count,
                    header.message_id
                );
            }
            let slot = message
                .fragments
                .get_mut(index)
                .context("fragment index out of range")?;
            if slot.is_some() {
                // 重复分片: 保留先到的那份, 不重复计费。
                return Ok(None);
            }
            if message.payload_bytes + payload.len() > MAX_MESSAGE_SIZE {
                bail!("reassembled message exceeds the message size limit");
            }
            *slot = Some(payload.to_vec());
            message.received_count += 1;
            message.payload_bytes += payload.len();
            message.charged_bytes += payload.len();
            message.received_count == fragment_count
        };
        self.buffered_bytes += payload.len();

        if !complete {
            return Ok(None);
        }

        let message = self
            .partial
            .remove(&header.message_id)
            .context("completed reassembly entry disappeared")?;
        self.buffered_bytes = self.buffered_bytes.saturating_sub(message.charged_bytes);

        let mut assembled = Vec::with_capacity(message.payload_bytes);
        for fragment in message.fragments {
            let fragment = fragment.context("complete message is missing a fragment")?;
            assembled.extend_from_slice(&fragment);
        }
        Ok(Some(assembled))
    }

    /// §16.6 丢弃存活超过 `timeout` 的未完成消息, 返回被丢弃的条数。
    pub fn purge_expired(&mut self, now: Instant, timeout: Duration) -> usize {
        let mut freed = 0usize;
        let mut purged = 0usize;
        self.partial.retain(|_, message| {
            if now.saturating_duration_since(message.first_seen) >= timeout {
                freed += message.charged_bytes;
                purged += 1;
                false
            } else {
                true
            }
        });
        self.buffered_bytes = self.buffered_bytes.saturating_sub(freed);
        purged
    }

    /// §16.6 当前在途的未完成消息数。
    pub fn pending_messages(&self) -> usize {
        self.partial.len()
    }

    /// §16.6 当前重组缓冲占用的字节数。
    pub fn buffered_bytes(&self) -> usize {
        self.buffered_bytes
    }

    fn evict_oldest(&mut self) -> bool {
        let oldest = self
            .partial
            .iter()
            .min_by_key(|(_, message)| message.first_seen)
            .map(|(message_id, _)| *message_id);
        let Some(oldest) = oldest else {
            return false;
        };
        if let Some(message) = self.partial.remove(&oldest) {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(message.charged_bytes);
        }
        true
    }
}
