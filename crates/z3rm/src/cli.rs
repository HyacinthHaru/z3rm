// CLI 控制接口
// 来源: spec §3.10 — tmux 兼容的 CLI 命令，让 agent 零学习成本操控 z3rm

pub mod capture;
pub mod dispatch;
pub mod keys;
pub mod marketplace;
pub mod target;

pub use dispatch::CliCommand;
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
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-t" | "--target" => {
                if i + 1 >= rest.len() {
                    // `attach -t` 缺值交给 CLI 解析层报错；GUI 意图跳过。
                    return None;
                }
                target = Some(rest[i + 1].clone());
                i += 2;
            }
            "--ssh" => return None,
            _ => i += 1,
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
    ls                              list all sessions\n\
    new -s <name> [-c <cwd>]         create a new session\n\
    kill -t <target>                 terminate a session\n\
    kill-server                      gracefully shut down mux_server\n\
    attach [-t <target>]             attach to a session (opens GUI)\n\
    attach --ssh <ssh://uri>         connect via SSH tunnel to remote mux_server\n\
    detach                           detach the current client\n\
    recover [--list | -t <session>]  list or confirm persisted session recovery\n\
    split-window [-t <target>] [-h|-v] [-c <command>]\n\
                                     split the active pane\n\
    send-keys -t <target> <keys...>  send input to a pane\n\
    capture-pane [-t <target>] [-p] [-S <-N>] [-e]\n\
                                     capture pane contents\n\
    list-panes [-t <session>]        list panes in a session\n\
    select-pane -t <target>          focus a pane\n\
    kill-pane -t <target>            close a pane\n\
    resize-pane [-t <target>] -x <W> -y <H>\n\
                                     resize a pane\n\
    new-window [-t <session>]         create a new tab\n\
    rename-window -t <target> <title> set the pane title\n\
\n\
aliases: list-sessions = ls, kill-session = kill\n\
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
        _ => {}
    }

    match normalized[1].as_str() {
        "kill" if !has_option_value(&normalized[2..], "-t", "--target") => {
            return Err("kill requires -t <target>".to_string());
        }
        "send-keys" if send_keys_payload_is_empty(&normalized[2..]) => {
            return Err("send-keys requires at least one key".to_string());
        }
        "rename-window" if rename_window_title(&normalized[2..]).is_none() => {
            return Err("rename-window requires a title".to_string());
        }
        command if is_mux_cli_command(command) => {}
        command => return Err(format!("unknown CLI command: {command}")),
    }
    parse_cli_args_lossy(&normalized)
}

fn has_option_value(args: &[String], short: &str, long: &str) -> bool {
    args.windows(2)
        .any(|window| (window[0] == short || window[0] == long) && !window[1].starts_with('-'))
}

fn send_keys_payload_is_empty(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-t" | "--target" => i += 2,
            _ => return false,
        }
    }
    true
}

fn rename_window_title(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-t" | "--target" => i += 2,
            _ => return Some(&args[i]),
        }
    }
    None
}

fn is_mux_cli_command(command: &str) -> bool {
    matches!(
        command,
        "ls" | "new"
            | "kill"
            | "kill-server"
            | "attach"
            | "detach"
            | "recover"
            | "split-window"
            | "send-keys"
            | "capture-pane"
            | "list-panes"
            | "select-pane"
            | "kill-pane"
            | "resize-pane"
            | "new-window"
            | "rename-window"
    )
}

