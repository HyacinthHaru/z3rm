// §16.6 剪贴板模块 — 服务器端剪贴板中继。
// ServerClipboard 维护全局剪贴板状态，支持 text/image/file-path 内容类型，
// 携带 origin_host 元数据，通过 OSC 52 和 bracketed paste 集成 (§16.6)。

use base64::Engine;
use mux_protocol::proto::ClipboardEntry as ProtoClipboardEntry;
use mux_protocol::proto::clipboard_entry::ClipboardContentType as ProtoContentType;
use tokio::sync::mpsc;

/// §16.6 剪贴板内容类型
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClipboardContentType {
    /// 纯文本
    Text,
    /// PNG 图像
    ImagePng,
    /// 文件路径
    FilePath,
}

impl ClipboardContentType {
    /// 从 protobuf i32 值转换 (§16.6)
    pub fn from_proto_value(val: i32) -> Self {
        match val {
            1 => Self::Text,
            2 => Self::ImagePng,
            3 => Self::FilePath,
            _ => Self::Text,
        }
    }

    /// 转换为 protobuf i32 值 (§16.6)
    pub fn to_proto_value(&self) -> i32 {
        match self {
            Self::Text => ProtoContentType::Text as i32,
            Self::ImagePng => ProtoContentType::ImagePng as i32,
            Self::FilePath => ProtoContentType::FilePath as i32,
        }
    }
}

/// §16.6 剪贴板条目 — 内容类型 + 数据 + 来源主机
#[derive(Clone, Debug)]
pub struct ClipboardEntry {
    pub content_type: ClipboardContentType,
    pub data: Vec<u8>,
    /// 来源主机名 (origin_host)
    pub origin_host: String,
}

impl ClipboardEntry {
    /// 从 protobuf 消息转换 (§16.6)
    pub fn from_proto(entry: &ProtoClipboardEntry) -> Self {
        Self {
            content_type: ClipboardContentType::from_proto_value(entry.content_type),
            data: entry.data.clone(),
            origin_host: entry.origin_host.clone(),
        }
    }

    /// 转换为 protobuf 消息 (§16.6)
    pub fn to_proto(&self) -> ProtoClipboardEntry {
        ProtoClipboardEntry {
            content_type: self.content_type.to_proto_value(),
            data: self.data.clone(),
            origin_host: self.origin_host.clone(),
        }
    }

    /// 创建纯文本条目 (§16.6)
    pub fn text(text: &str, origin_host: String) -> Self {
        Self {
            content_type: ClipboardContentType::Text,
            data: text.as_bytes().to_vec(),
            origin_host,
        }
    }
}

/// §16.6 服务器剪贴板 — 全局剪贴板空间。
/// 仅在实际复制操作（OSC 52 / 显式 SetClipboard RPC）时更新。
/// 不监听客户端系统剪贴板变化 (no auto-mirror, §16.6 Task 5)。
pub struct ServerClipboard {
    /// 当前剪贴板内容
    current: parking_lot::RwLock<Option<ClipboardEntry>>,
}

impl ServerClipboard {
    /// 创建空剪贴板 (§16.6)
    pub fn new() -> Self {
        Self {
            current: parking_lot::RwLock::new(None),
        }
    }

    /// §16.6 设置剪贴板并通知所有客户端
    /// 仅实际复制触发同步 (only actual copy triggers sync)
    pub fn set_clipboard(
        &self,
        entry: ClipboardEntry,
        notification_txs: &[mpsc::UnboundedSender<mux_protocol::Envelope>],
    ) {
        {
            let mut current = self.current.write();
            *current = Some(entry);
        }
        // §16.6 推送 ClipboardChanged 到所有会话客户端 (at-least-once fan-out)。
        let notification = mux_protocol::Notification {
            event: Some(mux_protocol::notification::Event::ClipboardChanged(
                mux_protocol::ClipboardChanged {},
            )),
        };
        let envelope = mux_protocol::Envelope {
            version: Some(mux_protocol::PROTOCOL_VERSION.clone()),
            payload: Some(mux_protocol::envelope::Payload::Notification(notification)),
        };
        for tx in notification_txs {
            let _ = tx.send(envelope.clone());
        }
        tracing::debug!(clients = notification_txs.len(), "clipboard updated");
    }

    /// §16.6 获取当前剪贴板内容
    pub fn get_clipboard(&self) -> Option<ClipboardEntry> {
        self.current.read().clone()
    }

    /// §16.6 从 OSC 52 设置文本剪贴板
    /// OSC 52 格式: ESC ] 52 ; c ; <base64>; BEL
    pub fn set_from_osc52(
        &self,
        base64_data: &str,
        origin_host: String,
        notification_txs: &[mpsc::UnboundedSender<mux_protocol::Envelope>],
    ) -> anyhow::Result<()> {
        // §16.6 OSC 52 内容经过 base64 编码
        let data = base64::engine::general_purpose::STANDARD.decode(base64_data)?;
        let entry = ClipboardEntry {
            content_type: ClipboardContentType::Text,
            data,
            origin_host,
        };
        self.set_clipboard(entry, notification_txs);
        Ok(())
    }
}

