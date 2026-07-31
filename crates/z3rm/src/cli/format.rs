// tmux 风格 `#{...}` 格式字符串引擎
// 来源: spec §3.10 — `-F` 让 agent 用稳定的机器可读格式读取 session/window/pane 元数据

use anyhow::{Result, anyhow};
use mux_protocol::proto::{PaneInfo, SessionInfo, TabInfo};

/// 一次 `-F` 展开中可见的实体。
///
/// 字段为 `None` 表示当前命令没有这一层上下文（`ls` 拿不到 window/pane），
/// 此时对应变量按 tmux 语义展开成空串而不是报错，这样同一个格式串可以在
/// 多个命令之间复用。
#[derive(Default, Clone, Copy)]
pub struct FormatScope<'a> {
    pub session: Option<&'a SessionInfo>,
    pub session_windows: Option<usize>,
    pub window: Option<&'a TabInfo>,
    pub window_index: Option<usize>,
    pub window_active: Option<bool>,
    pub pane: Option<&'a PaneInfo>,
    pub pane_index: Option<usize>,
    pub pane_active: Option<bool>,
}

impl FormatScope<'_> {
    /// 查询变量。`None` 表示变量名未知或当前作用域没有它。
    ///
    /// 只暴露 `mux.proto` 里真实存在且服务端会填充的字段；tmux 里那些依赖
    /// 进程表的变量（`pane_pid`、`pane_current_command`）没有协议来源，
    /// 故意不实现，以免返回骗人的空值。
    fn lookup(&self, name: &str) -> Option<String> {
        match name {
            "session_id" => self.session.map(|session| session.id.clone()),
            "session_name" => self.session.map(|session| session.name.clone()),
            "session_path" => self.session.map(|session| session.cwd.clone()),
            "session_attached" => self
                .session
                .map(|session| session.attached_clients.to_string()),
            "session_created" => self
                .session
                .map(|session| session.created_timestamp.to_string()),
            "session_windows" => self.session_windows.map(|count| count.to_string()),
            "window_id" => self.window.map(|window| window.id.clone()),
            "window_name" => self.window.map(|window| window.title.clone()),
            "window_index" => self.window_index.map(|index| index.to_string()),
            "window_active" => self.window_active.map(format_flag),
            "window_panes" => self.window.map(|window| window.panes.len().to_string()),
            "pane_id" => self.pane.map(|pane| pane.id.clone()),
            "pane_index" => self.pane_index.map(|index| index.to_string()),
            "pane_title" => self.pane.map(|pane| pane.title.clone()),
            "pane_active" => self.pane_active.map(format_flag),
            "pane_width" => self.pane.map(|pane| pane_size(pane).0.to_string()),
            "pane_height" => self.pane.map(|pane| pane_size(pane).1.to_string()),
            "pane_current_path" => self.pane.map(|pane| pane.cwd.clone()),
            "pane_start_command" => self.pane.map(|pane| pane.command.clone()),
            "pane_dead" => self.pane.map(|pane| format_flag(!pane.is_alive)),
            _ => None,
        }
    }
}

fn format_flag(value: bool) -> String {
    if value {
        "1".to_string()
    } else {
        "0".to_string()
    }
}

fn pane_size(pane: &PaneInfo) -> (u32, u32) {
    pane.size
        .as_ref()
        .map(|size| (size.cols, size.rows))
        .unwrap_or((0, 0))
}

