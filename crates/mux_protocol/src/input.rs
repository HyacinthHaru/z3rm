// §16.7 输入路由优先级链 (spec §16.7, Plan 21)
//
// 输入优先级: IME → 扩展 → prefix mode → Agent CLI → 全屏应用 → copy mode → 终端应用
//
// 实现 prefix mode 状态机，支持 tmux/screen 风格的 prefix key → 命令键模式。
// 全屏应用检测 (alt screen / bracketed paste / mouse tracking) 触发 passthrough。

use std::time::Duration;

/// §16.7 Prefix mode 状态机状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixModeState {
    /// 正常模式：所有按键直接传递给 PTY，除非匹配 mux keymap
    Normal,
    /// Prefix 模式：已按下 prefix key，等待下一个键 (带超时)
    PrefixWait,
}

/// §16.7 Prefix mode 配置
///
/// 由 keymap profile 定义。tmux 使用 Ctrl-b，screen 使用 Ctrl-a。
#[derive(Debug, Clone)]
pub struct PrefixModeConfig {
    /// Prefix key 超时时间 (毫秒)。超时后退出 prefix mode，按键透传到 PTY。
    pub timeout_ms: u64,
    /// 当前是否处于全屏应用模式 (alt screen / bracketed paste / mouse tracking)。
    /// 全屏模式下 prefix key 透传到 PTY，不触发 prefix mode。
    pub full_screen_passthrough: bool,
}

impl Default for PrefixModeConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 500,
            full_screen_passthrough: false,
        }
    }
}

/// §16.7 Prefix mode 状态机
///
/// 状态转换:
/// Normal → (按下 prefix key) → PrefixWait
/// PrefixWait → (匹配 prefix binding) → Normal (执行命令)
/// PrefixWait → (超时) → Normal (按键透传)
/// PrefixWait → (按下 prefix key) → Normal (发送 literal prefix key 到 PTY)
/// PrefixWait → (不匹配按键) → Normal (按键透传)
#[derive(Debug, Clone)]
pub struct PrefixModeMachine {
    state: PrefixModeState,
    config: PrefixModeConfig,
}

impl PrefixModeMachine {
    /// §16.7 创建新的 prefix mode 状态机
    pub fn new(config: PrefixModeConfig) -> Self {
        Self {
            state: PrefixModeState::Normal,
            config,
        }
    }

    /// §16.7 获取当前状态
    pub fn state(&self) -> PrefixModeState {
        self.state
    }

    /// §16.7 检查是否处于 prefix wait 状态
    pub fn is_prefix_wait(&self) -> bool {
        self.state == PrefixModeState::PrefixWait
    }

    /// §16.7 更新全屏应用 passthrough 状态
    ///
    /// 当检测到终端应用启用 alt screen / bracketed paste / mouse tracking 时，
    /// 设置 `full_screen_passthrough = true`，prefix key 将透传到 PTY。
    pub fn set_full_screen_passthrough(&mut self, passthrough: bool) {
        self.config.full_screen_passthrough = passthrough;
    }

    /// §16.7 处理 prefix key 按下
    ///
    /// 返回 `PrefixAction`:
    /// - `Passthrough` — 全屏应用模式下，prefix key 透传到 PTY
    /// - `EnterPrefixMode` — 进入 prefix wait 状态
    pub fn on_prefix_key(&mut self) -> PrefixAction {
        if self.config.full_screen_passthrough {
            // §16.7 全屏应用 passthrough: prefix key 直接透传到 PTY
            PrefixAction::Passthrough
        } else {
            // §16.7 进入 prefix mode
            self.state = PrefixModeState::PrefixWait;
            PrefixAction::EnterPrefixMode
        }
    }

