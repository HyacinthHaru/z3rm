//! # mux_protocol
//!
//! z3rm 客户端与 mux_server 之间的 prost/protobuf 有线协议。
//! 协议版本化（§3.10），基于长度前缀的二进制帧（§9），
//! 覆盖会话生命周期、Pane 生命周期、网格同步、滚动缓冲、文件读取、
//! 剪贴板中继以及扩展 Chrome RPC（§16）。

use prost::Message;

// §9 由 prost-build 生成的 protobuf 类型，命名空间为 z3rm.mux。
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/z3rm.mux.rs"));
}

pub mod input;
pub use proto::*;

// §3.10 当前协议版本：major 用于破坏性变更，minor 用于新增字段。
pub const PROTOCOL_VERSION: proto::ProtocolVersion = proto::ProtocolVersion { major: 1, minor: 1 };

/// Stable bits used by `FullGridSnapshot.modes`; these do not mirror any
/// terminal emulator's private representation.
pub mod terminal_mode {
    pub const APP_CURSOR: u32 = 1 << 0;
    pub const APP_KEYPAD: u32 = 1 << 1;
    pub const SHOW_CURSOR: u32 = 1 << 2;
    pub const LINE_WRAP: u32 = 1 << 3;
    pub const ORIGIN: u32 = 1 << 4;
    pub const INSERT: u32 = 1 << 5;
    pub const LINE_FEED_NEW_LINE: u32 = 1 << 6;
    pub const FOCUS_IN_OUT: u32 = 1 << 7;
    pub const ALTERNATE_SCROLL: u32 = 1 << 8;
    pub const BRACKETED_PASTE: u32 = 1 << 9;
    pub const SGR_MOUSE: u32 = 1 << 10;
    pub const UTF8_MOUSE: u32 = 1 << 11;
    pub const ALT_SCREEN: u32 = 1 << 12;
    pub const MOUSE_REPORT_CLICK: u32 = 1 << 13;
    pub const MOUSE_DRAG: u32 = 1 << 14;
    pub const MOUSE_MOTION: u32 = 1 << 15;
    pub const VI: u32 = 1 << 16;
    pub const KNOWN: u32 = (1 << 17) - 1;
}

// §9 将 Envelope 编码为长度前缀二进制帧：| varint len | protobuf bytes |。
/// Frame a message as length-prefixed binary.
pub fn frame(msg: &Envelope) -> Result<Vec<u8>, prost::EncodeError> {
    let mut buf = Vec::with_capacity(msg.encoded_len() + 4);
    msg.encode_length_delimited(&mut buf)?;
    Ok(buf)
}

// §9 从长度前缀二进制帧解码 Envelope，返回 (消息, 已消费字节数)。
/// Decode a framed message. Returns (message, bytes_consumed).
pub fn unframe(buf: &[u8]) -> Result<(Envelope, usize), prost::DecodeError> {
    let mut rest: &[u8] = buf;
    let msg = Envelope::decode_length_delimited(&mut rest)?;
    let consumed = buf.len() - rest.len();
    Ok((msg, consumed))
}

// ============================================================================
// §9 长度前缀帧的边界保护 (wire hardening)
// ============================================================================
//
// 客户端 (mux) 与服务端 (mux_server) 的帧读取器都从线上读取 varint 长度前缀,
// 再据此分配缓冲区。若前缀由攻击者控制 (例如声明 len = u64::MAX), 读取器在
// 分配前若不做边界检查, 会被诱导申请近 2^63 字节内存。
//
// 以下常量与函数是两端共享的唯一真相来源:
// - MAX_VARINT_LEN:    varint(u64) 最多 10 字节, 超过即为 overlong/截断。
// - MAX_FRAME_PAYLOAD: 单帧 payload (varint 之后的 protobuf 字节) 上限,
//                      大于该值在分配前即被拒绝。
//                      64 MiB 远大于任何合理的 Envelope (含回滚/文件块/pane
//                      输出), 但又足以挡住攻击者构造的巨型前缀。
//
// 读者必须: 先严格校验前缀, 再用校验后的长度作为分配上限; 任何 EOF mid-frame
// 或畸形前缀返回错误, 不得退化为 "无数据" 的成功语义。

