// CLI 控制接口
// 来源: spec §3.10 — tmux 兼容的 CLI 命令，让 agent 零学习成本操控 z3rm

pub mod capture;
pub mod dispatch;
pub mod format;
pub mod keys;
pub mod marketplace;
pub mod target;

pub use capture::CaptureLine;
pub use dispatch::CliCommand;
pub use dispatch::SendKeysEncoding;
pub use dispatch::run_cli_command;

use std::path::PathBuf;

/// z3rm 启动意图 — GUI 模式 vs RPC 模式。
///
/// `z3rm attach [-t target]`（不带 `--ssh`）不再走 RPC attach 然后 exit(0)，
/// 而是标记为 GUI 启动意图：main.rs 不会在此处 `exit(0)`，而把目标 session 名字
/// /ID 推入进程环境，进入与 GUI 启动相同的 daemon 流程。`attach --ssh` 必须保持
/// 原行为（建立 SSH 隧道后退出），不属于 GUI 意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchIntent {
    /// Launch GUI and attach the requested session.
    /// `target` 是原始 target 字符串（如 `"dev"`、`"dev:0.1"`），由运行时解析；
    /// `None` 表示 "默认/最近使用的 session"。
    Gui { target: Option<String> },
}

/// 询问 `argv` 是否表达 GUI 启动意图。
/// 目前仅识别 `attach [-t target]`；`attach --ssh ...` 仍走 CLI 短路。
pub fn parse_launch_intent_from(args: &[String]) -> Option<LaunchIntent> {
    if args.len() < 2 || args[1] != "attach" {
        return None;
    }
    let rest = &args[2..];
    let mut target: Option<String> = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "-t" | "--target" => {
                // `attach -t` 缺值或多余参数都交给 CLI 解析层报错，GUI 意图不抢先消费。
                let value = rest.get(index + 1)?;
                target = Some(value.clone());
                index += 2;
            }
            // `attach --ssh ...` 仍走 CLI 短路；缺值的 --ssh 也交给 CLI 报错。
            "--ssh" => return None,
            // 未知 flag / 位置参数让 CLI 解析层报错，而不是静默退回默认 session。
            _ => return None,
        }
    }
    Some(LaunchIntent::Gui { target })
}

pub fn parse_launch_intent() -> Option<LaunchIntent> {
    parse_launch_intent_from(&std::env::args().collect::<Vec<_>>())
}

/// 解析命令行参数，返回 CLI 命令或 None (表示 GUI/extension 模式)。
pub fn parse_cli_args() -> Result<Option<CliCommand>, String> {
    let args: Vec<String> = std::env::args().collect();
    parse_cli_args_from(&args)
}

/// Sentinel error message returned when `--help` or `help` is requested.
/// The main function uses this URL-constant to distinguish a help request
/// from a real parse error and writes usage to stdout + exit(0).
pub const HELP_REQUESTED: &str = "usage: z3rm <command> [args]";

/// Return the full usage summary (spec §3.10 command table).
pub fn format_usage() -> String {
    format!(
        "usage: z3rm <command> [args]\n\
\n\
commands (spec §3.10):\n\
    ls [-F <format>]                 list all sessions\n\
    new -s <name> [-c <cwd>]         create a new session\n\
    kill -t <target>                 terminate a session\n\
    rename-session [-t <target>] <name>\n\
                                     rename a session\n\
    has-session -t <target>          exit 0 if the session exists\n\
    kill-server                      gracefully shut down mux_server\n\
    attach [-t <target>]             attach to a session (opens GUI)\n\
    attach --ssh <ssh://uri>         connect via SSH tunnel to remote mux_server\n\
    detach                           detach the current client\n\
    recover [--list | -t <session>]  list or confirm persisted session recovery\n\
    split-window [-t <target>] [-h|-v] [-c <command>]\n\
                                     split the active pane\n\
    send-keys -t <target> [-l|-H] [-N <count>] <keys...>\n\
                                     send input to a pane\n\
    paste-buffer [-t <target>]       paste stdin into a pane\n\
    capture-pane [-t <target>] [-p] [-S <line>] [-E <line>] [-J] [-e]\n\
                                     capture pane contents\n\
    list-panes [-t <session>] [-F <format>]\n\
                                     list panes in a session\n\
    list-windows [-t <session>] [-F <format>]\n\
                                     list windows in a session\n\
    select-pane -t <target>          focus a pane\n\
    kill-pane -t <target>            close a pane\n\
    resize-pane [-t <target>] [-x <W>] [-y <H>] [-Z]\n\
                                     resize a pane, or toggle zoom with -Z (requires -x, -y, or -Z)\n\
    new-window [-t <session>]         create a new tab\n\
    rename-window -t <target> <title> set the pane title\n\
\n\
capture-pane line numbers follow tmux: 0 is the first visible row, negative\n\
numbers reach into history, and '-' is the far edge (history start for -S,\n\
visible end for -E).\n\
\n\
-F format strings use tmux syntax: '#{{name}}' substitutes a variable,\n\
'#{{?name,yes,no}}' branches on it, and '##' is a literal '#'. Unknown or\n\
out-of-scope variables expand to an empty string. Supported variables:\n\
    session_id session_name session_path session_attached session_created\n\
    session_windows window_id window_index window_name window_active\n\
    window_panes pane_id pane_index pane_title pane_active pane_width\n\
    pane_height pane_current_path pane_start_command pane_dead\n\
\n\
aliases: list-sessions = ls, kill-session = kill, list-windows = lsw\n\
run 'z3rm extension --help' for marketplace commands\n"
    )
}