    /// §16.7 处理 prefix mode 下的按键
    ///
    /// 参数:
    /// - `key` — 按下的键
    /// - `is_prefix_key` — 是否为 prefix key 本身 (double-tap 检测)
    /// - `binding_match` — 是否有匹配的 prefix binding
    ///
    /// 返回 `PrefixAction`:
    /// - `ExecuteCommand` — 匹配到 binding，执行对应命令
    /// - `Passthrough` — 不匹配或 double-tap，按键透传到 PTY
    /// - `DoubleTapLiteral` — 按下 prefix key 本身，发送 literal prefix key 到 PTY
    pub fn on_prefix_wait_key(&mut self, is_prefix_key: bool, binding_match: bool) -> PrefixAction {
        let action = if is_prefix_key {
            // §16.7 Double-tap: 按下 prefix key 本身 → 发送 literal 到 PTY
            PrefixAction::DoubleTapLiteral
        } else if binding_match {
            // §16.7 匹配到 prefix binding → 执行命令
            PrefixAction::ExecuteCommand
        } else {
            // §16.7 不匹配 → 按键透传到 PTY
            PrefixAction::Passthrough
        };

        // 处理完成后退出 prefix mode
        self.state = PrefixModeState::Normal;
        action
    }

    /// §16.7 超时: 退出 prefix mode
    ///
    /// 调用方在 prefix wait 超时后调用此方法。
    /// 超时后的按键应透传到 PTY。
    pub fn on_timeout(&mut self) {
        self.state = PrefixModeState::Normal;
    }

    /// §16.7 强制重置到 normal 状态
    ///
    /// 在 profile 切换或 session 重置时调用。
    pub fn reset(&mut self) {
        self.state = PrefixModeState::Normal;
    }

    /// §16.7 更新 prefix mode 超时时间
    pub fn set_timeout_ms(&mut self, timeout_ms: u64) {
        self.config.timeout_ms = timeout_ms;
    }

    /// §16.7 获取超时时间
    pub fn timeout_ms(&self) -> u64 {
        self.config.timeout_ms
    }
}

/// §16.7 Prefix mode 处理结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixAction {
    /// 进入 prefix mode (启动超时计时器)
    EnterPrefixMode,
    /// 执行匹配的命令 (退出 prefix mode)
    ExecuteCommand,
    /// 按键透传到 PTY (无匹配或全屏应用模式)
    Passthrough,
    /// Double-tap: 发送 literal prefix key 到 PTY
    DoubleTapLiteral,
}

/// §16.7 全屏应用 passthrough 检测器
///
/// 检测终端应用是否启用了以下模式之一:
/// - Alt screen (DECSET 1049 / 1047) — vim, htop, less 等
/// - Bracketed paste (DECSET 2004) — 部分编辑器
/// - Mouse tracking (DECSET 1002/1003/1006) — vim, tmux 内应用
///
/// 检测原理: 监控 PTY 输出的 escape sequences，当检测到
/// `CSI ? 1049 h` / `CSI ? 1047 h` / `CSI ? 2004 h` / `CSI ? 1002 h` 等
/// 时，标记为全屏应用模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullScreenMode {
    /// 未检测到全屏模式
    None,
    /// Alt screen 模式 (DECSET 1049/1047)
    AltScreen,
    /// Bracketed paste 模式 (DECSET 2004)
    BracketedPaste,
    /// Mouse tracking 模式 (DECSET 1002/1003/1006)
    MouseTracking,
}

/// §16.7 检测 output bytes 中是否包含全屏模式 escape sequence
///
/// 检查 PTY 输出中是否包含以下序列:
/// - `\x1b[?1049h` — alt screen (on)
/// - `\x1b[?1047h` — alt screen (on, variant)
/// - `\x1b[?2004h` — bracketed paste (on)
/// - `\x1b[?1002h` — mouse tracking (normal)
/// - `\x1b[?1003h` — mouse tracking (any)
/// - `\x1b[?1006h` — mouse tracking (SGR)
///
/// 返回检测到的模式，若无则为 `FullScreenMode::None`。
///
/// 注意: 此函数只检测 "enable" 序列 (h suffix)。
/// "disable" 序列 (l suffix) 应调用 `detect_full_screen_disable` 来清除状态。
pub fn detect_full_screen_enable(output: &[u8]) -> FullScreenMode {
    // §16.7 查找 ESC [ ? 开头的 sequence (CSI Ps)
    let mut mode = FullScreenMode::None;

    // ESC = 0x1B, '[' = 0x5B, '?' = 0x3F
    // 查找 \x1b[?NNNNh 模式
    if let Some(pos) = find_csi_sequence(output, b'?', b'h') {
        if let Some(code) = parse_csi_parameter(output, pos) {
            match code {
                1049 | 1047 => mode = FullScreenMode::AltScreen,
                2004 => if mode == FullScreenMode::None {
                    mode = FullScreenMode::BracketedPaste
                },
                1002 | 1003 | 1006 => if mode == FullScreenMode::None {
                    mode = FullScreenMode::MouseTracking
                },
                _ => {}
            }
        }
    }

    mode
}

