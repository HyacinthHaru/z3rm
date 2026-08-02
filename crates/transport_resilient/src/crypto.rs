//! §16.6 每包 AEAD 加密 (AES-256-GCM) 与重放保护。
//!
//! 数据报线格式: `[nonce 计数器 (8, big endian)] [ciphertext || GCM tag]`。
//!
//! # nonce 唯一性论证
//!
//! AES-GCM 一旦对同一 `(key, nonce)` 加密两次就会完全失效: 两段密文异或即得两段
//! 明文的异或, 更严重的是 GHASH 认证子密钥 H 可被恢复, 攻击者从此可以伪造任意
//! 数据包。会话两端共享同一把 32 字节密钥, 所以唯一性必须完全由 nonce 本身提供。
//!
//! nonce 共 12 字节: 4 字节前缀在整个会话内固定且**不上线**, 后接 8 字节大端计数
//! 器 (计数器明文随包传输)。计数器按方向切分:
//!
//! * bit 63 是方向位: client → server 为 0, server → client 为 1;
//! * bit 0..63 是该端自己的序列号。
//!
//! 由此得到三条性质:
//!
//! 1. 两个方向取自互不相交的计数器区间, 因此即使共享密钥, 客户端与服务端也不可能
//!    产生相同的 nonce。这正是方向切分要防的缺陷 —— 早先的实现让两端各自从 1 开始
//!    计数, 双方发出的第 N 个包 `(key, nonce)` 完全相同。
//! 2. 单个方向内序列号来自唯一一个原子计数器, 从 1 开始且只增不减; 序列空间耗尽时
//!    [`PacketCodec::encrypt`] 返回错误而不是回绕, 所以同一个值不会被用第二次。
//! 3. 接收方拒绝方向位等于自己发送方向的包, 因此攻击者无法把一个包原样反射回发送
//!    者来让它被接受。
//!
//! 前缀不参与唯一性, 只是一个域分隔符, 因此**不从密钥派生**: 早先的实现取
//! `key[0..4]` 当前缀, 把秘密材料混进一个只需要"不重复"的字段, 没有换来任何安全性。

use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, Key, KeyInit};
use aes_gcm::aes::cipher::typenum::U12;
use anyhow::{Result, anyhow, bail};
use generic_array::GenericArray;

// §16.6 加密密钥长度: AES-256。
pub const KEY_SIZE: usize = 32;

// §16.6 nonce 前缀长度: 4 字节 (会话内固定, 不上线)。
pub const NONCE_PREFIX_LEN: usize = 4;

// §16.6 nonce 计数器长度: 8 字节 (随包传输)。
pub const NONCE_COUNTER_LEN: usize = 8;

// §16.6 nonce 总长度: 12 字节 (GCM 标准)。
pub const NONCE_SIZE: usize = NONCE_PREFIX_LEN + NONCE_COUNTER_LEN;

// §16.6 GCM 认证标签长度。
pub const GCM_TAG_LEN: usize = 16;

// §16.6 每个数据报的固定线上开销: 计数器 + GCM tag。
pub const PACKET_OVERHEAD: usize = NONCE_COUNTER_LEN + GCM_TAG_LEN;

// §16.6 重放窗口大小 = 位掩码宽度。窗口取满 u128 的 128 位而不是更小的值, 是因为
// 分片会让同一条消息的多个包连续到达, 乱序深度天然比单包协议更大。
pub const REPLAY_WINDOW_SIZE: usize = 128;

// §16.6 会话内固定的 nonce 前缀。它只做域分隔, 不需要保密, 也不从密钥派生。
pub const DEFAULT_NONCE_PREFIX: [u8; NONCE_PREFIX_LEN] = *b"z3rm";

// §16.6 nonce 计数器的方向位。
const DIRECTION_BIT: u64 = 1 << 63;

// §16.6 nonce 计数器中留给序列号的位。
const SEQUENCE_MASK: u64 = DIRECTION_BIT - 1;

