// §16.7 输入路由优先级链 (spec §16.7, Plan 21)
//
// 实现已移至 mux_protocol crate (纯状态机,无 GPUI 依赖,可独立测试)。
// 此处仅 re-export,保持调用方兼容。

pub use mux_protocol::input::*;