/// varint(u64) 的最大编码长度 (字节)。超过即 overlong 畸形前缀。
pub const MAX_VARINT_LEN: usize = 10;

/// §9 单帧 payload 长度上限 (64 MiB)。读取器在分配或扩容前据此拒绝越界前缀。
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024 * 1024;

pub const MAX_GRID_COLUMNS: usize = 4_096;
pub const MAX_GRID_ROWS: usize = 4_096;
pub const MAX_GRID_CELLS: usize = 1_048_576;

pub fn checked_grid_cell_count(cols: usize, rows: usize) -> Result<usize, &'static str> {
    if cols == 0 || rows == 0 {
        return Err("grid dimensions must be nonzero");
    }
    if cols > MAX_GRID_COLUMNS || rows > MAX_GRID_ROWS {
        return Err("grid dimensions exceed protocol limits");
    }
    let cells = cols.checked_mul(rows).ok_or("grid dimensions overflow")?;
    if cells > MAX_GRID_CELLS {
        return Err("grid cell count exceeds protocol limit");
    }
    Ok(cells)
}

/// §9 长度前缀帧的解析错误: overlong 前缀、溢出、或超过 payload 上限。
///
/// 读取器必须把它向上传播为错误, 不得静默当作 "无数据" 的成功返回。
#[derive(Debug)]
pub struct FrameLengthError(pub FrameLengthErrorKind);

/// §9 帧长度错误类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLengthErrorKind {
    /// varint 前缀字节数超过 `MAX_VARINT_LEN` 仍未终止 (overlong 攻击)。
    OverlongPrefix,
    /// varint 编码的长度超过 `usize` 位数 (32-bit 平台上的 u64::MAX 等)。
    LengthOverflow,
    /// varint 编码的长度超过 `MAX_FRAME_PAYLOAD`。
    PayloadTooLarge { len: usize },
}

impl std::fmt::Display for FrameLengthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            FrameLengthErrorKind::OverlongPrefix => {
                write!(f, "frame length varint exceeds {} bytes", MAX_VARINT_LEN)
            }
            FrameLengthErrorKind::LengthOverflow => {
                write!(f, "frame length prefix overflows usize")
            }
            FrameLengthErrorKind::PayloadTooLarge { len } => {
                write!(
                    f,
                    "frame length {} exceeds max payload {}",
                    len, MAX_FRAME_PAYLOAD
                )
            }
        }
    }
}

impl std::error::Error for FrameLengthError {}

