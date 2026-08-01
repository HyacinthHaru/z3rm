//! §16.6 AEAD per-packet encryption with replay protection.
//!
//! 每包 AEAD 加密: AES-256-GCM。
//! nonce = 12 字节: 4 字节前缀 (随机生成) + 8 字节递增计数器。
//! 重放保护: sliding window (默认窗口大小 64)。

use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, Key, KeyInit};
use aes_gcm::aes::cipher::typenum::U12;
use anyhow::Result;
use generic_array::GenericArray;

// §16.6 加密密钥长度: AES-256
pub const KEY_SIZE: usize = 32;

// §16.6 nonce 前缀长度: 4 字节 (固定随机值)
pub const NONCE_PREFIX_LEN: usize = 4;

// §16.6 nonce 计数器长度: 8 字节 (递增)
pub const NONCE_COUNTER_LEN: usize = 8;

// §16.6 nonce 总长度: 12 字节 (GCM 标准)
pub const NONCE_SIZE: usize = NONCE_PREFIX_LEN + NONCE_COUNTER_LEN;

// §16.6 重放窗口大小: 64 (与 mosh 默认一致)
pub const REPLAY_WINDOW_SIZE: usize = 64;

/// §16.6 重放保护滑动窗口。
///
/// 使用位掩码跟踪最近接收的 nonce 计数器值。
/// 拒绝重复或过于滞后的数据包。
pub struct PacketWindow {
    /// 已确认的最高 nonce 计数器值。
    high_water: u64,
    /// 位掩码: 最近 REPLAY_WINDOW_SIZE 个 nonce 是否已接收。
    bitmask: u128,
}

impl PacketWindow {
    pub fn new() -> Self {
        Self {
            high_water: 0,
            bitmask: 0,
        }
    }

    /// 检查 nonce 计数器是否有效 (非重复、非过时)。
    /// 返回 `true` 表示该 nonce 是合法的，同时标记为已接收。
    pub fn check_and_mark(&mut self, nonce_val: u64) -> bool {
        // §16.6 如果 nonce 大于最高水位，更新窗口。
        if nonce_val > self.high_water {
            let gap = (nonce_val - self.high_water) as u32;
            if gap > REPLAY_WINDOW_SIZE as u32 {
                // 间隔太大，可能是攻击或时钟回退。
                return false;
            }
            // 移动窗口: 将 bitmask 左移 gap 位。
            self.bitmask <<= gap;
            self.high_water = nonce_val;
            // 标记当前 nonce 为已接收。
            self.bitmask |= 1;
            true
        } else {
            // §16.6 nonce 小于等于最高水位，检查是否在窗口内。
            let offset = (self.high_water - nonce_val) as u32;
            if offset == 0 || offset > REPLAY_WINDOW_SIZE as u32 {
                return false;
            }
            let bit = 1u128 << offset;
            // 如果该位已设置，说明是重复。
            if self.bitmask & bit != 0 {
                return false;
            }
            // 标记为已接收。
            self.bitmask |= bit;
            true
        }
    }
}

/// §16.6 AEAD 数据包编解码器。
///
/// 加密格式: `[nonce_counter (8)] [ciphertext + GCM tag]`。
/// nonce 前缀在构造时随机生成，对会话全程固定。
pub struct PacketCodec {
    cipher: Aes256Gcm,
    /// §16.6 发送 nonce 计数器 (递增)。
    send_nonce: AtomicU64,
    /// §16.6 nonce 前缀 (4 字节随机值)。
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    /// §16.6 接收端重放保护窗口。
    recv_window: parking_lot::Mutex<PacketWindow>,
}

impl PacketCodec {
    /// §16.6 创建新的 PacketCodec，使用给定的 32 字节密钥。
    pub fn new(key: [u8; KEY_SIZE]) -> Self {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        // §16.6 nonce 前缀从会话密钥派生 (前 4 字节)。
        let nonce_prefix = [key[0], key[1], key[2], key[3]];
        Self {
            cipher,
            send_nonce: AtomicU64::new(1),
            nonce_prefix,
            recv_window: parking_lot::Mutex::new(PacketWindow::new()),
        }
    }

    /// §16.6 创建新的 PacketCodec，使用给定的密钥和 nonce 前缀。
    /// 用于测试或双方协商好 nonce 前缀的场景。
    pub fn new_with_prefix(key: [u8; KEY_SIZE], nonce_prefix: [u8; NONCE_PREFIX_LEN]) -> Self {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        Self {
            cipher,
            send_nonce: AtomicU64::new(1),
            nonce_prefix,
            recv_window: parking_lot::Mutex::new(PacketWindow::new()),
        }
    }

    /// §16.6 加密明文数据，返回 `[nonce_counter (8)] [ciphertext]`。
    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce_val = self.send_nonce.fetch_add(1, Ordering::SeqCst);
        let nonce_bytes = nonce_val.to_be_bytes();

        // §16.6 构建 12 字节 nonce: 前缀 + 计数器。
        let mut nonce_full = [0u8; NONCE_SIZE];
        nonce_full[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce_full[NONCE_PREFIX_LEN..].copy_from_slice(&nonce_bytes);

        let nonce: GenericArray<u8, U12> = nonce_full.into();
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("aes-gcm encrypt failed");

        // §16.6 数据包格式: nonce_counter (8) + ciphertext。
        let mut packet = Vec::with_capacity(NONCE_COUNTER_LEN + ciphertext.len());
        packet.extend_from_slice(&nonce_bytes);
        packet.extend_from_slice(&ciphertext);
        packet
    }

    /// §16.6 解密数据包，返回明文。检查重放保护窗口。
    pub fn decrypt(&self, packet: &[u8]) -> Result<Vec<u8>> {
        // §16.6 数据包必须至少包含 nonce (8) + GCM tag (16)。
        if packet.len() < NONCE_COUNTER_LEN + 16 {
            anyhow::bail!("packet too short for decryption");
        }

        // 提取 nonce 计数器值。
        let nonce_counter = &packet[..NONCE_COUNTER_LEN];
        let nonce_val = u64::from_be_bytes(
            nonce_counter
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid nonce"))?,
        );

        // §16.6 检查重放保护窗口。
        if !self.recv_window.lock().check_and_mark(nonce_val) {
            anyhow::bail!("replay detected or nonce out of window");
        }

        // §16.6 构建完整 nonce 用于解密。
        let mut nonce_full = [0u8; NONCE_SIZE];
        nonce_full[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce_full[NONCE_PREFIX_LEN..].copy_from_slice(nonce_counter);

        let nonce: GenericArray<u8, U12> = nonce_full.into();
        let ciphertext = &packet[NONCE_COUNTER_LEN..];
        let plaintext = self
            .cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("decryption failed: {}", e))?;

        Ok(plaintext)
    }
}