/// §16.7 检测 output bytes 中是否包含全屏模式关闭序列
///
/// 检查 PTY 输出中是否包含:
/// - `\x1b[?1049l` — alt screen (off)
/// - `\x1b[?1047l` — alt screen (off, variant)
/// - `\x1b[?2004l` — bracketed paste (off)
/// - `\x1b[?1002l` — mouse tracking (off)
/// - `\x1b[?1003l` — mouse tracking (off)
/// - `\x1b[?1006l` — mouse tracking (off)
///
/// 返回被关闭的模式，若无则为 `FullScreenMode::None`。
pub fn detect_full_screen_disable(output: &[u8]) -> FullScreenMode {
    let mut mode = FullScreenMode::None;

    if let Some(pos) = find_csi_sequence(output, b'?', b'l') {
        if let Some(code) = parse_csi_parameter(output, pos) {
            match code {
                1049 | 1047 => mode = FullScreenMode::AltScreen,
                2004 => mode = FullScreenMode::BracketedPaste,
                1002 | 1003 | 1006 => mode = FullScreenMode::MouseTracking,
                _ => {}
            }
        }
    }

    mode
}

/// §16.7 查找 CSI ? 序列: ESC [ ? ... suffix
fn find_csi_sequence(bytes: &[u8], _mode_byte: u8, suffix: u8) -> Option<usize> {
    // §16.7 查找 \x1b[?NNNNsuffix 模式
    // ESC = 0x1B, '[' = 0x5B, '?' = 0x3F
    const ESC: u8 = 0x1B;
    const LBRACKET: u8 = 0x5B;
    const QMARK: u8 = 0x3F;

    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i] == ESC
            && bytes[i + 1] == LBRACKET
            && bytes[i + 2] == QMARK
        {
            // 找到 ESC[? 开头，向后查找 suffix
            for j in (i + 3)..bytes.len() {
                if bytes[j] == suffix {
                    return Some(i);
                }
                // 数字或 ':' 分隔符，继续扫描
                if bytes[j] != b'0'
                    && bytes[j] != b'1'
                    && bytes[j] != b'2'
                    && bytes[j] != b'3'
                    && bytes[j] != b'4'
                    && bytes[j] != b'5'
                    && bytes[j] != b'6'
                    && bytes[j] != b'7'
                    && bytes[j] != b'8'
                    && bytes[j] != b'9'
                    && bytes[j] != b':'
                {
                    break;
                }
            }
        }
    }

    None
}

/// §16.7 解析 CSI 序列中的参数数字
fn parse_csi_parameter(bytes: &[u8], start: usize) -> Option<u32> {
    // §16.7 从 ESC[? 之后读取数字参数
    let param_start = start + 3; // skip ESC[?
    let mut num = 0u32;
    let mut found_digit = false;

    for &b in bytes[param_start..].iter() {
        match b {
            b'0'..=b'9' => {
                num = num.saturating_mul(10) + (b - b'0') as u32;
                found_digit = true;
            }
            _ => break,
        }
    }

    if found_digit {
        Some(num)
    } else {
        None
    }
}

/// §16.7 Prefix mode 超时配置
pub fn default_prefix_timeout() -> Duration {
    Duration::from_millis(500)
}