impl From<FrameLengthError> for std::io::Error {
    fn from(e: FrameLengthError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

/// §9 校验从线上读出的 varint 长度, 返回可用于分配的 `usize`。
///
/// 在分配或扩容缓冲区之前调用, 统一拒绝:
/// - 超过 `usize` 位数 (溢出);
/// - 超过 `MAX_FRAME_PAYLOAD` (越界 payload)。
///
/// ```ignore
/// let len = mux_protocol::check_frame_len(raw_len)?;
/// let mut buf = vec![0u8; len];
/// ```
pub fn check_frame_len(len: u64) -> Result<usize, FrameLengthError> {
    let len_usize =
        usize::try_from(len).map_err(|_| FrameLengthError(FrameLengthErrorKind::LengthOverflow))?;
    if len_usize > MAX_FRAME_PAYLOAD {
        return Err(FrameLengthError(FrameLengthErrorKind::PayloadTooLarge {
            len: len_usize,
        }));
    }
    Ok(len_usize)
}

/// §9 从已缓冲的字节解析 varint 长度前缀。
///
/// 返回:
/// - `Ok(Some((len, header_len)))`  前缀完整且合法, 已消费 `header_len` 字节;
/// - `Ok(None)`                     已缓冲字节不足以判定 (调用方应继续读取);
/// - `Err(OverlongPrefix)`          varint 持续未终止, 字节数已达
///                                  `MAX_VARINT_LEN` (overlong 攻击)。
///
/// 本函数只做前缀终止与 overlong 校验, **不**校验长度上限 — 调用方在分配前
/// 仍须用 `check_frame_len` 做溢出 / payload 上限检查, 以便错误类别精确。
pub fn parse_len_prefix(buf: &[u8]) -> Result<Option<(u64, usize)>, FrameLengthError> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in buf.iter().enumerate() {
        // shift 增长先于终止判断: 第 i 个字节贡献 bits [7i, 7i+7)。
        // 当已处理满 MAX_VARINT_LEN 个字节仍未遇到终止位, 视为 overlong。
        // 注意 protobuf 中完整的 u64 varint 恰为 10 字节, 但第 10 字节 (shift=63)
        // 只允许低 1 位有效; 这里宽松地接受 "已终止" 的第 10 字节, 仅拒绝
        // 第 10 字节仍带 continuation 的情形 (即真正 overlong)。
        if byte & 0x80 == 0 {
            if i + 1 == MAX_VARINT_LEN && byte > 1 {
                return Err(FrameLengthError(FrameLengthErrorKind::LengthOverflow));
            }
            value |= (byte as u64) << shift;
            return Ok(Some((value, i + 1)));
        }
        value |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        // 下一个字节会让 shift 达到或超过 7*MAX_VARINT_LEN, 必然 overlong。
        if i + 1 >= MAX_VARINT_LEN {
            return Err(FrameLengthError(FrameLengthErrorKind::OverlongPrefix));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod frame_length_tests {
    use super::*;

    #[test]
    fn parse_len_prefix_waits_for_incomplete_prefix() {
        assert!(parse_len_prefix(&[0x80]).expect("parse prefix").is_none());
    }

    #[test]
    fn parse_len_prefix_rejects_overlong_prefix() {
        let bytes = [0x80; MAX_VARINT_LEN];
        let error = parse_len_prefix(&bytes).expect_err("overlong prefix must fail");
        assert_eq!(error.0, FrameLengthErrorKind::OverlongPrefix);
    }

    #[test]
    fn parse_len_prefix_rejects_u64_overflow_prefix() {
        let mut bytes = [0xff; MAX_VARINT_LEN];
        bytes[MAX_VARINT_LEN - 1] = 0x02;
        let error = parse_len_prefix(&bytes).expect_err("overflow prefix must fail");
        assert_eq!(error.0, FrameLengthErrorKind::LengthOverflow);
    }

    #[test]
    fn check_frame_len_rejects_oversized_payload() {
        let error = check_frame_len((MAX_FRAME_PAYLOAD as u64) + 1)
            .expect_err("oversized payload must fail");
        assert_eq!(
            error.0,
            FrameLengthErrorKind::PayloadTooLarge {
                len: MAX_FRAME_PAYLOAD + 1,
            }
        );
    }
}

// ============================================================================
// §3.10 tmux 兼容按键名解析 (CLI 协议契约)
// ============================================================================
//
// send-keys 命令接受 tmux 风格按键名。这是 wire protocol 的一部分:
// z3rm CLI 把字符串 "Enter"/"C-c"/"Up" 翻译成字节,通过 SendInputRequest
// 发到 server。放在 mux_protocol 里让 CLI 与 server 共享同一翻译表,
// 避免漂移。原本在 crates/z3rm/src/cli/keys.rs,但 z3rm bin test 链
// 经过 editor (有 broken refs),导致这些纯函数测试无法运行。

/// 将 tmux 风格的按键名转换为字节序列。
///
/// 支持的格式:
/// - 命名按键: `Enter`, `Tab`, `BSpace`, `Escape`, `Space`, `Up`, `Down`,
///   `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`
/// - Ctrl 组合: `C-a` through `C-z` (以及 `C-A` through `C-Z`)
/// - Alt 组合: `M-a` through `M-z` (以及 `M-A` through `M-Z`)
/// - 字面文本: 其他字符串直接作为 UTF-8 字节
pub fn parse_key(name: &str) -> Vec<u8> {
    match name {
        "Enter" | "Return" => b"\r".to_vec(),
        "Tab" => b"\t".to_vec(),
        "BSpace" => b"\x7f".to_vec(),
        "Escape" => b"\x1b".to_vec(),
        "Space" => b" ".to_vec(),
        "Up" => b"\x1b[A".to_vec(),
        "Down" => b"\x1b[B".to_vec(),
        "Right" => b"\x1b[C".to_vec(),
        "Left" => b"\x1b[D".to_vec(),
        "Home" => b"\x1b[H".to_vec(),
        "End" => b"\x1b[F".to_vec(),
        "PageUp" => b"\x1b[5~".to_vec(),
        "PageDown" => b"\x1b[6~".to_vec(),
        // C-c → Ctrl+C = 0x03
        s if s.starts_with("C-") && s.len() == 3 => {
            // Ctrl+X: mask to control range (0x00–0x1F). This correctly
            // handles C-[ (ESC), C-@ (NUL), C-/ (FS), C-\\ (FS), etc.
            vec![s.as_bytes()[2] & 0x1f]
        }
        // M-x → Alt+X: ESC followed by x
        s if s.starts_with("M-") && s.len() == 3 => {
            vec![0x1b, s.as_bytes()[2]]
        }
        // 字面文本
        _ => name.as_bytes().to_vec(),
    }
}

/// 解析多个按键名,返回合并的字节序列。
pub fn parse_keys(names: &[String]) -> Vec<u8> {
    let mut result = Vec::new();
    for name in names {
        result.extend(parse_key(name));
    }
    result
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn parse_enter() {
        assert_eq!(parse_key("Enter"), b"\r");
        assert_eq!(parse_key("Return"), b"\r");
    }

    #[test]
    fn parse_tab() {
        assert_eq!(parse_key("Tab"), b"\t");
    }

    #[test]
    fn parse_backspace() {
        assert_eq!(parse_key("BSpace"), b"\x7f");
    }

    #[test]
    fn parse_escape() {
        assert_eq!(parse_key("Escape"), b"\x1b");
    }

    #[test]
    fn parse_space() {
        assert_eq!(parse_key("Space"), b" ");
    }

    #[test]
    fn parse_arrow_keys() {
        assert_eq!(parse_key("Up"), b"\x1b[A");
        assert_eq!(parse_key("Down"), b"\x1b[B");
        assert_eq!(parse_key("Right"), b"\x1b[C");
        assert_eq!(parse_key("Left"), b"\x1b[D");
    }

    #[test]
    fn parse_home_end() {
        assert_eq!(parse_key("Home"), b"\x1b[H");
        assert_eq!(parse_key("End"), b"\x1b[F");
    }

    #[test]
    fn parse_page_keys() {
        assert_eq!(parse_key("PageUp"), b"\x1b[5~");
        assert_eq!(parse_key("PageDown"), b"\x1b[6~");
    }

    #[test]
    fn parse_ctrl_keys() {
        // Ctrl+A = 0x01, Ctrl+B = 0x02, Ctrl+C = 0x03
        assert_eq!(parse_key("C-a"), vec![1]);
        assert_eq!(parse_key("C-b"), vec![2]);
        assert_eq!(parse_key("C-c"), vec![3]);
        assert_eq!(parse_key("C-d"), vec![4]);
        // 大写也有效
        assert_eq!(parse_key("C-A"), vec![1]);
        assert_eq!(parse_key("C-Z"), vec![26]);
    }

    #[test]
    fn parse_ctrl_punctuation() {
        assert_eq!(parse_key("C-["), vec![0x1b]); // ESC
        assert_eq!(parse_key("C-@"), vec![0x00]); // NUL
        assert_eq!(parse_key("C-\\"), vec![0x1c]); // FS
        assert_eq!(parse_key("C-]"), vec![0x1d]); // GS
        assert_eq!(parse_key("C-^"), vec![0x1e]); // RS
        assert_eq!(parse_key("C-_"), vec![0x1f]); // US
    }

    #[test]
    fn parse_meta_keys() {
        // M-a = ESC + 'a'
        assert_eq!(parse_key("M-a"), vec![0x1b, b'a']);
        assert_eq!(parse_key("M-x"), vec![0x1b, b'x']);
    }

    #[test]
    fn parse_literal() {
        assert_eq!(parse_key("hello"), b"hello");
        assert_eq!(parse_key("foo bar"), b"foo bar");
    }

    #[test]
    fn parse_keys_multiple() {
        let keys = vec![
            "echo".to_string(),
            " ".to_string(),
            "hello".to_string(),
            "Enter".to_string(),
        ];
        let bytes = parse_keys(&keys);
        assert_eq!(bytes, b"echo hello\r");
    }
}

#[cfg(test)]
mod grid_limit_tests {
    use super::*;

    #[test]
    fn decoded_grid_limits_reject_oversized_dimensions_and_products() {
        assert_eq!(checked_grid_cell_count(80, 24), Ok(1_920));
        assert!(checked_grid_cell_count(0, 24).is_err());
        assert!(checked_grid_cell_count(MAX_GRID_COLUMNS + 1, 1).is_err());
        assert!(checked_grid_cell_count(2_048, 2_048).is_err());
    }
}
// ============================================================================
// §3.10 tmux 风格目标 specifier 解析 (CLI 协议契约)
// ============================================================================
//
// send-keys / split-window / capture-pane 等命令的 -t 参数格式。
// 同 keys.rs,放在 protocol 层让 CLI 与 server 共享。

/// 目标类型: session、pane 索引、session:window.pane、当前焦点
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// 按名称指定 session: `z3rm send-keys -t mysession`
    Session(String),
    /// 按 session:window.pane 指定 pane: `z3rm send-keys -t mysession:0.1`
    /// window = tab index, pane = pane index within that tab
    PaneInSession {
        session: String,
        window: u32,
        pane: u32,
    },
    /// 按 pane 全局索引: `z3rm send-keys -t %3`
    PaneByIndex(u32),
    /// 未指定 target, 使用当前 focused pane
    Current,
}

/// 解析 tmux 风格的目标字符串。
///
/// 支持格式:
/// - `None` → Current
/// - `%N` → PaneByIndex(N)
/// - `session:W.P` → PaneInSession { session, window: W, pane: P }
/// - `session` → Session(session)
pub fn parse_target(s: &Option<String>) -> Target {
    match s {
        None => Target::Current,
        Some(s) if s.starts_with('%') => Target::PaneByIndex(s[1..].parse().unwrap_or(0)),
        Some(s) if s.contains(':') && s.contains('.') => {
            let parts: Vec<&str> = s.splitn(3, |c| c == ':' || c == '.').collect();
            Target::PaneInSession {
                session: parts[0].to_string(),
                window: parts[1].parse().unwrap_or(0),
                pane: parts[2].parse().unwrap_or(0),
            }
        }
        Some(s) => Target::Session(s.clone()),
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn parse_none() {
        let target = parse_target(&None);
        assert!(matches!(target, Target::Current));
    }

    #[test]
    fn parse_session_name() {
        let target = parse_target(&Some("mysession".to_string()));
        assert_eq!(target, Target::Session("mysession".to_string()));
    }

    #[test]
    fn parse_pane_index() {
        let target = parse_target(&Some("%3".to_string()));
        assert_eq!(target, Target::PaneByIndex(3));
    }

    #[test]
    fn parse_session_window_pane() {
        let target = parse_target(&Some("dev:0.1".to_string()));
        assert_eq!(
            target,
            Target::PaneInSession {
                session: "dev".to_string(),
                window: 0,
                pane: 1,
            }
        );
    }

    #[test]
    fn parse_session_window_pane_multi() {
        let target = parse_target(&Some("prod:2.5".to_string()));
        assert_eq!(
            target,
            Target::PaneInSession {
                session: "prod".to_string(),
                window: 2,
                pane: 5,
            }
        );
    }

    #[test]
    fn parse_pane_index_zero() {
        let target = parse_target(&Some("%0".to_string()));
        assert_eq!(target, Target::PaneByIndex(0));
    }
}
