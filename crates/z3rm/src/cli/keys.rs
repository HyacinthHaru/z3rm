// tmux 兼容的按键名解析
// 来源: spec §3.10 — send-keys 接受 tmux 风格按键名
//
// 实现已移至 mux_protocol crate,让 CLI 与 server 共享同一翻译表 (避免漂移)。
// 此处仅 re-export,保持调用方兼容。

pub use mux_protocol::{parse_key, parse_keys};
