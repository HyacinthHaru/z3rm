// CLI 控制接口
// 来源: spec §3.10 — tmux 兼容的 CLI 命令，让 agent 零学习成本操控 z3rm

pub mod capture;
pub mod dispatch;
pub mod keys;
pub mod marketplace;
pub mod target;

pub use dispatch::CliCommand;
pub use dispatch::run_cli_command;

use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

/// z3rm CLI — tmux 兼容的会话控制工具 (§3.10)
#[derive(Parser, Debug)]
#[command(name = "z3rm", about = "tmux-compatible session control", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Subcommand, Debug)]
enum CliSubcommand {
    /// 列出所有 session
    Ls,
    /// 创建新 session
    New {
        /// Session 名称
        #[arg(short, long)]
        s: Option<String>,
        /// 工作目录
        #[arg(short = 'c', long)]
        cwd: Option<PathBuf>,
    },
    /// 终止 session
    Kill {
        /// 目标 session
        #[arg(short, long, default_value = None)]
        t: Option<String>,
    },
    /// 连接到 session (打印确认信息后立即退出)
    Attach {
        /// 目标 session
        #[arg(short, long, default_value = None)]
        t: Option<String>,
    },
    /// 断开当前 client
    Detach,
    /// 分割 pane
    SplitWindow {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
        /// 水平分割 (左右)
        #[arg(short = 'h', long, action = ArgAction::SetTrue)]
        horizontal: bool,
        /// 垂直分割 (上下) — 默认
        #[arg(short = 'v', long, action = ArgAction::SetTrue)]
        vertical: bool,
        /// 在新 pane 中执行的命令
        #[arg(long)]
        command: Option<String>,
    },
    /// 发送输入到 pane
    SendKeys {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
        /// 按键名 (tmux 风格: Enter, C-c, Up, M-x, 或字面文本)
        keys: Vec<String>,
    },
    /// 捕获 pane 内容
    CapturePane {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
        /// 直接输出到 stdout (无额外换行)
        #[arg(short, long, action = ArgAction::SetTrue)]
        print: bool,
        /// 包含 scrollback (负值 = 行数)
        #[arg(short = 'S', long, value_parser = parse_i32)]
        scrollback: Option<i32>,
        /// 保留 ANSI 转义码
        #[arg(short = 'e', long, action = ArgAction::SetTrue)]
        escape: bool,
    },
    /// 列出 session 中的 pane
    ListPanes {
        /// 目标 session
        #[arg(short, long, default_value = None)]
        t: Option<String>,
    },
    /// 聚焦 pane
    SelectPane {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
    },
    /// 关闭 pane
    KillPane {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
    },
    /// 调整 pane 大小
    ResizePane {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
        /// 宽度 (列数)
        #[arg(short = 'x', long)]
        width: Option<u16>,
        /// 高度 (行数)
        #[arg(short = 'y', long)]
        height: Option<u16>,
    },
    /// 创建新 tab (tmux 的 new-window)
    NewWindow {
        /// 目标 session
        #[arg(short, long, default_value = None)]
        t: Option<String>,
    },
    /// 设置 pane 标题 (tmux 的 rename-window)
    RenameWindow {
        /// 目标 pane
        #[arg(short, long, default_value = None)]
        t: Option<String>,
        /// 新标题
        title: String,
    },
}

fn parse_i32(s: &str) -> Result<i32, String> {
    s.parse().map_err(|e| format!("invalid integer '{}': {}", s, e))
}

/// 解析命令行参数, 返回 CLI 命令或 None (表示 GUI 模式)
pub fn parse_cli_args() -> Option<CliCommand> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        return None;
    }

    // 第一个参数是程序名, 第二个是子命令
    let subcommand = &args[1];
    match subcommand.as_str() {
        "ls" => Some(CliCommand::ListSessions),

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
            Some(CliCommand::NewSession { name, cwd })
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
                Some(t) => Some(CliCommand::KillSession { target: t }),
                None => {
                    eprintln!("error: kill requires -t <target>");
                    None
                }
            }
        }

        "attach" => {
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
            Some(CliCommand::Attach { target })
        }

        "detach" => Some(CliCommand::Detach),

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
            Some(CliCommand::SplitWindow {
                target,
                horizontal,
                command,
            })
        }

        "send-keys" => {
            let mut target = None;
            let mut keys = Vec::new();
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
                        keys.push(rest[i].clone());
                        i += 1;
                    }
                }
            }
            if keys.is_empty() {
                eprintln!("error: send-keys requires at least one key");
                None
            } else {
                Some(CliCommand::SendKeys { target, keys })
            }
        }

        "capture-pane" => {
            let mut target = None;
            let mut print = false;
            let mut scrollback = None;
            let mut escape = false;
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
                    "-p" | "--print" => {
                        print = true;
                        i += 1;
                    }
                    "-S" | "--scrollback" => {
                        if i + 1 < rest.len() {
                            if let Ok(n) = rest[i + 1].parse::<i32>() {
                                scrollback = Some(n);
                            }
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    "-e" | "--escape" => {
                        escape = true;
                        i += 1;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }
            Some(CliCommand::CapturePane {
                target,
                print,
                scrollback,
                escape,
            })
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
            Some(CliCommand::ListPanes { target })
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
            Some(CliCommand::SelectPane { target })
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
            Some(CliCommand::KillPane { target })
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
                            if let Ok(n) = rest[i + 1].parse::<u16>() {
                                width = Some(n);
                            }
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    "-y" | "--height" => {
                        if i + 1 < rest.len() {
                            if let Ok(n) = rest[i + 1].parse::<u16>() {
                                height = Some(n);
                            }
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
            Some(CliCommand::ResizePane {
                target,
                width,
                height,
            })
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
            Some(CliCommand::NewWindow { target })
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
                eprintln!("error: rename-window requires a title");
                return None;
            };
            Some(CliCommand::RenameWindow { target, title })
        }

        _ => None,
    }
}