/// §16.6 单个方向可用的最大序列号。超过后必须重新协商会话密钥。
pub const MAX_SEQUENCE: u64 = SEQUENCE_MASK;

const _: () = assert!(REPLAY_WINDOW_SIZE == u128::BITS as usize);

/// §16.6 数据流方向。决定 nonce 计数器的高位, 从而把两端的 nonce 空间分开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    /// §16.6 该方向在 nonce 计数器中占据的方向位取值。
    pub fn nonce_bit(self) -> u64 {
        match self {
            Direction::ClientToServer => 0,
            Direction::ServerToClient => DIRECTION_BIT,
        }
    }

    /// §16.6 对端的发送方向, 即本端期望在收到的包上看到的方向。
    pub fn peer(self) -> Self {
        match self {
            Direction::ClientToServer => Direction::ServerToClient,
            Direction::ServerToClient => Direction::ClientToServer,
        }
    }
}

/// §16.6 重放保护滑动窗口。
///
/// `high_water` 是已接受的最大序列号, `bitmask` 的第 `n` 位表示序列号
/// `high_water - n` 是否已被接受 (第 0 位即 `high_water` 自身)。
pub struct PacketWindow {
    high_water: u64,
    bitmask: u128,
}

impl PacketWindow {
    pub fn new() -> Self {
        Self {
            high_water: 0,
            bitmask: 0,
        }
    }

    /// §16.6 只读判断: 该序列号是否还有可能被接受。
    ///
    /// 认证之前先做这一步, 可以让明显的重放不必付出一次解密的代价; 窗口本身不在这里
    /// 推进 —— 推进只发生在 [`PacketWindow::mark`], 即包通过认证之后 (RFC 4303 §3.4.3
    /// 的顺序)。否则攻击者只要伪造一个序列号极大的垃圾包, 就能把窗口顶到远处, 让之后
    /// 所有合法包统统落在窗口之外。
    pub fn is_acceptable(&self, sequence: u64) -> bool {
        if sequence == 0 {
            // 序列号从 1 开始, 0 永远不会被合法发送。
            return false;
        }
        if sequence > self.high_water {
            return true;
        }
        let offset = self.high_water - sequence;
        if offset >= REPLAY_WINDOW_SIZE as u64 {
            return false;
        }
        self.bitmask & (1u128 << offset) == 0
    }

    /// §16.6 把已通过认证的序列号记入窗口。返回 `false` 表示它其实是重放。
    pub fn mark(&mut self, sequence: u64) -> bool {
        if sequence == 0 {
            return false;
        }
        if sequence > self.high_water {
            let gap = sequence - self.high_water;
            // 跨度超过整个窗口在 UDP 上是常态 (连丢一批包即可)。此时窗口原先描述的
            // 序列号全部已经老到不可能再被接受, 直接清空并重新锚定即可; 早先的实现在
            // 这里直接拒绝且不推进 high_water, 于是一旦丢包超过窗口大小, 之后所有合法
            // 包都被永久拒绝, 连接静默死亡。
            self.bitmask = if gap >= REPLAY_WINDOW_SIZE as u64 {
                0
            } else {
                self.bitmask << gap
            };
            self.high_water = sequence;
            self.bitmask |= 1;
            true
        } else {
            let offset = self.high_water - sequence;
            if offset >= REPLAY_WINDOW_SIZE as u64 {
                return false;
            }
            let bit = 1u128 << offset;
            if self.bitmask & bit != 0 {
                return false;
            }
            self.bitmask |= bit;
            true
        }
    }

    /// §16.6 当前已接受的最大序列号。
    pub fn high_water(&self) -> u64 {
        self.high_water
    }
}

/// §16.6 AEAD 数据包编解码器。
///
/// 一个 codec 只服务一个方向: 它用 `send_direction` 加密, 只接受方向为
/// `send_direction.peer()` 的包。会话两端必须用**不同**的方向构造, 详见模块文档的
/// nonce 唯一性论证。
pub struct PacketCodec {
    cipher: Aes256Gcm,
    send_direction: Direction,
    /// §16.6 本端发送序列号, 从 1 开始只增不减。
    send_sequence: AtomicU64,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    recv_window: parking_lot::Mutex<PacketWindow>,
}