/// 展开一个 tmux 格式串。
///
/// 支持的语法：
/// - `##` — 字面量 `#`
/// - `#{name}` — 变量替换，未知变量展开成空串（tmux 行为）
/// - `#{?condition,when-true,when-false}` — 条件分支，两个分支会被递归展开
pub fn expand(format: &str, scope: &FormatScope<'_>) -> Result<String> {
    let mut output = String::new();
    let mut rest = format;
    while let Some(hash) = rest.find('#') {
        output.push_str(&rest[..hash]);
        let after = &rest[hash + 1..];
        match after.as_bytes().first() {
            Some(b'#') => {
                output.push('#');
                rest = &after[1..];
            }
            Some(b'{') => {
                let body = &after[1..];
                let end = matching_brace(body)
                    .ok_or_else(|| anyhow!("unterminated '#{{' in format string: {format}"))?;
                output.push_str(&expand_body(&body[..end], scope)?);
                rest = &body[end + 1..];
            }
            _ => {
                output.push('#');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    Ok(output)
}

fn expand_body(body: &str, scope: &FormatScope<'_>) -> Result<String> {
    let Some(conditional) = body.strip_prefix('?') else {
        return Ok(scope.lookup(body).unwrap_or_default());
    };
    let (condition, when_true, when_false) = split_conditional(conditional).ok_or_else(|| {
        anyhow!("conditional format '#{{?{conditional}}}' needs a ',' after the condition")
    })?;
    if evaluate_condition(condition, scope)? {
        expand(when_true, scope)
    } else {
        expand(when_false, scope)
    }
}

/// 条件里既可以直接写变量名（`#{?pane_active,..}`），也可以写一段带 `#{}`
/// 的子格式串。裸变量名不能先展开再判真假，否则 `pane_active` 这个字面词
/// 本身非空就永远为真。
fn evaluate_condition(condition: &str, scope: &FormatScope<'_>) -> Result<bool> {
    let value = if condition.contains("#{") {
        expand(condition, scope)?
    } else {
        scope.lookup(condition).unwrap_or_default()
    };
    Ok(!value.is_empty() && value != "0")
}

/// 返回与外层 `#{` 配对的 `}` 在 `body` 中的字节偏移。
///
/// 只匹配 ASCII 字节，多字节 UTF-8 序列不含 ASCII 字节，所以返回的偏移
/// 始终落在字符边界上。
fn matching_brace(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'#' if bytes.get(index + 1) == Some(&b'#') => index += 2,
            b'#' if bytes.get(index + 1) == Some(&b'{') => {
                depth += 1;
                index += 2;
            }
            b'}' if depth == 0 => return Some(index),
            b'}' => {
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

/// 在顶层逗号处把 `condition,true,false` 切成三段。
/// 嵌套 `#{...}` 内部的逗号属于内层，不参与切分；第二个逗号之后的内容
/// 全部归 false 分支，这样 false 分支里可以直接写逗号。
fn split_conditional(body: &str) -> Option<(&str, &str, &str)> {
    let first = top_level_comma(body)?;
    let (condition, rest) = (&body[..first], &body[first + 1..]);
    match top_level_comma(rest) {
        Some(second) => Some((condition, &rest[..second], &rest[second + 1..])),
        None => Some((condition, rest, "")),
    }
}

fn top_level_comma(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'#' if bytes.get(index + 1) == Some(&b'#') => index += 2,
            b'#' if bytes.get(index + 1) == Some(&b'{') => {
                depth += 1;
                index += 2;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                index += 1;
            }
            b',' if depth == 0 => return Some(index),
            _ => index += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::proto::TerminalSize;

    fn session() -> SessionInfo {
        SessionInfo {
            id: "sess-1".into(),
            name: "dev".into(),
            cwd: "/tmp/work".into(),
            created_timestamp: 1700,
            attached_clients: 2,
        }
    }

    fn pane() -> PaneInfo {
        PaneInfo {
            id: "pane-1".into(),
            cwd: "/tmp/work/src".into(),
            title: "zsh".into(),
            command: "/bin/zsh".into(),
            generation: 7,
            size: Some(TerminalSize { cols: 80, rows: 24 }),
            is_alive: true,
            zoomed: false,
        }
    }

    fn window(panes: Vec<PaneInfo>) -> TabInfo {
        TabInfo {
            id: "tab-1".into(),
            title: "editor".into(),
            panes,
        }
    }

    #[test]
    fn expands_session_window_and_pane_variables() {
        let session = session();
        let pane = pane();
        let window = window(vec![pane.clone()]);
        let scope = FormatScope {
            session: Some(&session),
            session_windows: Some(3),
            window: Some(&window),
            window_index: Some(1),
            window_active: Some(true),
            pane: Some(&pane),
            pane_index: Some(0),
            pane_active: Some(false),
        };

        let rendered = expand(
            "#{session_name}:#{window_index}.#{pane_index} #{window_name} #{pane_title} \
             #{pane_width}x#{pane_height} #{session_attached} #{session_windows} \
             #{window_panes} #{window_active}#{pane_active} #{session_id} #{window_id} \
             #{pane_id} #{session_path} #{pane_current_path} #{session_created} \
             #{pane_start_command} #{pane_dead}",
            &scope,
        )
        .expect("expand");
        assert_eq!(
            rendered,
            "dev:1.0 editor zsh 80x24 2 3 1 10 sess-1 tab-1 pane-1 /tmp/work /tmp/work/src \
             1700 /bin/zsh 0"
        );
    }

    #[test]
    fn unknown_variables_expand_to_empty_string() {
        let session = session();
        let scope = FormatScope {
            session: Some(&session),
            ..Default::default()
        };
        assert_eq!(
            expand("[#{pane_pid}][#{not_a_variable}]", &scope).expect("expand"),
            "[][]"
        );
    }

    #[test]
    fn variables_outside_the_scope_expand_to_empty_string() {
        // `ls` 没有 window/pane 上下文，同一个格式串仍然要能跑，不能报错。
        let session = session();
        let scope = FormatScope {
            session: Some(&session),
            ..Default::default()
        };
        assert_eq!(
            expand("#{session_name}|#{window_name}|#{pane_title}", &scope).expect("expand"),
            "dev||"
        );
    }

    #[test]
    fn double_hash_is_a_literal_hash_and_braces_stay_literal() {
        let scope = FormatScope::default();
        assert_eq!(
            expand("##{session_name} {literal} #not-a-var", &scope).expect("expand"),
            "#{session_name} {literal} #not-a-var"
        );
    }

    #[test]
    fn unterminated_variable_is_an_error() {
        let scope = FormatScope::default();
        let error = expand("#{session_name", &scope).expect_err("unterminated");
        assert!(error.to_string().contains("unterminated"), "{error}");
    }

    #[test]
    fn conditionals_select_a_branch_and_nest() {
        let session = session();
        let pane = pane();
        let window = window(vec![pane.clone()]);
        let active = FormatScope {
            session: Some(&session),
            window: Some(&window),
            window_index: Some(0),
            window_active: Some(true),
            pane: Some(&pane),
            pane_index: Some(0),
            pane_active: Some(true),
            ..Default::default()
        };
        let inactive = FormatScope {
            pane_active: Some(false),
            ..active
        };

        assert_eq!(expand("#{?pane_active,*,-}", &active).expect("expand"), "*");
        assert_eq!(
            expand("#{?pane_active,*,-}", &inactive).expect("expand"),
            "-"
        );

        // 嵌套：分支里再放变量，条件里再放一层 `#{}`。
        assert_eq!(
            expand(
                "#{?window_active,#{window_name}#{?pane_active,*,},#{session_name}}",
                &active
            )
            .expect("expand"),
            "editor*"
        );
        assert_eq!(
            expand("#{?#{pane_active},on,off}", &inactive).expect("expand"),
            "off"
        );
    }

    #[test]
    fn conditional_treats_empty_and_zero_as_false() {
        let pane = pane();
        let scope = FormatScope {
            pane: Some(&pane),
            pane_index: Some(0),
            pane_active: Some(false),
            ..Default::default()
        };
        // pane_index 是 "0" — tmux 把 "0" 当假；pane_title 非空为真；
        // 缺席的 session_name 是空串，同样为假。
        assert_eq!(
            expand(
                "#{?pane_index,i,-}#{?pane_title,t,-}#{?session_name,s,-}",
                &scope
            )
            .expect("expand"),
            "-t-"
        );
    }

    #[test]
    fn conditional_keeps_commas_in_the_false_branch() {
        let scope = FormatScope::default();
        assert_eq!(
            expand("#{?pane_active,yes,no,really}", &scope).expect("expand"),
            "no,really"
        );
    }

    #[test]
    fn conditional_without_separator_is_an_error() {
        let scope = FormatScope::default();
        let error = expand("#{?pane_active}", &scope).expect_err("missing comma");
        assert!(error.to_string().contains("','"), "{error}");
    }

    #[test]
    fn multibyte_literals_survive_expansion() {
        let session = session();
        let scope = FormatScope {
            session: Some(&session),
            ..Default::default()
        };
        assert_eq!(
            expand("会话 → #{session_name} ✓", &scope).expect("expand"),
            "会话 → dev ✓"
        );
    }
}
