// tmux 风格的目标 specifier 解析
// 来源: spec §3.10 — 支持 session_name, session:window.pane, %N 格式
//
// 实现已移至 mux_protocol crate,让 CLI 与 server 共享 (避免漂移)。
// 此处仅 re-export,保持调用方兼容。

pub use mux_protocol::{Target, parse_target};
