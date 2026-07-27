//! §3.3 DEC-2026 Synchronized Output (BSU/ESU)
//!
//! 解析 DEC-2026 同步输出序列:
//! - BSU (Begin Synchronized Update): ESC 2 0 1 ~
//! - ESU (End Synchronized Update): ESC 2 0 2 ~
//!
//! 实现 100ms timeout: 如果收到 BSU 但未收到匹配的 ESU,
//! 超过 100ms 后强制刷新 generation bump。

use std::time::{Duration, Instant};

/// DEC-2026 状态机 (§3.3)
pub struct Dec2026Parser {
    /// 当前是否处于同步输出模式
    in_sync: bool,
    /// BSU 接收时间 (用于 100ms timeout)
    bsu_time: Option<Instant>,
}

impl Dec2026Parser {
    /// 创建 DEC-2026 解析器 (§3.3)
    pub fn new() -> Self {
        Self {
            in_sync: false,
            bsu_time: None,
        }
    }

    /// 解析字节序列，检测 BSU/ESU。
    /// 返回是否需要强制刷新 (unpaired BSU timeout)。
    pub fn parse(&mut self, bytes: &[u8]) -> bool {
        // ESC 2 0 1 ~ = BSU (Begin Synchronized Update)
        let bsu_seq = [0x1B, '2' as u8, '0' as u8, '1' as u8, '~' as u8];
        // ESC 2 0 2 ~ = ESU (End Synchronized Update)
        let esu_seq = [0x1B, '2' as u8, '0' as u8, '2' as u8, '~' as u8];

        let mut i = 0;
        while i + bsu_seq.len() <= bytes.len() {
            if bytes[i..].starts_with(&bsu_seq) {
                self.in_sync = true;
                self.bsu_time = Some(Instant::now());
                i += bsu_seq.len();
            } else if bytes[i..].starts_with(&esu_seq) {
                self.in_sync = false;
                self.bsu_time = None;
                i += esu_seq.len();
            } else {
                i += 1;
            }
        }

        // §3.3 100ms timeout: 检查 unpaired BSU
        if let Some(bsu_time) = self.bsu_time {
            if Instant::now().duration_since(bsu_time) > Duration::from_millis(100) {
                self.in_sync = false;
                self.bsu_time = None;
                return true; // 需要强制刷新
            }
        }

        false
    }

    /// §3.3 Wall-clock check for unpaired BSU without waiting for more PTY bytes.
    /// Call from the PTY read loop on poll timeout so a quiet terminal after BSU
    /// still force-flushes within ~100ms.
    pub fn check_timeout(&mut self) -> bool {
        if let Some(bsu_time) = self.bsu_time {
            if Instant::now().duration_since(bsu_time) > Duration::from_millis(100) {
                self.in_sync = false;
                self.bsu_time = None;
                return true;
            }
        }
        false
    }

    /// 获取当前同步状态
    pub fn is_in_sync(&self) -> bool {
        self.in_sync
    }

    /// 重置状态
    pub fn reset(&mut self) {
        self.in_sync = false;
        self.bsu_time = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bsu_esu_pair() {
        let mut parser = Dec2026Parser::new();

        // 发送 BSU
        let bsu = [0x1B, '2' as u8, '0' as u8, '1' as u8, '~' as u8];
        parser.parse(&bsu);
        assert!(parser.is_in_sync());

        // 发送 ESU
        let esu = [0x1B, '2' as u8, '0' as u8, '2' as u8, '~' as u8];
        parser.parse(&esu);
        assert!(!parser.is_in_sync());
    }

    #[test]
    fn test_bsu_without_esu() {
        let mut parser = Dec2026Parser::new();

        let bsu = [0x1B, '2' as u8, '0' as u8, '1' as u8, '~' as u8];
        parser.parse(&bsu);
        assert!(parser.is_in_sync());

        // 未发送 ESU, 不应该触发强制刷新 (时间未到)
        let force_flush = parser.parse(b"hello world");
        assert!(!force_flush);
        assert!(parser.is_in_sync());
    }

    #[test]
    fn test_bsu_timeout() {
        let mut parser = Dec2026Parser::new();

        let bsu = [0x1B, '2' as u8, '0' as u8, '1' as u8, '~' as u8];
        parser.parse(&bsu);
        assert!(parser.is_in_sync());

        // 模拟时间流逝 (手动设置)
        std::thread::sleep(Duration::from_millis(110));

        // 100ms 后应触发强制刷新
        let force_flush = parser.parse(b"test");
        assert!(force_flush);
        assert!(!parser.is_in_sync());
    }
}