// ============================================================================
// §16.7 Pane 模式状态 (Plan 21)
// ============================================================================

/// §16.7 Pane 模式集合，用于判断当前终端处于什么模式
///
/// 由 dispatch_context 从 Terminal 的 Modes 标志位构建。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneModes {
    /// Alt screen 模式 (DECSET 1049/1047) — vim, htop, less 等全屏应用
    pub alt_screen: bool,
    /// Bracketed paste 模式 (DECSET 2004)
    pub bracketed_paste: bool,
    /// Mouse tracking 模式 (DECSET 1002/1003/1006)
    pub mouse_tracking: bool,
    /// 其他 DECSET 模式已启用
    pub any_decset: bool,
}

/// §16.7 检查 Pane 是否处于全屏应用模式
///
/// 当任一模式 (alt_screen, bracketed_paste, mouse_tracking, any_decset)
/// 为 true 时，认为当前 pane 运行全屏应用，输入应直接透传到 PTY。
pub fn is_full_screen_active(modes: &PaneModes) -> bool {
    modes.alt_screen || modes.bracketed_paste || modes.mouse_tracking || modes.any_decset
}

// ============================================================================
// §16.7 输入路由优先级链 (Plan 21)
// ============================================================================

/// §16.7 输入路由上下文
///
/// 包含输入路由决策所需的全部状态信息。
/// 由调用方 (TerminalView) 在 key_down 处理时构建并传入。
#[derive(Debug, Clone)]
pub struct KeyDispatchContext {
    /// IME 是否处于组字中 (composition active)。组字期间按键路由到 IME。
    pub ime_composing: bool,
    /// 扩展全局快捷键匹配结果。Some(action_id) 表示匹配到扩展快捷键。
    pub extension_shortcut: Option<String>,
    /// Prefix mode 状态机。
    pub prefix_mode_machine: PrefixModeMachine,
    /// 当前 pane 的模式状态 (alt screen, bracketed paste 等)。
    pub pane_modes: PaneModes,
    /// Agent CLI 是否处于活动状态 (agent 工具在终端中运行)。
    /// 活动期间所有输入直接透传到 PTY，不经过 prefix mode 等拦截。
    pub agent_cli_mode: bool,
    /// Copy mode (vi 浏览模式) 是否激活。
    pub copy_mode: bool,
}

/// §16.7 输入路由结果
///
/// 优先级链的每一步返回对应的路由结果，调用方根据结果执行相应操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDispatchResult {
    /// 路由到 IME: 按键由输入法处理，不发送到 PTY
    RouteToIme,
    /// 执行扩展全局快捷键。包含扩展 action ID。
    ExecuteExtensionAction(String),
    /// 执行 prefix mode 命令。调用方应执行对应的 mux 命令。
    ExecutePrefixCommand,
    /// 按键透传到 PTY (无匹配或全屏应用模式)
    Passthrough,
    /// 双击 prefix key，发送 literal 字节到 PTY
    SendLiteral {
        /// 要发送的字节 (prefix key 本身)
        bytes: Vec<u8>,
    },
    /// 发送到 PTY (终端应用)
    SendToPty {
        /// 要发送的字节
        bytes: Vec<u8>,
    },
    /// 路由到 Copy mode 处理器
    RouteToCopyMode,
    /// Agent CLI 透传: 输入直接发送到 PTY，不经过任何拦截
    RouteToAgentCli,
}