impl Default for ServerClipboard {
    fn default() -> Self {
        Self::new()
    }
}

/// §16.6 OSC 52 序列解析器
/// 从终端输入流中检测并解析 OSC 52 剪贴板操作
pub struct Osc52Parser {
    /// 累积的 OSC 序列字节
    buffer: Vec<u8>,
    /// 解析状态
    state: Osc52State,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Osc52State {
    /// 初始状态: 等待 ESC (0x1B)
    Idle,
    /// 已收到 ESC, 等待 ']' (0x5D)
    Esc,
    /// 已进入 OSC, 收集参数直到 ';' 或 BEL/ST
    OscParam,
    /// 解析参数 "52", 收集后续 ';' 分隔的子参数
    Osc52,
}

impl Osc52Parser {
    /// 创建新的 OSC 52 解析器 (§16.6)
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            state: Osc52State::Idle,
        }
    }

    /// 处理输入字节，返回解析出的 OSC 52 内容 (如果完整)
    /// 返回 Some(content) 表示检测到完整的 OSC 52 设置操作
    /// 返回 None 表示尚未形成完整序列
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        for &byte in bytes {
            self.buffer.push(byte);
            match self.state {
                Osc52State::Idle => {
                    if byte == 0x1B {
                        self.state = Osc52State::Esc;
                    } else {
                        // 非 ESC 字节, 清空缓冲
                        self.buffer.clear();
                    }
                }
                Osc52State::Esc => {
                    if byte == 0x5D {
                        self.state = Osc52State::OscParam;
                    } else {
                        self.state = Osc52State::Idle;
                        self.buffer.clear();
                    }
                }
                Osc52State::OscParam => {
                    // 收集参数直到 ';'
                    if byte == b';' {
                        // 检查参数是否为 "52"
                        let param_len = self.buffer.len() - 3; // 减去 ESC ] ;
                        if param_len >= 2 && self.buffer[2..2 + param_len] == *b"52" {
                            // 匹配 OSC 52, 重置缓冲只保留参数部分
                            self.buffer = vec![0x1B, 0x5D, b'5', b'2', b';'];
                            self.state = Osc52State::Osc52;
                        } else {
                            // 非 OSC 52, 回到空闲
                            self.state = Osc52State::Idle;
                            self.buffer.clear();
                        }
                    }
                }
                Osc52State::Osc52 => {
                    // BEL (0x07) 或 ST (0x1B \) 终止 OSC 序列
                    if byte == 0x07 || byte == 0x1B {
                        // 提取 base64 内容
                        let content = self.extract_osc52_content()?;
                        self.buffer.clear();
                        self.state = Osc52State::Idle;
                        return Some(content);
                    }
                }
            }
        }
        None
    }

    /// 从缓冲区提取 OSC 52 的 base64 内容 (§16.6)
    fn extract_osc52_content(&self) -> Option<String> {
        // OSC 52 格式: ESC ] 52 ; c ; <base64> BEL/ST
        // buffer: ESC ] 52 ; c ; <base64> BEL/ST
        // 跳过 "ESC ] 52 ; " 前缀 (5 字节)
        if self.buffer.len() < 6 {
            return None;
        }
        // 找到第二个 ';' 之后的内容
        let start = 5; // 跳过 ESC ] 52 ;
        let remaining = &self.buffer[start..];
        // 跳过 'c' 和 ';' (clipboard 子命令)
        if remaining.len() < 2 || remaining[0] != b'c' {
            return None;
        }
        if remaining.len() < 3 || remaining[1] != b';' {
            return None;
        }
        // 提取 base64 部分 (去掉末尾的 BEL/ST)
        let base64_start = 2;
        let base64_end = remaining.len() - 1; // 去掉末尾 BEL/ESC
        if base64_end <= base64_start {
            return None;
        }
        String::from_utf8(remaining[base64_start..base64_end].to_vec()).ok()
    }
}