impl PacketCodec {
    /// §16.6 用给定的 32 字节会话密钥与发送方向创建 codec。
    pub fn new(key: [u8; KEY_SIZE], send_direction: Direction) -> Self {
        Self::new_with_prefix(key, send_direction, DEFAULT_NONCE_PREFIX)
    }

    /// §16.6 用给定的密钥、方向与 nonce 前缀创建 codec。
    ///
    /// 前缀只做域分隔 (例如让不同会话的 nonce 空间不重叠), 两端必须一致。
    pub fn new_with_prefix(
        key: [u8; KEY_SIZE],
        send_direction: Direction,
        nonce_prefix: [u8; NONCE_PREFIX_LEN],
    ) -> Self {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        Self {
            cipher,
            send_direction,
            send_sequence: AtomicU64::new(1),
            nonce_prefix,
            recv_window: parking_lot::Mutex::new(PacketWindow::new()),
        }
    }

    /// §16.6 本 codec 的发送方向。
    pub fn send_direction(&self) -> Direction {
        self.send_direction
    }

    /// §16.6 加密明文, 返回 `[nonce 计数器 (8)] [ciphertext || tag]`。
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        // 序列空间耗尽时停在上限而不是回绕: 回绕会让 (key, nonce) 重复。
        let sequence = self
            .send_sequence
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current <= MAX_SEQUENCE).then(|| current + 1)
            })
            .map_err(|_| {
                anyhow!("nonce sequence space exhausted; the session key must be rotated")
            })?;

        let counter = sequence | self.send_direction.nonce_bit();
        let counter_bytes = counter.to_be_bytes();

        let mut nonce_full = [0u8; NONCE_SIZE];
        nonce_full[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce_full[NONCE_PREFIX_LEN..].copy_from_slice(&counter_bytes);

        let nonce: GenericArray<u8, U12> = nonce_full.into();
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|error| anyhow!("aes-gcm encryption failed: {}", error))?;

        let mut packet = Vec::with_capacity(NONCE_COUNTER_LEN + ciphertext.len());
        packet.extend_from_slice(&counter_bytes);
        packet.extend_from_slice(&ciphertext);
        Ok(packet)
    }

    /// §16.6 解密数据包, 返回明文。方向不符、重放或认证失败都会返回错误。
    pub fn decrypt(&self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() < PACKET_OVERHEAD {
            bail!("packet too short for decryption: {} bytes", packet.len());
        }
        let (counter_bytes, ciphertext) = packet.split_at(NONCE_COUNTER_LEN);
        let counter = u64::from_be_bytes(
            counter_bytes
                .try_into()
                .map_err(|_| anyhow!("malformed nonce counter"))?,
        );

        // 方向位必须是对端的。等于自己的方向说明这个包是被反射回来的自家数据包。
        if counter & DIRECTION_BIT != self.send_direction.peer().nonce_bit() {
            bail!("packet direction mismatch; refusing reflected packet");
        }
        let sequence = counter & SEQUENCE_MASK;

        if !self.recv_window.lock().is_acceptable(sequence) {
            bail!("replay detected or nonce outside the replay window");
        }

        let mut nonce_full = [0u8; NONCE_SIZE];
        nonce_full[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce_full[NONCE_PREFIX_LEN..].copy_from_slice(counter_bytes);

        let nonce: GenericArray<u8, U12> = nonce_full.into();
        let plaintext = self
            .cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|error| anyhow!("decryption failed: {}", error))?;

        // 认证通过之后才推进窗口, 伪造包因此无法把合法包挤出窗口。
        if !self.recv_window.lock().mark(sequence) {
            bail!("replay detected while committing the replay window");
        }

        Ok(plaintext)
    }

    /// §16.6 接收窗口当前的最高序列号 (用于测试与诊断)。
    pub fn recv_high_water(&self) -> u64 {
        self.recv_window.lock().high_water()
    }
}