/// §16.7 输入路由优先级链调度器
///
/// 按以下优先级顺序检查输入路由:
/// 1. IME composing — 组字期间按键由 IME 处理
/// 2. 扩展全局快捷键 — 扩展定义的快捷键优先于 terminal 快捷键
/// 3. Prefix mode — 如果处于 prefix wait 状态，处理 prefix key
/// 4. Agent CLI 透传 — agent CLI 运行时输入直接透传
/// 5. 全屏应用透传 — alt screen / bracketed paste / mouse tracking 模式下透传
/// 6. Copy mode — vi 浏览模式处理
/// 7. 终端应用 — 默认发送到 PTY
///
/// 返回值指示调用方应采取的动作。
pub fn handle_key_event(
    key_bytes: &[u8],
    is_prefix_key: bool,
    binding_match: bool,
    ctx: &mut KeyDispatchContext,
) -> KeyDispatchResult {
    // §16.7 Step 1: IME composing?
    // 组字期间按键由 IME 处理，不发送到 PTY
    if ctx.ime_composing {
        return KeyDispatchResult::RouteToIme;
    }

    // §16.7 Step 2: Extension global shortcut?
    // 扩展快捷键优先于 terminal 快捷键
    if let Some(action) = &ctx.extension_shortcut {
        return KeyDispatchResult::ExecuteExtensionAction(action.clone());
    }

    // §16.7 Step 3: Prefix mode active?
    // 处理 prefix mode 状态机
    let machine = &mut ctx.prefix_mode_machine;
    if machine.is_prefix_wait() {
        // §16.7 处于 prefix wait 状态，处理后续按键
        let action = machine.on_prefix_wait_key(is_prefix_key, binding_match);
        match action {
            PrefixAction::ExecuteCommand => return KeyDispatchResult::ExecutePrefixCommand,
            PrefixAction::DoubleTapLiteral => {
                return KeyDispatchResult::SendLiteral {
                    bytes: key_bytes.to_vec(),
                }
            }
            PrefixAction::Passthrough => return KeyDispatchResult::Passthrough,
            PrefixAction::EnterPrefixMode => {} // 不会在此分支触发
        }
    }

    // §16.7 Step 4: Agent CLI passthrough?
    // Agent CLI 运行时，输入直接透传到 PTY，不经过 prefix mode 等拦截
    if ctx.agent_cli_mode {
        return KeyDispatchResult::RouteToAgentCli;
    }

    // §16.7 Step 5: Full-screen app passthrough?
    // 全屏应用模式下 prefix key 也透传到 PTY
    if is_full_screen_active(&ctx.pane_modes) {
        if is_prefix_key && machine.is_prefix_wait() {
            // §16.7 双击 prefix key: 发送 literal 字节
            machine.on_timeout();
            return KeyDispatchResult::SendLiteral {
                bytes: key_bytes.to_vec(),
            }
        }
        // §16.7 全屏应用 passthrough: 直接发送到 PTY
        return KeyDispatchResult::SendToPty {
            bytes: key_bytes.to_vec(),
        };
    }

    // §16.7 检查是否为 prefix key，进入 prefix mode
    if is_prefix_key {
        let action = machine.on_prefix_key();
        match action {
            PrefixAction::EnterPrefixMode => {
                // §16.7 进入 prefix mode，等待后续按键
                return KeyDispatchResult::Passthrough;
            }
            PrefixAction::Passthrough => {
                // §16.7 全屏模式下 prefix key 透传 (已由上面的全屏检测处理)
                return KeyDispatchResult::SendToPty {
                    bytes: key_bytes.to_vec(),
                };
            }
            _ => {}
        }
    }

    // §16.7 Step 6: Copy mode?
    // vi 浏览模式下按键由 copy mode 处理器处理
    if ctx.copy_mode {
        return KeyDispatchResult::RouteToCopyMode;
    }

    // §16.7 Step 7: Terminal application (default)
    // 默认将按键发送到 PTY
    KeyDispatchResult::SendToPty {
        bytes: key_bytes.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_mode_enter_and_execute() {
        // §16.7 测试: prefix key → prefix wait → 匹配 binding → 执行命令
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());

        assert_eq!(machine.state(), PrefixModeState::Normal);

        // 按下 prefix key
        let action = machine.on_prefix_key();
        assert_eq!(action, PrefixAction::EnterPrefixMode);
        assert!(machine.is_prefix_wait());

        // 按下匹配的 binding 键
        let action = machine.on_prefix_wait_key(false, true);
        assert_eq!(action, PrefixAction::ExecuteCommand);
        assert!(!machine.is_prefix_wait());
        assert_eq!(machine.state(), PrefixModeState::Normal);
    }

    #[test]
    fn test_prefix_mode_passthrough_no_match() {
        // §16.7 测试: prefix key → prefix wait → 无匹配 → 透传到 PTY
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());

        let action = machine.on_prefix_key();
        assert_eq!(action, PrefixAction::EnterPrefixMode);

        // 按下不匹配的键
        let action = machine.on_prefix_wait_key(false, false);
        assert_eq!(action, PrefixAction::Passthrough);
        assert!(!machine.is_prefix_wait());
    }

    #[test]
    fn test_prefix_mode_double_tap() {
        // §16.7 测试: prefix key → prefix wait → prefix key (double-tap) → literal
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());

        let action = machine.on_prefix_key();
        assert_eq!(action, PrefixAction::EnterPrefixMode);

        // 再次按下 prefix key (double-tap)
        let action = machine.on_prefix_wait_key(true, false);
        assert_eq!(action, PrefixAction::DoubleTapLiteral);
        assert!(!machine.is_prefix_wait());
    }

    #[test]
    fn test_prefix_mode_full_screen_passthrough() {
        // §16.7 测试: 全屏应用模式下 prefix key 透传到 PTY
        let mut config = PrefixModeConfig::default();
        config.full_screen_passthrough = true;
        let mut machine = PrefixModeMachine::new(config);

        // prefix key 直接透传，不进入 prefix mode
        let action = machine.on_prefix_key();
        assert_eq!(action, PrefixAction::Passthrough);
        assert_eq!(machine.state(), PrefixModeState::Normal);
    }

    #[test]
    fn test_prefix_mode_timeout() {
        // §16.7 测试: prefix wait 超时后退出到 normal
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());

        machine.on_prefix_key();
        assert!(machine.is_prefix_wait());

        machine.on_timeout();
        assert!(!machine.is_prefix_wait());
        assert_eq!(machine.state(), PrefixModeState::Normal);
    }

    #[test]
    fn test_prefix_mode_reset() {
        // §16.7 测试: reset 强制回到 normal
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());

        machine.on_prefix_key();
        assert!(machine.is_prefix_wait());

        machine.reset();
        assert_eq!(machine.state(), PrefixModeState::Normal);
    }

    #[test]
    fn test_detect_alt_screen_enable() {
        // §16.7 测试: 检测 alt screen 开启序列
        let output = b"\x1b[?1049h";
        let mode = detect_full_screen_enable(output);
        assert_eq!(mode, FullScreenMode::AltScreen);

        let output2 = b"\x1b[?1047h";
        let mode2 = detect_full_screen_enable(output2);
        assert_eq!(mode2, FullScreenMode::AltScreen);
    }

    #[test]
    fn test_detect_alt_screen_disable() {
        // §16.7 测试: 检测 alt screen 关闭序列
        let output = b"\x1b[?1049l";
        let mode = detect_full_screen_disable(output);
        assert_eq!(mode, FullScreenMode::AltScreen);
    }

    #[test]
    fn test_detect_bracketed_paste() {
        // §16.7 测试: 检测 bracketed paste 开启序列
        let output = b"\x1b[?2004h";
        let mode = detect_full_screen_enable(output);
        assert_eq!(mode, FullScreenMode::BracketedPaste);
    }

    #[test]
    fn test_detect_mouse_tracking() {
        // §16.7 测试: 检测 mouse tracking 开启序列
        let output = b"\x1b[?1002h";
        let mode = detect_full_screen_enable(output);
        assert_eq!(mode, FullScreenMode::MouseTracking);

        let output2 = b"\x1b[?1006h";
        let mode2 = detect_full_screen_enable(output2);
        assert_eq!(mode2, FullScreenMode::MouseTracking);
    }

    #[test]
    fn test_detect_no_full_screen() {
        // §16.7 测试: 普通输出不包含全屏模式
        let output = b"Hello, world!\n";
        let mode = detect_full_screen_enable(output);
        assert_eq!(mode, FullScreenMode::None);
    }

    #[test]
    fn test_detect_in_buffer() {
        // §16.7 测试: escape sequence 出现在输出中间
        let output = b"Some text\x1b[?1049h more text";
        let mode = detect_full_screen_enable(output);
        assert_eq!(mode, FullScreenMode::AltScreen);
    }

    #[test]
    fn test_prefix_mode_set_timeout() {
        // §16.7 测试: 更新超时时间
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());
        assert_eq!(machine.timeout_ms(), 500);

        machine.set_timeout_ms(1000);
        assert_eq!(machine.timeout_ms(), 1000);
    }

    #[test]
    fn test_prefix_mode_set_full_screen_passthrough() {
        // §16.7 测试: 运行时切换全屏 passthrough
        let mut machine = PrefixModeMachine::new(PrefixModeConfig::default());

        machine.on_prefix_key();
        assert_eq!(machine.state(), PrefixModeState::PrefixWait);

        machine.reset();
        machine.set_full_screen_passthrough(true);
        machine.on_prefix_key();
        assert_eq!(machine.state(), PrefixModeState::Normal);
    }

    #[test]
    fn test_csi_parameter_parsing() {
        // §16.7 测试: CSI 参数解析
        let bytes = b"\x1b[?1049h";
        let code = parse_csi_parameter(bytes, 0);
        assert_eq!(code, Some(1049));

        let bytes2 = b"\x1b[?1002h";
        let code2 = parse_csi_parameter(bytes2, 0);
        assert_eq!(code2, Some(1002));
    }

    #[test]
    fn test_csi_parameter_no_digit() {
        // §16.7 测试: 无数字参数的 CSI 序列
        let bytes = b"\x1b[?h";
        let code = parse_csi_parameter(bytes, 0);
        assert_eq!(code, None);
    }

    #[test]
    fn test_priority_ime_composing() {
        // §16.7 测试: IME 组字期间按键路由到 IME
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: true,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };

        let result = handle_key_event(b"a", false, false, &mut ctx);
        assert_eq!(result, KeyDispatchResult::RouteToIme);
    }

    #[test]
    fn test_priority_extension_shortcut() {
        // §16.7 测试: 扩展全局快捷键优先于 prefix mode
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: Some("toggle_pane".to_string()),
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };

        let result = handle_key_event(b"c", false, false, &mut ctx);
        assert_eq!(
            result,
            KeyDispatchResult::ExecuteExtensionAction("toggle_pane".to_string())
        );
    }

    #[test]
    fn test_priority_prefix_execute_command() {
        // §16.7 测试: prefix mode 匹配 binding → 执行命令
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };

        // 先按下 prefix key 进入 prefix wait
        let result = handle_key_event(b"\x02", true, false, &mut ctx);
        assert_eq!(result, KeyDispatchResult::Passthrough);

        // 再按下匹配的 binding 键
        let result = handle_key_event(b"c", false, true, &mut ctx);
        assert_eq!(result, KeyDispatchResult::ExecutePrefixCommand);
    }

    #[test]
    fn test_priority_prefix_passthrough_no_match() {
        // §16.7 测试: prefix mode 不匹配 → 透传到 PTY
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };

        // 进入 prefix wait
        let _ = handle_key_event(b"\x02", true, false, &mut ctx);

        // 不匹配的键 → 透传
        let result = handle_key_event(b"x", false, false, &mut ctx);
        assert_eq!(result, KeyDispatchResult::Passthrough);
    }

    #[test]
    fn test_priority_prefix_double_tap() {
        // §16.7 测试: prefix mode double-tap → 发送 literal 字节
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };

        // 进入 prefix wait
        let _ = handle_key_event(b"\x02", true, false, &mut ctx);

        // 再次按下 prefix key (double-tap)
        let result = handle_key_event(b"\x02", true, false, &mut ctx);
        assert_eq!(
            result,
            KeyDispatchResult::SendLiteral { bytes: vec![0x02] }
        );
    }

    #[test]
    fn test_priority_agent_cli_passthrough() {
        // §16.7 测试: Agent CLI 透传 — 输入直接发送到 PTY
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: true,
            copy_mode: false,
        };

        let result = handle_key_event(b"a", false, false, &mut ctx);
        assert_eq!(result, KeyDispatchResult::RouteToAgentCli);
    }

    #[test]
    fn test_priority_full_screen_passthrough() {
        // §16.7 测试: 全屏应用模式下输入透传到 PTY
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes {
                alt_screen: true,
                bracketed_paste: false,
                mouse_tracking: false,
                any_decset: false,
            },
            agent_cli_mode: false,
            copy_mode: false,
        };

        let result = handle_key_event(b"v", false, false, &mut ctx);
        assert_eq!(
            result,
            KeyDispatchResult::SendToPty { bytes: vec![b'v'] }
        );
    }

    #[test]
    fn test_priority_copy_mode() {
        // §16.7 测试: Copy mode 激活时路由到 copy mode 处理器
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: true,
        };

        let result = handle_key_event(b"j", false, false, &mut ctx);
        assert_eq!(result, KeyDispatchResult::RouteToCopyMode);
    }

    #[test]
    fn test_priority_terminal_default() {
        // §16.7 测试: 默认路由到终端应用 (PTY)
        let machine = PrefixModeMachine::new(PrefixModeConfig::default());
        let mut ctx = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: machine,
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };

        // 非 prefix key, 非全屏, 非 copy mode → 发送到 PTY
        let result = handle_key_event(b"h", false, false, &mut ctx);
        assert_eq!(result, KeyDispatchResult::SendToPty { bytes: vec![b'h'] });
    }

    #[test]
    fn test_priority_chain_order() {
        // §16.7 测试: 验证优先级链顺序
        // IME > extension > prefix > agent_cli > full_screen > copy > terminal

        // IME 优先于 extension
        let mut ctx_ime_ext = KeyDispatchContext {
            ime_composing: true,
            extension_shortcut: Some("action".to_string()),
            prefix_mode_machine: PrefixModeMachine::new(PrefixModeConfig::default()),
            pane_modes: PaneModes::default(),
            agent_cli_mode: false,
            copy_mode: false,
        };
        let result = handle_key_event(b"a", false, false, &mut ctx_ime_ext);
        assert_eq!(result, KeyDispatchResult::RouteToIme);

        // Extension 优先于 agent CLI
        let mut ctx_ext_agent = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: Some("action".to_string()),
            prefix_mode_machine: PrefixModeMachine::new(PrefixModeConfig::default()),
            pane_modes: PaneModes::default(),
            agent_cli_mode: true,
            copy_mode: false,
        };
        let result = handle_key_event(b"a", false, false, &mut ctx_ext_agent);
        assert_eq!(
            result,
            KeyDispatchResult::ExecuteExtensionAction("action".to_string())
        );

        // Agent CLI 优先于 full screen
        let mut ctx_agent_fs = KeyDispatchContext {
            ime_composing: false,
            extension_shortcut: None,
            prefix_mode_machine: PrefixModeMachine::new(PrefixModeConfig::default()),
            pane_modes: PaneModes {
                alt_screen: true,
                bracketed_paste: false,
                mouse_tracking: false,
                any_decset: false,
            },
            agent_cli_mode: true,
            copy_mode: false,
        };
        let result = handle_key_event(b"a", false, false, &mut ctx_agent_fs);
        assert_eq!(result, KeyDispatchResult::RouteToAgentCli);
    }

    #[test]
    fn test_is_full_screen_active() {
        // §16.7 测试: is_full_screen_active 检查
        assert!(!is_full_screen_active(&PaneModes::default()));
        assert!(is_full_screen_active(&PaneModes {
            alt_screen: true,
            ..Default::default()
        }));
        assert!(is_full_screen_active(&PaneModes {
            bracketed_paste: true,
            ..Default::default()
        }));
        assert!(is_full_screen_active(&PaneModes {
            mouse_tracking: true,
            ..Default::default()
        }));
        assert!(is_full_screen_active(&PaneModes {
            any_decset: true,
            ..Default::default()
        }));
    }
}
