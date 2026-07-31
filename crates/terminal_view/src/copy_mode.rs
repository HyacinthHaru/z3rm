// §12 复制模式: vi 风格导航 + 文本选择 + 搜索 + 复制到剪贴板
// Plan 31 — Copy mode for terminal scrollback browsing

use gpui::{Context, Entity, Keystroke};
use terminal::Terminal;

/// 复制模式状态
///
/// 当复制模式激活时，TerminalView 拦截所有按键，
/// 将导航命令路由到 terminal 的 vi_motion，
/// 将编辑命令 (V, /, q, n, N, escape, i) 拦截到本模块。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyModeState {
    /// 是否激活复制模式
    pub active: bool,
    /// `/` 搜索输入缓冲区。`Some` 表示正在输入查询串 (回车确认 / Esc 取消)。
    pub search_input: Option<String>,
    /// 已确认的搜索查询 (None = 无搜索)，由 `n` / `N` 导航。
    pub search_query: Option<String>,
    /// `search_query` 的匹配数量。
    pub match_count: usize,
    /// 查询无法编译为正则时的错误文本。
    pub search_error: Option<String>,
}

impl CopyModeState {
    /// §12 Plan 31 — 复制模式搜索状态的可见描述，供 UI 指示器渲染。
    pub fn search_indicator(&self) -> Option<String> {
        if let Some(input) = &self.search_input {
            return Some(format!("/{input}"));
        }
        let query = self.search_query.as_ref()?;
        Some(match &self.search_error {
            Some(error) => format!("/{query} — {error}"),
            None if self.match_count == 0 => format!("/{query} — no matches"),
            None => format!("/{query} — {} matches", self.match_count),
        })
    }

    fn clear_search(&mut self) {
        self.search_input = None;
        self.search_query = None;
        self.match_count = 0;
        self.search_error = None;
    }
}

/// 在复制模式处理按键。
///
/// 返回 `true` 表示按键已被拦截（不发送到 PTY）。
/// 返回 `false` 表示按键应转发到 terminal.vi_motion。
pub fn dispatch_copy_mode_key(
    keystroke: &Keystroke,
    state: &mut CopyModeState,
    terminal: &Entity<Terminal>,
    cx: &mut Context<super::TerminalView>,
) -> bool {
    // §12 Plan 31 — 搜索输入优先于所有复制模式命令，否则查询串里就无法
    // 输入 q / n / i 这些同时也是命令的字符。
    if state.search_input.is_some() {
        return dispatch_search_input_key(keystroke, state, terminal, cx);
    }

    match keystroke.key.as_str() {
        // V: 行选择模式 (Line selection) — §12 Plan 31
        // 现在由 terminal.vi_motion 处理 (uppercase V via shift),转发即可。
        "v" if keystroke.modifiers.shift => false, // forward to vi_motion (handles "V")

        // /: 进入搜索输入状态 — §12 Plan 31
        "/" => {
            state.search_input = Some(String::new());
            true
        }

        // q: 退出复制模式
        "q" => {
            exit_copy_mode(terminal, state, cx);
            true
        }

        // N (shift-n): 上一个搜索匹配 — §12 Plan 31
        "n" if keystroke.modifiers.shift => {
            if has_query(state) {
                terminal.update(cx, |term, _| term.search_previous());
            }
            true
        }

        // n: 下一个搜索匹配 — §12 Plan 31
        "n" => {
            if has_query(state) {
                terminal.update(cx, |term, _| term.search_next());
            }
            true
        }

        // escape: 清除选择 + 退出复制模式
        "escape" => {
            exit_copy_mode(terminal, state, cx);
            true
        }

        // i: 退出复制模式
        "i" => {
            exit_copy_mode(terminal, state, cx);
            true
        }

        // 其他按键: 转发到 terminal.vi_motion
        _ => false,
    }
}

fn has_query(state: &CopyModeState) -> bool {
    state
        .search_query
        .as_ref()
        .is_some_and(|query| !query.is_empty())
}

/// §12 Plan 31 — `/` 搜索输入状态的按键处理。
///
/// 始终返回 `true`: 输入期间所有按键都由搜索输入消费，不得落到 vi_motion
/// 或 PTY。
fn dispatch_search_input_key(
    keystroke: &Keystroke,
    state: &mut CopyModeState,
    terminal: &Entity<Terminal>,
    cx: &mut Context<super::TerminalView>,
) -> bool {
    match keystroke.key.as_str() {
        "escape" => {
            state.search_input = None;
        }
        "enter" => {
            let query = state.search_input.take().unwrap_or_default();
            confirm_search(query, state, terminal, cx);
        }
        "backspace" => {
            if let Some(input) = state.search_input.as_mut() {
                input.pop();
            }
        }
        _ => {
            if let Some(text) = typed_text(keystroke)
                && let Some(input) = state.search_input.as_mut()
            {
                input.push_str(&text);
            }
        }
    }
    true
}

/// §12 Plan 31 — 按键为搜索查询贡献的文本。
///
/// 平台按键事件带 `key_char`；合成按键 (`SendKeystroke`、测试) 只有键名，
/// 因此单字符键名作为回退。带 control / platform / function 修饰的和弦不是
/// 可打印输入，不得写进查询串。
fn typed_text(keystroke: &Keystroke) -> Option<String> {
    if keystroke.modifiers.control
        || keystroke.modifiers.platform
        || keystroke.modifiers.function
    {
        return None;
    }
    if let Some(key_char) = keystroke.key_char.as_ref() {
        return Some(key_char.clone());
    }
    if keystroke.key.chars().count() != 1 {
        return None;
    }
    Some(if keystroke.modifiers.shift {
        keystroke.key.to_uppercase()
    } else {
        keystroke.key.clone()
    })
}

fn confirm_search(
    query: String,
    state: &mut CopyModeState,
    terminal: &Entity<Terminal>,
    cx: &mut Context<super::TerminalView>,
) {
    if query.is_empty() {
        state.clear_search();
        terminal.update(cx, |term, _| term.clear_search());
        return;
    }

    match terminal.update(cx, |term, _| term.set_search_query(&query)) {
        Ok(match_count) => {
            state.match_count = match_count;
            state.search_error = None;
            if match_count > 0 {
                terminal.update(cx, |term, _| term.search_next());
            }
        }
        Err(error) => {
            state.match_count = 0;
            state.search_error = Some(error.to_string());
        }
    }
    state.search_query = Some(query);
}

/// 进入复制模式 — §12 Plan 31
///
/// 先启用 vi 模式（复制模式基于 vi 模式），然后激活复制模式。
pub fn enter_copy_mode(
    terminal: &Entity<Terminal>,
    state: &mut CopyModeState,
    cx: &mut Context<super::TerminalView>,
) {
    // 先启用 vi 模式
    terminal.update(cx, |term, _| {
        if !term.vi_mode_enabled() {
            term.toggle_vi_mode();
        }
    });
    state.active = true;
    state.clear_search();
}

/// 退出复制模式 — §12 Plan 31
///
/// 清除选择，退出 vi 模式，关闭复制模式。
pub fn exit_copy_mode(
    terminal: &Entity<Terminal>,
    state: &mut CopyModeState,
    cx: &mut Context<super::TerminalView>,
) {
    terminal.update(cx, |term, _| {
        // 清除选择
        let mut escape = Keystroke::default();
        escape.key = "escape".to_string();
        term.vi_motion(&escape);
        term.clear_search();
        if term.vi_mode_enabled() {
            term.toggle_vi_mode();
        }
    });

    state.active = false;
    state.clear_search();
}