pub fn parse_cli_args_from(args: &[String]) -> Result<Option<CliCommand>, String> {
    if args.len() <= 1 {
        return Ok(None);
    }

    if args[1] == "extension" {
        return Ok(None);
    }

    // --help / help: return a sentinel error that main.rs detects and exits 0
    if args[1] == "--help" || args[1] == "help" {
        return Err(HELP_REQUESTED.to_string());
    }

    let mut normalized = args.to_vec();
    match normalized[1].as_str() {
        "list-sessions" => normalized[1] = "ls".to_string(),
        "kill-session" => normalized[1] = "kill".to_string(),
        "lsw" => normalized[1] = "list-windows".to_string(),
        _ => {}
    }

    match normalized[1].as_str() {
        command if is_mux_cli_command(command) => {}
        command => return Err(format!("unknown CLI command: {command}")),
    }
    parse_cli_args_lossy(&normalized)
}

/// 严格解析只接受 `specs` 中声明的选项的命令。未知 flag、多余位置参数、
/// 缺失的选项值一律报错，不会静默忽略。
///
/// 返回 `短名 -> 值` 的映射；布尔 flag 的值为 `None`。
fn parse_strict_options(
    command: &str,
    args: &[String],
    specs: &[OptionSpec],
) -> Result<std::collections::HashMap<&'static str, Option<String>>, String> {
    let mut values: std::collections::HashMap<&'static str, Option<String>> =
        std::collections::HashMap::new();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let Some(spec) = specs
            .iter()
            .find(|spec| argument == spec.short || argument == spec.long)
        else {
            return Err(format!("unsupported {command} option: {argument}"));
        };
        if spec.takes_value {
            let value = args
                .get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| format!("{command} requires a value for {argument}"))?;
            values.insert(spec.short, Some(value.clone()));
            index += 2;
        } else {
            values.insert(spec.short, None);
            index += 1;
        }
    }
    Ok(values)
}

/// 取严格选项解析结果里某个带值选项的值。
fn option_value<'a>(
    values: &'a std::collections::HashMap<&'static str, Option<String>>,
    short: &str,
) -> Option<&'a str> {
    values.get(short).and_then(|value| value.as_deref())
}

/// 单个选项的声明: 短名、长名、是否带值。
struct OptionSpec {
    short: &'static str,
    long: &'static str,
    takes_value: bool,
}

impl OptionSpec {
    const fn value(short: &'static str, long: &'static str) -> Self {
        Self {
            short,
            long,
            takes_value: true,
        }
    }

    const fn flag(short: &'static str, long: &'static str) -> Self {
        Self {
            short,
            long,
            takes_value: false,
        }
    }
}

fn is_mux_cli_command(command: &str) -> bool {
    matches!(
        command,
        "ls" | "new"
            | "kill"
            | "rename-session"
            | "has-session"
            | "kill-server"
            | "attach"
            | "detach"
            | "recover"
            | "split-window"
            | "send-keys"
            | "paste-buffer"
            | "capture-pane"
            | "list-panes"
            | "list-windows"
            | "select-pane"
            | "kill-pane"
            | "resize-pane"
            | "new-window"
            | "rename-window"
    )
}

/// 解析 `-t <target>` / `-F <format>` 这类"只有可选 target + 可选 format"的
/// 列表命令。未知 flag 一律报错，避免拼错的选项被静默忽略。
fn parse_list_options(
    command: &str,
    args: &[String],
) -> Result<(Option<String>, Option<String>), String> {
    let mut target = None;
    let mut format = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-t" | "--target" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| format!("{command} requires a value for -t"))?;
                target = Some(value.clone());
                index += 2;
            }
            // 格式串是任意文本 (可能以 `-` 开头), 只要存在就照单全收。
            "-F" | "--format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{command} requires a value for -F"))?;
                format = Some(value.clone());
                index += 2;
            }
            option => return Err(format!("unsupported {command} option: {option}")),
        }
    }
    Ok((target, format))
}

/// 解析 capture-pane 的 `-S` / `-E` 行号。
///
/// tmux 行号模型: `0` 是可见区第一行，负数进入历史，字面量 `-` 是这一侧的
/// 极端边界 (`-S -` 取到历史最开头，`-E -` 取到可见区最后一行)。
fn parse_capture_line(value: Option<&String>, flag: &str) -> Result<CaptureLine, String> {
    let value = value.ok_or_else(|| format!("capture-pane requires a value for {flag}"))?;
    if value == "-" {
        return Ok(CaptureLine::Edge);
    }
    value
        .parse::<i32>()
        .map(CaptureLine::Line)
        .map_err(|_| format!("invalid integer for {flag}: '{value}'"))
}

/// 解析 `[-t <target>] [<positional>]` 形式的命令。
fn parse_target_and_positional(
    command: &str,
    args: &[String],
) -> Result<(Option<String>, Option<String>), String> {
    let mut target = None;
    let mut positional: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-t" | "--target" => {
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| format!("{command} requires a value for -t"))?;
                target = Some(value.clone());
                index += 2;
            }
            option if option.starts_with('-') => {
                return Err(format!("unsupported {command} option: {option}"));
            }
            value => {
                if positional.is_some() {
                    return Err(format!(
                        "{command} takes a single argument; quote it if it contains spaces"
                    ));
                }
                positional = Some(value.to_string());
                index += 1;
            }
        }
    }
    Ok((target, positional))
}