fn parse_cli_args_lossy(args: &[String]) -> Result<Option<CliCommand>, String> {
    if args.len() <= 1 {
        return Ok(None);
    }

    // 第一个参数是程序名, 第二个是子命令
    let subcommand = &args[1];
    match subcommand.as_str() {
        "ls" => Ok(Some(CliCommand::ListSessions)),

        "new" => {
            let mut name = None;
            let mut cwd = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-s" | "--session-name" => {
                        if i + 1 < rest.len() {
                            name = Some(rest[i + 1].clone());
                        }
                    }
                    "-c" | "--cwd" => {
                        if i + 1 < rest.len() {
                            cwd = Some(PathBuf::from(&rest[i + 1]));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(CliCommand::NewSession { name, cwd }))
        }

        "kill" => {
            let mut target = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            match target {
                Some(t) => Ok(Some(CliCommand::KillSession { target: t })),
                None => Err("kill requires -t <target>".to_string()),
            }
        }

        "kill-server" => Ok(Some(CliCommand::KillServer)),

        "attach" => {
            let mut target = None;
            let mut ssh_uri = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    "--ssh" => {
                        if i + 1 < rest.len() {
                            ssh_uri = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            if let Some(uri) = ssh_uri {
                Ok(Some(CliCommand::Ssh { target: uri }))
            } else {
                Ok(Some(CliCommand::Attach { target }))
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
            let mut target = None;
            let mut horizontal = false;
            let mut command = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    "-h" | "--horizontal" => horizontal = true,
                    "-v" | "--vertical" => {} // 默认就是垂直, 不需要处理
                    "-c" | "--command" => {
                        if i + 1 < rest.len() {
                            command = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(CliCommand::SplitWindow {
                target,
                horizontal,
                command,
            }))
        }

        "send-keys" => {
            let mut target = None;
            let mut keys = Vec::new();
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
                Ok(Some(CliCommand::SendKeys { target, keys }))
            }
        }

        "capture-pane" => {
            let mut target = None;
            let mut print = false;
            let mut scrollback = None;
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
                    "-S" | "--scrollback" => {
                        let value = rest
                            .get(index + 1)
                            .ok_or_else(|| "capture-pane requires a value for -S".to_string())?;
                        let lines = value
                            .parse::<i32>()
                            .map_err(|_| format!("invalid integer for -S: '{value}'"))?;
                        if lines >= 0 {
                            return Err(
                                "capture-pane -S requires a negative line count".to_string()
                            );
                        }
                        scrollback = Some(lines);
                        index += 2;
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
                scrollback,
                escape,
            }))
        }

        "list-panes" => {
            let mut target = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(CliCommand::ListPanes { target }))
        }

        "select-pane" => {
            let mut target = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(CliCommand::SelectPane { target }))
        }

        "kill-pane" => {
            let mut target = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(CliCommand::KillPane { target }))
        }

        "resize-pane" => {
            let mut target = None;
            let mut width = None;
            let mut height = None;
            let rest = &args[2..];
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    "-x" | "--width" => {
                        if i + 1 < rest.len() {
                            let n = rest[i + 1].parse::<u16>().map_err(|_| {
                                format!("invalid integer for -x: '{}'", rest[i + 1])
                            })?;
                            width = Some(n);
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    "-y" | "--height" => {
                        if i + 1 < rest.len() {
                            let n = rest[i + 1].parse::<u16>().map_err(|_| {
                                format!("invalid integer for -y: '{}'", rest[i + 1])
                            })?;
                            height = Some(n);
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            Ok(Some(CliCommand::ResizePane {
                target,
                width,
                height,
            }))
        }

        "new-window" => {
            let mut target = None;
            let rest = &args[2..];
            for i in 0..rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(CliCommand::NewWindow { target }))
        }

        "rename-window" => {
            let mut target = None;
            let rest = &args[2..];
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-t" | "--target" => {
                        if i + 1 < rest.len() {
                            target = Some(rest[i + 1].clone());
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    _ => {
                        // 剩余第一个非 flag 参数是 title
                        break;
                    }
                }
            }
            let title = if i < rest.len() {
                rest[i].clone()
            } else {
                return Err("rename-window requires a title".to_string());
            };
            Ok(Some(CliCommand::RenameWindow { target, title }))
        }

        _ => unreachable!(),
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
        assert!(matches!(parsed, Some(CliCommand::ListSessions)));
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
    fn capture_pane_rejects_missing_nonnegative_and_unknown_options() {
        for arguments in [
            vec!["capture-pane", "-S"],
            vec!["capture-pane", "-S", "0"],
            vec!["capture-pane", "-S", "2"],
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
    fn send_keys_rejects_options_but_allows_literal_hyphen_after_terminator() {
        assert!(parse_cli_args_from(&args(&["send-keys", "-l", "hello"])).is_err());
        assert!(parse_cli_args_from(&args(&["send-keys", "-t"])).is_err());

        let parsed = parse_cli_args_from(&args(&["send-keys", "--", "-literal"]))
            .expect("parse literal key after option terminator");
        match parsed {
            Some(CliCommand::SendKeys { target, keys }) => {
                assert_eq!(target, None);
                assert_eq!(keys, vec!["-literal"]);
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
}
