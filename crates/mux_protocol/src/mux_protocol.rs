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
pub const PROTOCOL_VERSION: proto::ProtocolVersion = proto::ProtocolVersion {
    major: 1,
    minor: 0,
};

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
            let c = s.as_bytes()[2].to_ascii_lowercase();
            vec![c.wrapping_sub(b'a').wrapping_add(1)]
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