fn parse_cli_args_lossy(args: &[String]) -> Result<Option<CliCommand>, String> {
    if args.len() <= 1 {
        return Ok(None);
    }

    // 第一个参数是程序名, 第二个是子命令
    let subcommand = &args[1];
    match subcommand.as_str() {
        "ls" => {
            let (target, format) = parse_list_options("ls", &args[2..])?;
            if target.is_some() {
                return Err("ls does not take -t".to_string());
            }
            Ok(Some(CliCommand::ListSessions { format }))
        }

        "new" => {
            let values = parse_strict_options(
                "new",
                &args[2..],
                &[
                    OptionSpec::value("-s", "--session-name"),
                    OptionSpec::value("-c", "--cwd"),
                ],
            )?;
            if option_value(&values, "-s") == Some("") {
                return Err("new requires a non-empty session name".to_string());
            }
            Ok(Some(CliCommand::NewSession {
                name: option_value(&values, "-s").map(str::to_string),
                cwd: option_value(&values, "-c").map(PathBuf::from),
            }))
        }

        "kill" => {
            let values =
                parse_strict_options("kill", &args[2..], &[OptionSpec::value("-t", "--target")])?;
            match option_value(&values, "-t") {
                Some(target) => Ok(Some(CliCommand::KillSession {
                    target: target.to_string(),
                })),
                None => Err("kill requires -t <target>".to_string()),
            }
        }

        "rename-session" => {
            let rest = &args[2..];
            let (target, name) = parse_target_and_positional("rename-session", rest)?;
            let name = match name {
                Some(name) => name,
                None => return Err("rename-session requires a new session name".to_string()),
            };
            if name.is_empty() {
                return Err("rename-session requires a non-empty session name".to_string());
            }
            Ok(Some(CliCommand::RenameSession { target, name }))
        }

        "has-session" => {
            let (target, positional) = parse_target_and_positional("has-session", &args[2..])?;
            if positional.is_some() {
                return Err("has-session takes only -t <target>".to_string());
            }
            match target {
                Some(target) => Ok(Some(CliCommand::HasSession { target })),
                None => Err("has-session requires -t <target>".to_string()),
            }
        }

        "kill-server" => Ok(Some(CliCommand::KillServer)),

        "attach" => {
            let values = parse_strict_options(
                "attach",
                &args[2..],
                &[
                    OptionSpec::value("-t", "--target"),
                    OptionSpec::value("--ssh", "--ssh"),
                ],
            )?;
            match (option_value(&values, "-t"), option_value(&values, "--ssh")) {
                (Some(_), Some(_)) => {
                    Err("attach accepts either -t <target> or --ssh <uri>, not both".to_string())
                }
                (_, Some(uri)) => Ok(Some(CliCommand::Ssh {
                    target: uri.to_string(),
                })),
                (Some(target), None) => Ok(Some(CliCommand::Attach {
                    target: Some(target.to_string()),
                })),
                (None, None) => Ok(Some(CliCommand::Attach { target: None })),
            }
        }

        "detach" => Ok(Some(CliCommand::Detach)),

        "recover" => {
            let rest = &args[2..];
            let mut target = None;
            let mut list = false;
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--list" => {
                        list = true;
                        index += 1;
                    }
                    "-t" | "--target" => {
                        let value = rest
                            .get(index + 1)
                            .filter(|value| !value.starts_with('-'))
                            .ok_or_else(|| "recover requires a value for -t".to_string())?;
                        target = Some(value.clone());
                        index += 2;
                    }
                    option => return Err(format!("unsupported recover option: {option}")),
                }
            }
            if list && target.is_some() {
                return Err("recover accepts either --list or -t, not both".to_string());
            }
            Ok(Some(CliCommand::Recover { target }))
        }

        "split-window" => {
            let values = parse_strict_options(
                "split-window",
                &args[2..],
                &[
                    OptionSpec::value("-t", "--target"),
                    OptionSpec::flag("-h", "--horizontal"),
                    OptionSpec::flag("-v", "--vertical"),
                    OptionSpec::value("-c", "--command"),
                ],
            )?;
            let horizontal = values.contains_key("-h");
            let vertical = values.contains_key("-v");
            if horizontal && vertical {
                return Err("split-window accepts either -h or -v, not both".to_string());
            }
            Ok(Some(CliCommand::SplitWindow {
                target: option_value(&values, "-t").map(str::to_string),
                horizontal,
                command: option_value(&values, "-c").map(str::to_string),
            }))
        }

        "send-keys" => {
            let mut target = None;
            let mut keys = Vec::new();
            let mut encoding = SendKeysEncoding::KeyNames;
            let mut repeat: u32 = 1;
            let rest = &args[2..];
            let mut index = 0;
            let mut options_done = false;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--" if !options_done => {
                        options_done = true;
                        index += 1;
                    }
                    "-t" | "--target" if !options_done => {
                        let value = rest
                            .get(index + 1)
                            .filter(|value| !value.starts_with('-'))
                            .ok_or_else(|| "send-keys requires a value for -t".to_string())?;
                        target = Some(value.clone());
                        index += 2;
                    }
                    "-l" if !options_done => {
                        encoding = SendKeysEncoding::Literal;
                        index += 1;
                    }
                    "-H" if !options_done => {
                        encoding = SendKeysEncoding::Hex;
                        index += 1;
                    }
                    "-N" if !options_done => {
                        let value = rest
                            .get(index + 1)
                            .ok_or_else(|| "send-keys requires a value for -N".to_string())?;
                        repeat = value
                            .parse::<u32>()
                            .map_err(|_| format!("invalid repeat count for -N: {value}"))?;
                        if repeat == 0 {
                            return Err("send-keys -N requires a count of at least 1".to_string());
                        }
                        index += 2;
                    }
                    option if !options_done && option.starts_with('-') => {
                        return Err(format!("unsupported send-keys option: {option}"));
                    }
                    _ => {
                        keys.push(rest[index].clone());
                        index += 1;
                    }
                }
            }
            if keys.is_empty() {
                Err("send-keys requires at least one key".to_string())
            } else {
                Ok(Some(CliCommand::SendKeys {
                    target,
                    keys,
                    encoding,
                    repeat,
                }))
            }
        }

        "capture-pane" => {
            let mut target = None;
            let mut print = false;
            let mut start = None;
            let mut end = None;
            let mut join_wrapped = false;
            let mut escape = false;
            let rest = &args[2..];
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "-t" | "--target" => {
                        let value = rest
                            .get(index + 1)
                            .filter(|value| !value.starts_with('-'))
                            .ok_or_else(|| "capture-pane requires a value for -t".to_string())?;
                        target = Some(value.clone());
                        index += 2;
                    }
                    "-p" | "--print" => {
                        print = true;
                        index += 1;
                    }
                    "-S" | "--start-line" | "--scrollback" => {
                        start = Some(parse_capture_line(rest.get(index + 1), "-S")?);
                        index += 2;
                    }
                    "-E" | "--end-line" => {
                        end = Some(parse_capture_line(rest.get(index + 1), "-E")?);
                        index += 2;
                    }
                    "-J" | "--join" => {
                        join_wrapped = true;
                        index += 1;
                    }
                    "-e" | "--escape" => {
                        escape = true;
                        index += 1;
                    }
                    option => return Err(format!("unsupported capture-pane option: {option}")),
                }
            }
            Ok(Some(CliCommand::CapturePane {
                target,
                print,
                start,
                end,
                join_wrapped,
                escape,
            }))
        }

        "list-panes" => {
            let (target, format) = parse_list_options("list-panes", &args[2..])?;
            Ok(Some(CliCommand::ListPanes { target, format }))
        }

        "list-windows" => {
            let (target, format) = parse_list_options("list-windows", &args[2..])?;
            Ok(Some(CliCommand::ListWindows { target, format }))
        }

        "paste-buffer" => {
            let (target, positional) = parse_target_and_positional("paste-buffer", &args[2..])?;
            if positional.is_some() {
                return Err(
                    "paste-buffer reads the buffer from stdin and takes only -t <target>"
                        .to_string(),
                );
            }
            Ok(Some(CliCommand::PasteBuffer { target }))
        }

        "select-pane" => {
            let values = parse_strict_options(
                "select-pane",
                &args[2..],
                &[OptionSpec::value("-t", "--target")],
            )?;
            Ok(Some(CliCommand::SelectPane {
                target: option_value(&values, "-t").map(str::to_string),
            }))
        }

        "kill-pane" => {
            let values = parse_strict_options(
                "kill-pane",
                &args[2..],
                &[OptionSpec::value("-t", "--target")],
            )?;
            Ok(Some(CliCommand::KillPane {
                target: option_value(&values, "-t").map(str::to_string),
            }))
        }

        "resize-pane" => {
            let values = parse_strict_options(
                "resize-pane",
                &args[2..],
                &[
                    OptionSpec::value("-t", "--target"),
                    OptionSpec::value("-x", "--width"),
                    OptionSpec::value("-y", "--height"),
                    OptionSpec::flag("-Z", "--zoom"),
                ],
            )?;
            let parse_dimension = |flag: &str| -> Result<Option<u16>, String> {
                match option_value(&values, flag) {
                    Some(value) => value
                        .parse::<u16>()
                        .map(Some)
                        .map_err(|_| format!("invalid integer for {flag}: '{value}'")),
                    None => Ok(None),
                }
            };
            let width = parse_dimension("-x")?;
            let height = parse_dimension("-y")?;
            let zoom = values.contains_key("-Z");
            if zoom && (width.is_some() || height.is_some()) {
                return Err(
                    "resize-pane -Z toggles zoom and cannot be combined with -x/-y".to_string(),
                );
            }
            if !zoom && width.is_none() && height.is_none() {
                return Err("resize-pane requires -x <width>, -y <height>, or -Z".to_string());
            }
            Ok(Some(CliCommand::ResizePane {
                target: option_value(&values, "-t").map(str::to_string),
                width,
                height,
                zoom,
            }))
        }

        "new-window" => {
            let values = parse_strict_options(
                "new-window",
                &args[2..],
                &[OptionSpec::value("-t", "--target")],
            )?;
            Ok(Some(CliCommand::NewWindow {
                target: option_value(&values, "-t").map(str::to_string),
            }))
        }

        "rename-window" => {
            let (target, title) = parse_target_and_positional("rename-window", &args[2..])?;
            let title = match title {
                Some(title) => title,
                None => return Err("rename-window requires a title".to_string()),
            };
            if title.is_empty() {
                return Err("rename-window requires a non-empty title".to_string());
            }
            Ok(Some(CliCommand::RenameWindow { target, title }))
        }

        _ => Err(format!("unknown subcommand: {subcommand}")),
    }
}

mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        std::iter::once("z3rm".to_string())
            .chain(parts.iter().map(|part| (*part).to_string()))
            .collect()
    }

    #[test]
    fn list_sessions_alias_is_accepted() {
        let parsed = parse_cli_args_from(&args(&["list-sessions"])).expect("parse");
        assert!(matches!(
            parsed,
            Some(CliCommand::ListSessions { format: None })
        ));
    }

    #[test]
    fn list_windows_alias_and_format_are_accepted() {
        let parsed = parse_cli_args_from(&args(&["lsw", "-t", "dev", "-F", "#{window_name}"]))
            .expect("parse");
        match parsed {
            Some(CliCommand::ListWindows { target, format }) => {
                assert_eq!(target.as_deref(), Some("dev"));
                assert_eq!(format.as_deref(), Some("#{window_name}"));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        let parsed = parse_cli_args_from(&args(&["list-windows"])).expect("parse");
        assert!(matches!(
            parsed,
            Some(CliCommand::ListWindows {
                target: None,
                format: None
            })
        ));
    }

    #[test]
    fn list_commands_accept_format_and_reject_unknown_options() {
        let parsed = parse_cli_args_from(&args(&["ls", "-F", "#{session_name}"])).expect("parse");
        match parsed {
            Some(CliCommand::ListSessions { format }) => {
                assert_eq!(format.as_deref(), Some("#{session_name}"))
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        let parsed = parse_cli_args_from(&args(&["list-panes", "-t", "dev", "-F", "#{pane_id}"]))
            .expect("parse");
        match parsed {
            Some(CliCommand::ListPanes { target, format }) => {
                assert_eq!(target.as_deref(), Some("dev"));
                assert_eq!(format.as_deref(), Some("#{pane_id}"));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        for arguments in [
            vec!["ls", "-t", "dev"],
            vec!["ls", "-F"],
            vec!["ls", "--bogus"],
            vec!["list-panes", "-t"],
            vec!["list-panes", "--bogus"],
            vec!["list-windows", "-F"],
        ] {
            assert!(
                parse_cli_args_from(&args(&arguments)).is_err(),
                "arguments {arguments:?} must be rejected"
            );
        }
    }

    #[test]
    fn rename_session_requires_a_name() {
        let parsed =
            parse_cli_args_from(&args(&["rename-session", "-t", "dev", "prod"])).expect("parse");
        match parsed {
            Some(CliCommand::RenameSession { target, name }) => {
                assert_eq!(target.as_deref(), Some("dev"));
                assert_eq!(name, "prod");
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        let err = parse_cli_args_from(&args(&["rename-session", "-t", "dev"]))
            .expect_err("missing name must be a parse error");
        assert!(err.contains("rename-session requires"), "{err}");
    }

    #[test]
    fn has_session_requires_target_and_rejects_extras() {
        let parsed = parse_cli_args_from(&args(&["has-session", "-t", "dev"])).expect("parse");
        match parsed {
            Some(CliCommand::HasSession { target }) => assert_eq!(target, "dev"),
            other => panic!("unexpected parse result: {other:?}"),
        }

        assert!(parse_cli_args_from(&args(&["has-session"])).is_err());
        assert!(parse_cli_args_from(&args(&["has-session", "dev"])).is_err());
    }

    #[test]
    fn paste_buffer_takes_only_a_target() {
        let parsed = parse_cli_args_from(&args(&["paste-buffer", "-t", "dev"])).expect("parse");
        match parsed {
            Some(CliCommand::PasteBuffer { target }) => assert_eq!(target.as_deref(), Some("dev")),
            other => panic!("unexpected parse result: {other:?}"),
        }

        assert!(parse_cli_args_from(&args(&["paste-buffer", "some-text"])).is_err());
    }

    #[test]
    fn resize_pane_zoom_flag_is_exclusive() {
        let parsed =
            parse_cli_args_from(&args(&["resize-pane", "-t", "dev", "-Z"])).expect("parse");
        match parsed {
            Some(CliCommand::ResizePane {
                target,
                width,
                height,
                zoom,
            }) => {
                assert_eq!(target.as_deref(), Some("dev"));
                assert_eq!(width, None);
                assert_eq!(height, None);
                assert!(zoom);
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        assert!(parse_cli_args_from(&args(&["resize-pane", "-Z", "-x", "80"])).is_err());
    }

    #[test]
    fn kill_session_alias_is_accepted() {
        let parsed = parse_cli_args_from(&args(&["kill-session", "-t", "dev"])).expect("parse");
        match parsed {
            Some(CliCommand::KillSession { target }) => assert_eq!(target, "dev"),
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn kill_without_target_is_parse_error() {
        let err = parse_cli_args_from(&args(&["kill"]))
            .expect_err("kill without -t must be a parse error, not GUI mode");
        assert!(err.contains("kill requires"), "{err}");
    }

    #[test]
    fn send_keys_without_keys_is_parse_error() {
        let err = parse_cli_args_from(&args(&["send-keys", "-t", "dev"]))
            .expect_err("send-keys without keys must be a parse error");
        assert!(err.contains("send-keys requires"), "{err}");
    }

    #[test]
    fn capture_pane_bad_scrollback_returns_error() {
        let err = parse_cli_args_from(&args(&["capture-pane", "-S", "abc"]))
            .expect_err("-S with non-integer must be a parse error");
        assert!(err.contains("-S"), "error should name the flag: {err}");

        assert!(err.contains("abc"), "error should show bad value: {err}");
    }

    #[test]
    fn capture_pane_rejects_missing_values_and_unknown_options() {
        for arguments in [
            vec!["capture-pane", "-S"],
            vec!["capture-pane", "-E"],
            vec!["capture-pane", "-t"],
            vec!["capture-pane", "--bogus"],
        ] {
            assert!(
                parse_cli_args_from(&args(&arguments)).is_err(),
                "arguments {arguments:?} must be rejected"
            );
        }
    }

    #[test]
    fn capture_pane_parses_tmux_line_numbers_and_join() {
        // 负数进入历史 (原有语义), 非负是可见区行号, `-` 是这一侧的边界。
        let parsed = parse_cli_args_from(&args(&[
            "capture-pane",
            "-S",
            "-100",
            "-E",
            "-",
            "-J",
            "-p",
        ]))
        .expect("parse");
        match parsed {
            Some(CliCommand::CapturePane {
                start,
                end,
                join_wrapped,
                print,
                escape,
                target,
            }) => {
                assert_eq!(start, Some(CaptureLine::Line(-100)));
                assert_eq!(end, Some(CaptureLine::Edge));
                assert!(join_wrapped);
                assert!(print);
                assert!(!escape);
                assert_eq!(target, None);
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        let parsed =
            parse_cli_args_from(&args(&["capture-pane", "-S", "-", "-E", "5"])).expect("parse");
        match parsed {
            Some(CliCommand::CapturePane { start, end, .. }) => {
                assert_eq!(start, Some(CaptureLine::Edge));
                assert_eq!(end, Some(CaptureLine::Line(5)));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        let parsed = parse_cli_args_from(&args(&["capture-pane", "-S", "0"])).expect("parse");
        match parsed {
            Some(CliCommand::CapturePane { start, .. }) => {
                assert_eq!(start, Some(CaptureLine::Line(0)))
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn send_keys_rejects_options_but_allows_literal_hyphen_after_terminator() {
        assert!(parse_cli_args_from(&args(&["send-keys", "-X", "copy-mode"])).is_err());
        assert!(parse_cli_args_from(&args(&["send-keys", "-t"])).is_err());

        let parsed = parse_cli_args_from(&args(&["send-keys", "--", "-literal"]))
            .expect("parse literal key after option terminator");
        match parsed {
            Some(CliCommand::SendKeys {
                target,
                keys,
                encoding,
                repeat,
            }) => {
                assert_eq!(target, None);
                assert_eq!(keys, vec!["-literal"]);
                assert_eq!(encoding, SendKeysEncoding::KeyNames);
                assert_eq!(repeat, 1);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn recovery_cli_parses_list_and_explicit_confirmation() {
        assert!(matches!(
            parse_cli_args_from(&args(&["recover", "--list"])),
            Ok(Some(CliCommand::Recover { target: None }))
        ));
        assert!(matches!(
            parse_cli_args_from(&args(&["recover", "-t", "session-1"])),
            Ok(Some(CliCommand::Recover { target: Some(target) })) if target == "session-1"
        ));
        assert!(parse_cli_args_from(&args(&["recover", "-t"])).is_err());
        assert!(parse_cli_args_from(&args(&["recover", "--list", "-t", "session-1",])).is_err());
    }

    #[test]
    fn send_keys_literal_hex_and_repeat_are_parsed() {
        // `-l` 必须绕开按键名解析，否则 `Enter` 这个单词会被当成回车。
        let parsed = parse_cli_args_from(&args(&["send-keys", "-l", "Enter"]))
            .expect("parse literal send-keys");
        match parsed {
            Some(CliCommand::SendKeys { keys, encoding, .. }) => {
                assert_eq!(keys, vec!["Enter"]);
                assert_eq!(encoding, SendKeysEncoding::Literal);
            }
            command => panic!("unexpected command: {command:?}"),
        }

        let parsed = parse_cli_args_from(&args(&["send-keys", "-H", "1b", "5b"]))
            .expect("parse hex send-keys");
        match parsed {
            Some(CliCommand::SendKeys { encoding, .. }) => {
                assert_eq!(encoding, SendKeysEncoding::Hex);
            }
            command => panic!("unexpected command: {command:?}"),
        }

        let parsed =
            parse_cli_args_from(&args(&["send-keys", "-N", "3", "a"])).expect("parse repeat count");
        match parsed {
            Some(CliCommand::SendKeys { repeat, .. }) => assert_eq!(repeat, 3),
            command => panic!("unexpected command: {command:?}"),
        }

        assert!(parse_cli_args_from(&args(&["send-keys", "-N", "0", "a"])).is_err());
        assert!(parse_cli_args_from(&args(&["send-keys", "-N", "x", "a"])).is_err());
    }

    #[test]
    fn resize_pane_bad_width_returns_error() {
        let err = parse_cli_args_from(&args(&["resize-pane", "-x", "abc"]))
            .expect_err("-x with non-integer must be a parse error");
        assert!(err.contains("-x"), "error should name the flag: {err}");
    }

    #[test]
    fn resize_pane_bad_height_returns_error() {
        let err = parse_cli_args_from(&args(&["resize-pane", "-y", "xyz"]))
            .expect_err("-y with non-integer must be a parse error");
        assert!(err.contains("-y"), "error should name the flag: {err}");
    }

    #[test]
    fn help_flag_emits_usage() {
        let err =
            parse_cli_args_from(&args(&["--help"])).expect_err("--help must be a handled case");
        assert!(err.contains("usage"), "help should contain usage: {err}");
    }

    #[test]
    fn launch_intent_attach_with_target_is_gui_intent() {
        let intent = parse_launch_intent_from(&args(&["attach", "-t", "dev"]));
        assert_eq!(
            intent,
            Some(LaunchIntent::Gui {
                target: Some("dev".into())
            })
        );
    }

    #[test]
    fn launch_intent_attach_without_target_is_gui_intent() {
        let intent = parse_launch_intent_from(&args(&["attach"]));
        assert_eq!(intent, Some(LaunchIntent::Gui { target: None }));
    }

    #[test]
    fn launch_intent_attach_ssh_is_not_gui_intent() {
        // attach --ssh 必须保留 CLI 短路逻辑，不应被当作 GUI 意图拦截。
        let intent = parse_launch_intent_from(&args(&["attach", "--ssh", "ssh://host"]));
        assert_eq!(intent, None);
    }

    #[test]
    fn launch_intent_non_attach_args_return_none() {
        // 其他命令不属于 GUI 启动意图（继续走 CLI 短路或 GUI 模式）。
        assert_eq!(parse_launch_intent_from(&args(&["ls"])), None);
        assert_eq!(parse_launch_intent_from(&args(&["new", "-s", "x"])), None);
        assert_eq!(parse_launch_intent_from(&args(&[])), None);
        assert_eq!(parse_launch_intent_from(&args(&["--help"])), None);
    }

    #[test]
    fn launch_intent_missing_target_value_returns_none() {
        // `attach -t <empty>` 让 CLI 解析层报错；GUI 意图侧不抢先消费。
        assert_eq!(parse_launch_intent_from(&args(&["attach", "-t"])), None);
    }

    #[test]
    fn launch_intent_accepts_long_target_flag() {
        let intent = parse_launch_intent_from(&args(&["attach", "--target", "dev:0.1"]));
        assert_eq!(
            intent,
            Some(LaunchIntent::Gui {
                target: Some("dev:0.1".into())
            })
        );
    }

    #[test]
    fn attach_cli_command_still_parses_alongside_launch_intent() {
        // parse_cli_args 必须仍能识别 attach（CLI 模式测试不回归）。
        let parsed = parse_cli_args_from(&args(&["attach", "-t", "dev"]))
            .expect("attach should still be a recognized CLI command");
        if let Some(CliCommand::Attach { target }) = &parsed {
            assert_eq!(target.as_deref(), Some("dev"));
        } else {
            panic!("unexpected parse result: {parsed:?}");
        }
    }

    #[test]
    fn help_subcommand_emits_usage() {
        let err = parse_cli_args_from(&args(&["help"]))
            .expect_err("help subcommand must be a handled case");
        assert!(err.contains("usage"), "help should contain usage: {err}");
    }

    #[test]
    fn new_session_without_cwd_yields_none_for_dispatch_to_default() {
        // §3.10 当 -c / --cwd 没传时, 解析器必须保留 cwd=None,
        // 让 dispatch 层使用 std::env::current_dir() 作为工作目录。
        let parsed = parse_cli_args_from(&args(&["new", "-s", "dev"]))
            .expect("new should be a recognized CLI command");
        match parsed {
            Some(CliCommand::NewSession { name, cwd }) => {
                assert_eq!(name.as_deref(), Some("dev"));
                assert!(cwd.is_none(), "cwd must be None when -c is absent");
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn new_session_with_cwd_preserves_explicit_cwd() {
        // §3.10 -c 必须保留, 让 dispatch 直接使用, 不再去 current_dir 推算。
        let parsed =
            parse_cli_args_from(&args(&["new", "-s", "dev", "-c", "/tmp/work"])).expect("parse");
        match parsed {
            Some(CliCommand::NewSession { name, cwd }) => {
                assert_eq!(name.as_deref(), Some("dev"));
                let cwd = cwd.expect("cwd should be Some when -c passed");
                assert_eq!(cwd.to_string_lossy(), "/tmp/work");
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
    }

    #[test]
    fn new_session_accepts_long_cwd_flag() {
        let parsed = parse_cli_args_from(&args(&["new", "--session-name", "dev", "--cwd", "/var"]))
            .expect("parse");
        if let Some(CliCommand::NewSession { name, cwd }) = &parsed {
            assert_eq!(name.as_deref(), Some("dev"));
            let cwd = cwd.as_ref().expect("cwd should be Some when --cwd passed");
            assert_eq!(cwd.to_string_lossy(), "/var");
        } else {
            panic!("unexpected parse result: {parsed:?}");
        }
    }

    #[test]
    fn attach_intent_parses_alongside_cwd_default_preserved() {
        // §3.10 attach 携带 target 时, CLI 仍能诚实地被解析为 CliCommand::Attach,
        // GUI 启动侧另行消费 (parse_launch_intent_from) 决定走 GUI 路径。
        // 这里验证两个解析路径不会互相干扰: LaunchIntent 拿到 target,
        // parse_cli_args_from 也能拿到相同 target。
        let args_vec = args(&["attach", "-t", "dev"]);
        let intent = parse_launch_intent_from(&args_vec);
        let parsed = parse_cli_args_from(&args_vec).expect("parse");
        assert_eq!(
            intent,
            Some(LaunchIntent::Gui {
                target: Some("dev".into())
            })
        );
        if let Some(CliCommand::Attach { target }) = &parsed {
            assert_eq!(target.as_deref(), Some("dev"));
        } else {
            panic!("unexpected parse result: {parsed:?}");
        }
    }

    #[test]
    fn unknown_subcommand_is_rejected_not_panicked() {
        // The subcommand match's default arm used to be `unreachable!()`; if the
        // pre-filter and the match ever drift, that aborts the process. It must
        // be a recoverable parse error instead. Call the lossy parser directly so
        // the default arm is exercised regardless of pre-filtering.
        let err = parse_cli_args_lossy(&args(&["z3rm", "definitely-not-a-command"]))
            .expect_err("unknown subcommand must error, never panic");
        assert!(
            err.contains("unknown subcommand"),
            "error should name the condition: {err}"
        );
    }

    #[test]
    fn new_rejects_unknown_options_and_missing_values() {
        // `new` 无参数合法 (dispatch 生成默认名字); 带参数的必须严格校验。
        assert!(matches!(
            parse_cli_args_from(&args(&["new"])).expect("bare new is valid"),
            Some(CliCommand::NewSession {
                name: None,
                cwd: None
            })
        ));
        let parsed =
            parse_cli_args_from(&args(&["new", "-s", "dev", "-c", "/tmp/work"])).expect("parse");
        match parsed {
            Some(CliCommand::NewSession { name, cwd }) => {
                assert_eq!(name.as_deref(), Some("dev"));
                assert_eq!(
                    cwd.as_ref()
                        .map(|cwd| cwd.to_string_lossy().to_string())
                        .as_deref(),
                    Some("/tmp/work")
                );
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        for arguments in [
            vec!["new", "-s"],
            vec!["new", "-c"],
            vec!["new", "--bogus"],
            vec!["new", "-s", "dev", "extra"],
            vec!["new", "-s", ""],
        ] {
            let error = parse_cli_args_from(&args(&arguments))
                .expect_err("invalid new arguments must be rejected");
            assert!(
                error.contains("new "),
                "error should name the command: {error}"
            );
        }
    }

    #[test]
    fn attach_rejects_invalid_arguments() {
        let parsed = parse_cli_args_from(&args(&["attach", "-t", "dev"])).expect("parse");
        match parsed {
            Some(CliCommand::Attach { target }) => assert_eq!(target.as_deref(), Some("dev")),
            other => panic!("unexpected parse result: {other:?}"),
        }
        let parsed = parse_cli_args_from(&args(&["attach", "--ssh", "ssh://host"]))
            .expect("parse ssh attach");
        assert!(matches!(parsed, Some(CliCommand::Ssh { target }) if target == "ssh://host"));

        for arguments in [
            vec!["attach", "-t"],
            vec!["attach", "--ssh"],
            vec!["attach", "--bogus"],
            vec!["attach", "dev"],
            vec!["attach", "-t", "dev", "--ssh", "ssh://host"],
        ] {
            assert!(
                parse_cli_args_from(&args(&arguments)).is_err(),
                "arguments {arguments:?} must be rejected"
            );
        }
    }

    #[test]
    fn split_window_parses_strictly() {
        let parsed = parse_cli_args_from(&args(&[
            "split-window",
            "-t",
            "dev:0.0",
            "-h",
            "-c",
            "make test",
        ]))
        .expect("parse");
        match parsed {
            Some(CliCommand::SplitWindow {
                target,
                horizontal,
                command,
            }) => {
                assert_eq!(target.as_deref(), Some("dev:0.0"));
                assert!(horizontal);
                assert_eq!(command.as_deref(), Some("make test"));
            }
            other => panic!("unexpected parse result: {other:?}"),
        }
        let parsed = parse_cli_args_from(&args(&["split-window", "-v"])).expect("parse");
        match parsed {
            Some(CliCommand::SplitWindow { horizontal, .. }) => assert!(!horizontal),
            other => panic!("unexpected parse result: {other:?}"),
        }

        for arguments in [
            vec!["split-window", "--bogus"],
            vec!["split-window", "-t"],
            vec!["split-window", "-c"],
            vec!["split-window", "-h", "-v"],
            vec!["split-window", "-t", "-v"],
            vec!["split-window", "extra"],
        ] {
            assert!(
                parse_cli_args_from(&args(&arguments)).is_err(),
                "arguments {arguments:?} must be rejected"
            );
        }
    }

    #[test]
    fn select_and_kill_pane_reject_invalid_arguments() {
        let parsed = parse_cli_args_from(&args(&["select-pane", "-t", "%2"])).expect("parse");
        match parsed {
            Some(CliCommand::SelectPane { target }) => assert_eq!(target.as_deref(), Some("%2")),
            other => panic!("unexpected parse result: {other:?}"),
        }
        let parsed = parse_cli_args_from(&args(&["kill-pane", "-t", "dev:0.1"])).expect("parse");
        match parsed {
            Some(CliCommand::KillPane { target }) => assert_eq!(target.as_deref(), Some("dev:0.1")),
            other => panic!("unexpected parse result: {other:?}"),
        }

        for command in ["select-pane", "kill-pane"] {
            for arguments in [
                vec![command, "--bogus"],
                vec![command, "-t"],
                vec![command, "extra"],
            ] {
                assert!(
                    parse_cli_args_from(&args(&arguments)).is_err(),
                    "arguments {arguments:?} must be rejected"
                );
            }
        }
    }

    #[test]
    fn resize_pane_requires_a_dimension_or_zoom() {
        let parsed =
            parse_cli_args_from(&args(&["resize-pane", "-t", "dev", "-x", "100"])).expect("parse");
        match parsed {
            Some(CliCommand::ResizePane {
                target,
                width,
                height,
                zoom,
            }) => {
                assert_eq!(target.as_deref(), Some("dev"));
                assert_eq!(width, Some(100));
                assert_eq!(height, None);
                assert!(!zoom);
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        // 无 -x/-y/-Z 的空调用过去是发给 daemon 的 no-op resize, 现在直接报错。
        for arguments in [
            vec!["resize-pane"],
            vec!["resize-pane", "-t", "dev"],
            vec!["resize-pane", "--bogus"],
            vec!["resize-pane", "-x"],
            vec!["resize-pane", "-y"],
            vec!["resize-pane", "extra"],
        ] {
            let error = parse_cli_args_from(&args(&arguments))
                .expect_err("invalid resize-pane arguments must be rejected");
            assert!(
                error.contains("resize-pane"),
                "error should name the command: {error}"
            );
        }
    }

    #[test]
    fn new_window_rejects_invalid_arguments() {
        let parsed = parse_cli_args_from(&args(&["new-window", "-t", "dev"])).expect("parse");
        match parsed {
            Some(CliCommand::NewWindow { target }) => assert_eq!(target.as_deref(), Some("dev")),
            other => panic!("unexpected parse result: {other:?}"),
        }

        for arguments in [
            vec!["new-window", "--bogus"],
            vec!["new-window", "-t"],
            vec!["new-window", "extra"],
        ] {
            assert!(
                parse_cli_args_from(&args(&arguments)).is_err(),
                "arguments {arguments:?} must be rejected"
            );
        }
    }

    #[test]
    fn rename_window_rejects_invalid_arguments() {
        let parsed = parse_cli_args_from(&args(&["rename-window", "-t", "dev:0.0", "build"]))
            .expect("parse");
        match parsed {
            Some(CliCommand::RenameWindow { target, title }) => {
                assert_eq!(target.as_deref(), Some("dev:0.0"));
                assert_eq!(title, "build");
            }
            other => panic!("unexpected parse result: {other:?}"),
        }

        for arguments in [
            vec!["rename-window"],
            vec!["rename-window", "-t", "dev"],
            vec!["rename-window", "--bogus"],
            vec!["rename-window", "-t", "dev", "New", "Extra"],
            vec!["rename-window", "-t", "dev", ""],
        ] {
            let error = parse_cli_args_from(&args(&arguments))
                .expect_err("invalid rename-window arguments must be rejected");
            assert!(
                error.contains("rename-window"),
                "error should name the command: {error}"
            );
        }
    }

    #[test]
    fn kill_rejects_unknown_options_and_extras() {
        for arguments in [
            vec!["kill", "--bogus", "-t", "dev"],
            vec!["kill", "-t", "dev", "extra"],
            vec!["kill", "-t"],
        ] {
            assert!(
                parse_cli_args_from(&args(&arguments)).is_err(),
                "arguments {arguments:?} must be rejected"
            );
        }
    }

    #[test]
    fn rename_session_rejects_empty_name() {
        let error = parse_cli_args_from(&args(&["rename-session", "-t", "dev", ""]))
            .expect_err("empty name must be a parse error");
        assert!(error.contains("non-empty"), "{error}");
    }

    #[test]
    fn launch_intent_defers_unknown_arguments_to_cli_parser() {
        // 未知 flag / 多余位置参数不再是"静默 GUI 默认", 交给 CLI 解析层报错。
        assert_eq!(
            parse_launch_intent_from(&args(&["attach", "--bogus"])),
            None
        );
        assert_eq!(parse_launch_intent_from(&args(&["attach", "dev"])), None);
        assert_eq!(
            parse_launch_intent_from(&args(&["attach", "-t", "dev", "extra"])),
            None
        );
        assert_eq!(parse_launch_intent_from(&args(&["attach", "--ssh"])), None);
    }
}
