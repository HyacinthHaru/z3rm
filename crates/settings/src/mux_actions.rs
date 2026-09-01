//! §16.7 Mux keymap actions
//!
//! 多路复用器键位动作定义 (spec §16.7 Plan 17)
//! 用于 keymap profile 系统中的动作注册。

use gpui::{Action, actions};
use schemars::JsonSchema;
use serde::Deserialize;

// ============================================================================
// §16.7 mux 命名空间动作 (Mux session operations)
// ============================================================================

/// §15.7 Attach to the active mux session and reconcile its authoritative layout.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct Attach;

/// §16.7 断开当前 mux session 连接。
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct Detach;

/// §15.4 Reconnect the current mux transport and reconcile its authoritative snapshot.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct Reconnect;

/// §3.3 打开一个新窗口，attach 到当前 session (Plan 32)。
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct NewWindow;

/// §16.6 通过 SSH 地址打开一个远程会话窗口。
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct OpenRemote;

/// §16.7 关闭当前 mux session。
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct KillSession;

/// §16.7 关闭整个 mux_server 进程。
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct KillServer;

/// §16.7 进入 prefix 模式，等待下一个按键。
/// timeout_ms 控制 prefix 模式超时时间（默认 500ms）。
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux)]
pub struct EnterPrefixMode {
    #[serde(default = "default_prefix_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for EnterPrefixMode {
    fn default() -> Self {
        Self {
            timeout_ms: default_prefix_timeout_ms(),
        }
    }
}

fn default_prefix_timeout_ms() -> u64 {
    500
}

// ============================================================================
// §16.7 pane 命名空间动作 (Pane management)
// ============================================================================

actions!(
    mux_pane,
    [
        /// §16.7 新建标签页。
        NewTab,
        /// §16.7 关闭当前标签页。
        CloseTab,
        /// §16.7 切换到下一个标签页。
        NextTab,
        /// §16.7 切换到上一个标签页。
        PrevTab,
        /// §16.7 水平分割（右侧新建 pane）。
        SplitRight,
        /// §16.7 垂直分割（下方新建 pane）。
        SplitDown,
        /// §16.7 聚焦左侧 pane。
        FocusLeft,
        /// §16.7 聚焦右侧 pane。
        FocusRight,
        /// §16.7 聚焦上方 pane。
        FocusUp,
        /// §16.7 聚焦下方 pane。
        FocusDown,
        /// §16.7 切换到下一个 pane（循环）。
        FocusNextPane,
        /// §16.7 切换到上一个 pane（循环）。
        FocusPrevPane,
        /// §16.7 切换 pane 缩放状态。
        ZoomToggle,
        /// §16.7 向左调整 pane 大小。
        ResizeLeft,
        /// §16.7 向右调整 pane 大小。
        ResizeRight,
        /// §16.7 向上调整 pane 大小。
        ResizeUp,
        /// §16.7 向下调整 pane 大小。
        ResizeDown,
        /// §16.7 等分所有 pane 大小。
        ResizeEqual,
        /// §3.3 跳到上一条命令的提示符 (OSC 133)。
        JumpToPreviousPrompt,
        /// §3.3 跳到下一条命令的提示符 (OSC 133)。
        JumpToNextPrompt,
    ]
);

/// §16.7 按索引聚焦 pane。index 为 0-8。
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = mux_pane)]
pub struct FocusPaneIndex {
    #[serde(default)]
    pub index: u8,
}

impl Default for FocusPaneIndex {
    fn default() -> Self {
        Self { index: 0 }
    }
}

// 为 0-8 创建便捷动作，映射到 FocusPaneIndex
actions!(
    mux_pane,
    [
        /// §16.7 聚焦 pane 0。
        FocusPane0,
        /// §16.7 聚焦 pane 1。
        FocusPane1,
        /// §16.7 聚焦 pane 2。
        FocusPane2,
        /// §16.7 聚焦 pane 3。
        FocusPane3,
        /// §16.7 聚焦 pane 4。
        FocusPane4,
        /// §16.7 聚焦 pane 5。
        FocusPane5,
        /// §16.7 聚焦 pane 6。
        FocusPane6,
        /// §16.7 聚焦 pane 7。
        FocusPane7,
        /// §16.7 聚焦 pane 8。
        FocusPane8,
    ]
);

// ============================================================================
// §16.7 terminal 命名空间动作 (Terminal passthrough)
// ============================================================================

/// §16.7 向 PTY 发送字面按键（用于 prefix double-tap 转义）。
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendLiteral {
    #[serde(default)]
    pub keystroke: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn test_mux_connection_actions() {
        let _attach = Attach::default();
        let _detach = Detach::default();
        let _reconnect = Reconnect::default();
        let _new_window = NewWindow::default();
    }

    #[test]
    fn test_enter_prefix_mode_default_timeout() {
        let action = EnterPrefixMode::default();
        assert_eq!(action.timeout_ms, 500);
    }

    #[test]
    fn test_focus_pane_index_default() {
        let action = FocusPaneIndex::default();
        assert_eq!(action.index, 0);
    }

    #[test]
    fn test_pane_actions_exist() {
        let _ = NewTab::default();
        let _ = CloseTab::default();
        let _ = NextTab::default();
        let _ = PrevTab::default();
        let _ = SplitRight::default();
        let _ = SplitDown::default();
        let _ = FocusLeft::default();
        let _ = FocusRight::default();
        let _ = FocusUp::default();
        let _ = FocusDown::default();
        let _ = ZoomToggle::default();
        let _ = FocusPane0::default();
        let _ = FocusPane1::default();
    }
}
