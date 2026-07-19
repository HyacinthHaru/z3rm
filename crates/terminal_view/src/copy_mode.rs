// §12 复制模式: vi 风格导航 + 文本选择 + 复制到剪贴板
// Plan 31 — Copy mode for terminal scrollback browsing

use gpui::{Context, Entity, Keystroke};
use terminal::Terminal;

/// 复制模式状态
///
/// 当复制模式激活时，TerminalView 拦截所有按键，
/// 将导航命令路由到 terminal 的 vi_motion，
/// 将编辑命令 (V, /, q, n, N, escape, i) 拦截到本模块。
#[derive(Clone, Debug, Default)]
pub struct CopyModeState {
    /// 是否激活复制模式
    pub active: bool,
    /// 当前搜索查询 (None = 无搜索)
    pub search_query: Option<String>,
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
    match keystroke.key.as_str() {
        // V: 行选择模式 (Line selection) — §12 Plan 31
        // 现在由 terminal.vi_motion 处理 (uppercase V via shift),转发即可。
        "v" if keystroke.modifiers.shift => false, // forward to vi_motion (handles "V")

        // /: 进入搜索模式 — §12 Plan 31
        "/" => {
            state.search_query = Some(String::new());
            // 实际搜索输入由外部 search input 处理,这里仅拦截防止 / 到 PTY。
            true
        }

        // q: 退出复制模式
        "q" => {
            state.active = false;
            state.search_query = None;
            true
        }

        // n: 下一个搜索匹配 — §12 Plan 31
        "n" => {
            if let Some(query) = &state.search_query {
                if !query.is_empty() {
                    terminal.update(cx, |term, _| {
                        // 触发 alacritty 的搜索导航 (下一个匹配)
                        // terminal.search 需要外部预置 Search 对象,这里
                        // 简化为 forward 到 vi_motion (alacritty 的 n 绑定)。
                        // 完整实现需要 terminal.rs 暴露 search_next() 方法。
                        let mut ks = Keystroke::default();
                        ks.key = "n".to_string();
                        term.vi_motion(&ks);
                    });
                }
            }
            true
        }

        // N: 上一个搜索匹配 — §12 Plan 31
        "N" => {
            if let Some(query) = &state.search_query {
                if !query.is_empty() {
                    terminal.update(cx, |term, _| {
                        let mut ks = Keystroke::default();
                        ks.key = "N".to_string();
                        ks.modifiers.shift = true;
                        term.vi_motion(&ks);
                    });
                }
            }
            true
        }

        // escape: 清除选择 + 退出复制模式
        "escape" => {
            terminal.update(cx, |term, _| {
                let mut esc = Keystroke::default();
                esc.key = "escape".to_string();
                term.vi_motion(&esc);
            });
            state.active = false;
            state.search_query = None;
            true
        }

        // i: 退出复制模式
        "i" => {
            state.active = false;
            state.search_query = None;
            true
        }

        // 其他按键: 转发到 terminal.vi_motion
        _ => false,
    }
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
        term.toggle_vi_mode();
    });
    state.active = true;
    state.search_query = None;
}

/// 退出复制模式 — §12 Plan 31
///
/// 清除选择，退出 vi 模式，关闭复制模式。
pub fn exit_copy_mode(
    terminal: &Entity<Terminal>,
    state: &mut CopyModeState,
    cx: &mut Context<super::TerminalView>,
) {
    // 清除选择
    terminal.update(cx, |term, _| {
        let mut esc = Keystroke::default();
        esc.key = "escape".to_string();
        term.vi_motion(&esc);
    });

    state.active = false;
    state.search_query = None;
}