impl Default for Osc52Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// §16.6 括号粘贴包裹函数
/// 当 bracketed paste 模式激活时, 用 ESC [ 200 ~ ... ESC [ 201 ~ 包裹内容
pub fn wrap_bracketed_paste(text: &str, bracketed_paste_active: bool) -> String {
    if bracketed_paste_active {
        format!("\x1b[200~{}\x1b[201~", text)
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §16.6 测试 OSC 52 解析: BEL 终止
    #[test]
    fn test_osc52_bell_terminated() {
        let mut parser = Osc52Parser::new();
        // OSC 52 ; c ; SGVsbG8= BEL (base64 for "Hello")
        let bytes: Vec<u8> = vec![
            0x1B, 0x5D, b'5', b'2', b';', b'c', b';', b'S', b'G', b'V', b's', b'b', b'G', b'8',
            b'=', 0x07,
        ];
        let result = parser.feed(&bytes);
        assert!(result.is_some());
        let base64 = result.unwrap();
        assert_eq!(base64, "SGVsbG8="); // "Hello"
    }

    /// §16.6 测试 OSC 52 解析: ST 终止 (ESC \)
    #[test]
    fn test_osc52_st_terminated() {
        let mut parser = Osc52Parser::new();
        // OSC 52 ; c ; dGVzdA== ESC \
        let bytes: Vec<u8> = vec![
            0x1B, 0x5D, b'5', b'2', b';', b'c', b';', b'd', b'G', b'V', b'z', b'd', b'A', b'=',
            b'=', 0x1B, b'\\',
        ];
        let result = parser.feed(&bytes);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "dGVzdA==");
    }

    /// §16.6 测试非 OSC 52 序列被忽略
    #[test]
    fn test_non_osc52_ignored() {
        let mut parser = Osc52Parser::new();
        // OSC 12 (not 52)
        let bytes: Vec<u8> = vec![0x1B, 0x5D, b'1', b'2', b';', 0x07];
        let result = parser.feed(&bytes);
        assert!(result.is_none());
    }

    /// §16.6 测试流式解析: 分块输入
    #[test]
    fn test_osc52_streaming() {
        let mut parser = Osc52Parser::new();
        // 分块发送
        let chunk1: Vec<u8> = vec![0x1B, 0x5D, b'5', b'2'];
        let chunk2: Vec<u8> = vec![b';', b'c', b';', b'T', b'E'];
        let chunk3: Vec<u8> = vec![b'N', b'L', b'g', 0x07];

        assert!(parser.feed(&chunk1).is_none());
        assert!(parser.feed(&chunk2).is_none());
        let result = parser.feed(&chunk3);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "TENLg");
    }

    /// §16.6 测试括号粘贴包裹: 激活模式时添加标记
    #[test]
    fn test_bracketed_paste_wrapping() {
        let text = "hello world";
        let wrapped = wrap_bracketed_paste(text, true);
        let expected = format!("\x1b[200~{}\x1b[201~", text);
        assert_eq!(wrapped, expected);
    }

    #[test]
    fn test_bracketed_paste_no_wrapping() {
        let text = "hello world";
        let wrapped = wrap_bracketed_paste(text, false);
        assert_eq!(wrapped, text);
    }

    /// §16.6 测试 ServerClipboard set/get
    #[test]
    fn test_server_clipboard_set_get() {
        let clipboard = ServerClipboard::new();
        let (tx, _rx) = mpsc::unbounded_channel();

        assert!(clipboard.get_clipboard().is_none());

        clipboard.set_clipboard(
            ClipboardEntry::text("hello", "test-host".to_string()),
            &[tx],
        );

        let entry = clipboard.get_clipboard();
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.content_type, ClipboardContentType::Text);
        assert_eq!(entry.data, b"hello");
        assert_eq!(entry.origin_host, "test-host");
    }

    /// §16.6 测试多客户端无剪贴板污染
    /// 仅 SetClipboard 或 OSC 52 触发通知, 不监听系统剪贴板
    #[test]
    fn test_multi_client_no_pollution() {
        let clipboard = ServerClipboard::new();
        let (tx, mut rx) = mpsc::unbounded_channel();

        // 初始状态无通知
        clipboard.set_clipboard(
            ClipboardEntry::text("first", "host1".to_string()),
            &[tx.clone()],
        );
        // 确认收到通知
        let notification = rx.try_recv();
        assert!(notification.is_ok());
        let envelope = notification.unwrap();
        let notif = match envelope.payload {
            Some(mux_protocol::envelope::Payload::Notification(n)) => Some(n),
            _ => None,
        };
        assert!(matches!(
            notif.and_then(|n| n.event),
            Some(mux_protocol::notification::Event::ClipboardChanged(_))
        ));

        // 第二次更新
        clipboard.set_clipboard(ClipboardEntry::text("second", "host2".to_string()), &[tx]);
        let notification = rx.try_recv();
        assert!(notification.is_ok());

        // 验证当前值为最后一次设置
        let entry = clipboard.get_clipboard().unwrap();
        assert_eq!(entry.origin_host, "host2");
        assert_eq!(entry.data, b"second");
    }

    /// §16.6 测试内容类型转换
    #[test]
    fn test_content_type_roundtrip() {
        for (val, expected) in [
            (1, ClipboardContentType::Text),
            (2, ClipboardContentType::ImagePng),
            (3, ClipboardContentType::FilePath),
            (0, ClipboardContentType::Text),  // unspecified → text
            (99, ClipboardContentType::Text), // unknown → text
        ] {
            let ct = ClipboardContentType::from_proto_value(val);
            assert_eq!(ct, expected);
            assert_eq!(
                ct.to_proto_value(),
                match expected {
                    ClipboardContentType::Text => ProtoContentType::Text as i32,
                    ClipboardContentType::ImagePng => ProtoContentType::ImagePng as i32,
                    ClipboardContentType::FilePath => ProtoContentType::FilePath as i32,
                }
            );
        }
    }
}
