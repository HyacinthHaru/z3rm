// CLI 命令调度: 连接 daemon, 执行命令, 输出结果
// 来源: spec §3.10

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use mux::MuxDomain;
use mux_protocol::proto::{PaneInfo, ShellCommand, split_node::SplitDirection};

use super::capture::{CaptureLine, CaptureOptions};
use super::format::{FormatScope, expand as expand_format};
use super::keys::parse_keys;
use super::target::Target;

/// 把 send-keys 的参数编码成要写进 PTY 的字节。
///
/// 字面量和十六进制模式绕开按键名解析，否则像 `Enter` 这样的普通单词会被
/// 当成回车发出去。
fn encode_send_keys(keys: &[String], encoding: SendKeysEncoding) -> Result<Vec<u8>> {
    match encoding {
        SendKeysEncoding::KeyNames => Ok(parse_keys(keys)),
        SendKeysEncoding::Literal => Ok(keys.concat().into_bytes()),
        SendKeysEncoding::Hex => keys
            .iter()
            .map(|value| {
                let digits = value.strip_prefix("0x").unwrap_or(value);
                u8::from_str_radix(digits, 16)
                    .with_context(|| format!("invalid hex byte for send-keys -H: {value}"))
            })
            .collect(),
    }
}

/// 把 send-keys 的载荷重复 `repeat` 次。`-N` 可以大到让乘法或 `Vec::repeat`
/// 的容量计算溢出, 这里在分配前用 checked 算术 + 上限拦截, 变成可恢复错误。
fn repeated_payload(bytes: &[u8], repeat: u32) -> Result<Vec<u8>> {
    const MAX_REPEATED_PAYLOAD: usize = 1024 * 1024;
    let payload_len = bytes
        .len()
        .checked_mul(repeat as usize)
        .ok_or_else(|| anyhow::anyhow!("send-keys -N {repeat}: payload size overflow"))?;
    if payload_len > MAX_REPEATED_PAYLOAD {
        anyhow::bail!(
            "send-keys -N {repeat}: payload would be {payload_len} bytes \
             (max {MAX_REPEATED_PAYLOAD})"
        );
    }
    Ok(bytes.repeat(repeat as usize))
}

/// send-keys 载荷的解释方式。
/// 来源: spec §3.10 — 与 tmux 的 `-l` / `-H` 对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendKeysEncoding {
    /// 参数是按键名（`Enter`、`C-c`），未识别的按 UTF-8 字面量发送。
    #[default]
    KeyNames,
    /// `-l`：参数一律按字面文本发送，不做按键名解析。
    Literal,
    /// `-H`：每个参数是一个十六进制字节值。
    Hex,
}

/// CLI 控制命令枚举
/// 来源: spec §3.10 — tmux 兼容的 CLI 命令，让 agent 零学习成本操控 z3rm
#[derive(Debug)]
pub enum CliCommand {
    /// `z3rm ls [-F <format>]` — 列出所有 session
    ListSessions {
        format: Option<String>,
    },
    /// `z3rm new -s <name>` — 创建新 session
    NewSession {
        name: Option<String>,
        cwd: Option<PathBuf>,
    },
    /// `z3rm kill -t <target>` — 终止 session
    KillSession {
        target: String,
    },
    /// `z3rm rename-session [-t <target>] <name>` — 重命名 session
    RenameSession {
        target: Option<String>,
        name: String,
    },
    /// `z3rm has-session -t <target>` — session 存在则退出码 0，否则非 0
    HasSession {
        target: String,
    },
    /// `z3rm kill-server` — 优雅关闭 mux_server (结束所有 session 并退出)
    KillServer,
    /// `z3rm attach -t <target>` — 连接到 session (打开 GUI)
    Attach {
        target: Option<String>,
    },
    /// `z3rm detach` — 断开当前 client
    /// `z3rm attach --ssh <ssh://uri>` — 通过 SSH 隧道连接到远程 mux_server
    Ssh {
        target: String,
    },
    Detach,
    /// `z3rm recover [--list | -t <session>]` — list or explicitly confirm recovery.
    Recover {
        target: Option<String>,
    },
    /// `z3rm split-window -t <target> [-h|-v]` — 分割 pane
    SplitWindow {
        target: Option<String>,
        horizontal: bool,
        command: Option<String>,
    },
    /// `z3rm send-keys -t <target> [-l] [-H] [-N <count>] <keys...>` — 发送输入到 pane
    SendKeys {
        target: Option<String>,
        keys: Vec<String>,
        encoding: SendKeysEncoding,
        repeat: u32,
    },
    /// `z3rm paste-buffer -t <target>` — 把 stdin 的内容粘贴进 pane
    PasteBuffer {
        target: Option<String>,
    },
    /// `z3rm capture-pane -t <target> [-p] [-S <line>] [-E <line>] [-J] [-e]` — 捕获 pane 内容
    CapturePane {
        target: Option<String>,
        print: bool,
        start: Option<CaptureLine>,
        end: Option<CaptureLine>,
        join_wrapped: bool,
        escape: bool,
    },
    /// `z3rm list-panes [-t <target>] [-F <format>]` — 列出 session 中的 pane
    ListPanes {
        target: Option<String>,
        format: Option<String>,
    },
    /// `z3rm list-windows [-t <target>] [-F <format>]` — 列出 session 中的 window
    ListWindows {
        target: Option<String>,
        format: Option<String>,
    },
    /// `z3rm select-pane -t <target>` — 聚焦 pane
    SelectPane {
        target: Option<String>,
    },
    /// `z3rm kill-pane -t <target>` — 关闭 pane
    KillPane {
        target: Option<String>,
    },
    /// `z3rm resize-pane -t <target> [-x <W>] [-y <H>] [-Z]` — 调整 pane 大小或切换 zoom
    ResizePane {
        target: Option<String>,
        width: Option<u16>,
        height: Option<u16>,
        zoom: bool,
    },
    /// `z3rm new-window -t <target>` — 创建新 tab
    NewWindow {
        target: Option<String>,
    },
    /// `z3rm rename-window -t <target> <title>` — 设置 pane 标题
    RenameWindow {
        target: Option<String>,
        title: String,
    },
}

fn current_pane_from_env() -> Option<String> {
    std::env::var("Z3RM_PANE")
        .ok()
        .filter(|pane| !pane.is_empty())
        .or_else(|| {
            std::env::var("Z3RM_PANE_ID")
                .ok()
                .filter(|pane| !pane.is_empty())
        })
}

fn current_session_from_env() -> Option<String> {
    std::env::var("Z3RM_SESSION")
        .ok()
        .filter(|session| !session.is_empty())
}

async fn resolve_named_session_id(domain: &MuxDomain, name: &str) -> Result<String> {
    let sessions = domain.list_sessions().await?;
    let session = sessions
        .iter()
        .find(|session| session.id == name || session.name == name)
        .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;
    Ok(session.id.clone())
}

/// §3.10 Empty pane id 是错误: `unwrap_or_default()` 把空字符串变成合法目标,
/// 后续 send-keys / capture-pane 等在 daemon 端才发现失败。
/// 这里提前暴露错误, 用户即时看到。
fn ensure_non_empty_pane_id(pane_id: String, context: &str) -> Result<String> {
    if pane_id.is_empty() {
        anyhow::bail!("no focused pane in {context}");
    }
    Ok(pane_id)
}

#[derive(Clone, Copy)]
enum ResolveAccess {
    ReadOnly,
    ReadWrite,
}

impl ResolveAccess {
    fn attach_mode(self) -> mux::AttachMode {
        match self {
            Self::ReadOnly => mux::AttachMode::ReadOnly,
            Self::ReadWrite => mux::AttachMode::Shared,
        }
    }
}

/// 解析 target, 从 snapshot 中找到对应的 pane ID
async fn resolve_pane_id(
    domain: &MuxDomain,
    target: &Target,
    access: ResolveAccess,
) -> Result<String> {
    match target {
        Target::Current => {
            if let Some(pane_id) = current_pane_from_env() {
                return ensure_non_empty_pane_id(pane_id, "current");
            }

            let session_id = if let Some(session_id) = current_session_from_env() {
                resolve_named_session_id(domain, &session_id).await?
            } else {
                let sessions = domain.list_sessions().await?;
                sessions
                    .first()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("no active sessions"))?
            };

            let snapshot = domain.attach(&session_id, access.attach_mode()).await?;
            let pane_id = snapshot
                .snapshot
                .as_ref()
                .map(|s| s.focused_pane_id.clone())
                .unwrap_or_default();
            ensure_non_empty_pane_id(pane_id, "current session")
        }
        Target::Session(name) => {
            let session_id = resolve_named_session_id(domain, name).await?;
            let snapshot = domain.attach(&session_id, access.attach_mode()).await?;
            let pane_id = snapshot
                .snapshot
                .as_ref()
                .map(|s| s.focused_pane_id.clone())
                .unwrap_or_default();
            ensure_non_empty_pane_id(pane_id, &format!("session '{}'", name))
        }
        Target::PaneInSession {
            session,
            window,
            pane,
        } => {
            let sessions = domain.list_sessions().await?;
            let session_info = sessions
                .iter()
                .find(|s| s.id == *session || s.name == *session)
                .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;

            let snapshot = domain
                .attach(&session_info.id, access.attach_mode())
                .await?;

            if let Some(snap) = &snapshot.snapshot {
                if let Some(tab) = snap.tabs.get(*window as usize) {
                    if let Some(pane_info) = tab.panes.get(*pane as usize) {
                        return Ok(pane_info.id.clone());
                    }
                }
            }
            Err(anyhow::anyhow!(
                "pane {}:{} not found in session '{}'",
                window,
                pane,
                session
            ))
        }
        Target::PaneByIndex(idx) => {
            // §3.10 tmux-style %N: global pane index across sessions (tabs flattened).
            let sessions = domain.list_sessions().await?;
            if sessions.is_empty() {
                return Err(anyhow::anyhow!("no active sessions"));
            }
            let mut global_index = 0u32;
            for session in &sessions {
                let snapshot = domain.attach(&session.id, access.attach_mode()).await?;
                if let Some(snap) = &snapshot.snapshot {
                    for tab in &snap.tabs {
                        for pane_info in &tab.panes {
                            if global_index == *idx {
                                return Ok(pane_info.id.clone());
                            }
                            global_index += 1;
                        }
                    }
                }
            }
            Err(anyhow::anyhow!("pane %{} not found", idx))
        }
    }
}

/// 解析 target, 找到 session ID
async fn resolve_session_id(
    domain: &MuxDomain,
    target: &Target,
    default_session: &str,
) -> Result<String> {
    match target {
        Target::Current | Target::PaneByIndex(_) => {
            if let Some(session_id) = current_session_from_env() {
                resolve_named_session_id(domain, &session_id).await
            } else if default_session.is_empty() {
                // 空 default_session 是"一个 session 都没有", 提前报错比把空 ID
                // 发给 daemon 换来一句 "session not found" 更好懂。
                Err(anyhow::anyhow!("no active sessions"))
            } else {
                Ok(default_session.to_string())
            }
        }
        Target::Session(name) => resolve_named_session_id(domain, name).await,
        Target::PaneInSession { session, .. } => resolve_named_session_id(domain, session).await,
    }
}

/// 在所有 session 的快照里找到某个 pane 的元数据。
async fn find_pane_info(domain: &MuxDomain, pane_id: &str) -> Result<Option<PaneInfo>> {
    let sessions = domain.list_sessions().await?;
    for session in &sessions {
        let attached = domain.attach(&session.id, mux::AttachMode::Shared).await?;
        let Some(snapshot) = &attached.snapshot else {
            continue;
        };
        for tab in &snapshot.tabs {
            if let Some(pane) = tab.panes.iter().find(|pane| pane.id == pane_id) {
                return Ok(Some(pane.clone()));
            }
        }
    }
    Ok(None)
}

/// z3rm 没有 tmux 那样的服务端 paste buffer，缓冲区内容从 stdin 读。
/// stdin 是终端时直接报错 —— 否则命令会静默挂住等用户敲 EOF。
fn read_paste_buffer() -> Result<String> {
    use std::io::{IsTerminal, Read};

    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        anyhow::bail!(
            "paste-buffer reads the buffer from stdin; pipe it in (e.g. `echo hi | z3rm paste-buffer -t dev`)"
        );
    }
    let mut buffer = String::new();
    stdin
        .read_to_string(&mut buffer)
        .context("failed to read paste buffer from stdin")?;
    if buffer.is_empty() {
        anyhow::bail!("paste-buffer got an empty buffer on stdin");
    }
    Ok(buffer)
}

/// 执行 CLI 命令。
/// 来源: spec §3.10
pub async fn run_cli_command(cmd: CliCommand) -> Result<()> {
    // §16.6 SSH 远程连接不经过本地 daemon，直接建立 SSH 隧道后返回。
    if let CliCommand::Ssh { target } = cmd {
        let (_domain, _session) = mux::connect_ssh(&target)
            .await
            .context("failed to connect via SSH. Ensure the remote host has an OpenSSH client and is reachable.")?;
        eprintln!("connected to remote mux_server via SSH ({})", target);
        return Ok(());
    }

    // §3.10 CLI must never hang on a wedged daemon socket.
    let domain = tokio::time::timeout(Duration::from_secs(5), mux::connect_local(None))
        .await
        .context("mux_server not responding (connect timeout)")?
        .context("failed to connect to mux_server. Is the daemon running?")?;
    // 获取默认 session (第一个)；失败传播, 不再静默退回空串。
    let default_session = {
        let sessions = domain
            .list_sessions()
            .await
            .context("failed to list sessions when resolving default")?;
        sessions.first().map(|s| s.id.clone()).unwrap_or_default()
    };

    match cmd {
        CliCommand::Ssh { .. } => {
            // Already handled above (early return before connect_local).
        }
        CliCommand::ListSessions { format } => {
            let sessions = domain
                .list_sessions()
                .await
                .context("failed to list sessions")?;
            if let Some(format) = format {
                for session in &sessions {
                    let scope = FormatScope {
                        session: Some(session),
                        ..Default::default()
                    };
                    println!("{}", expand_format(&format, &scope)?);
                }
            } else if sessions.is_empty() {
                println!("no sessions");
            } else {
                for s in &sessions {
                    println!("{}: {} ({} clients)", s.name, s.id, s.attached_clients);
                }
            }
        }
        CliCommand::NewSession { name, cwd } => {
            let name = name.unwrap_or_else(|| {
                format!(
                    "session-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                )
            });
            // §3.10 cwd 缺省时使用当前进程的当前目录，错误向上传播。
            let cwd = match cwd {
                Some(cwd) => cwd,
                None => std::env::current_dir()
                    .context("failed to resolve current working directory for new session")?,
            };
            let id = domain
                .create_session(&name, &cwd)
                .await
                .context("failed to create session")?;

            // §3.10 tmux 语义:new -s 自动创建一个 window + 一个 pane,
            // 否则后续 send-keys / capture-pane 没有 target 可用。
            let snapshot = domain.attach(&id, mux::AttachMode::Shared).await?;
            let tab_id = snapshot
                .snapshot
                .as_ref()
                .and_then(|s| s.tabs.first())
                .map(|t| t.id.clone())
                .unwrap_or_else(|| "tab-0".to_string());
            let _pane_id = domain
                .spawn_pane(
                    &id,
                    &tab_id,
                    mux_protocol::proto::TerminalSize { cols: 80, rows: 24 },
                    None,
                    Some(&cwd),
                )
                .await
                .context("failed to spawn default pane")?;

            println!("created session {} ({})", name, id);
        }

        CliCommand::KillSession { target } => {
            let sessions = domain.list_sessions().await?;
            let session = sessions
                .iter()
                .find(|s| s.id == target || s.name == target)
                .ok_or_else(|| anyhow::anyhow!("session '{}' not found", target))?;
            domain
                .kill_session(&session.id)
                .await
                .context("failed to kill session")?;
            println!("killed session {}", session.name);
        }

        CliCommand::RenameSession { target, name } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            domain
                .rename_session(&session_id, &name)
                .await
                .context("failed to rename session")?;
            println!("renamed session {} to '{}'", session_id, name);
        }

        CliCommand::HasSession { target } => {
            let sessions = domain
                .list_sessions()
                .await
                .context("failed to list sessions")?;
            // tmux 契约: 存在 -> 退出码 0 且不输出;不存在 -> 非 0。
            if !sessions
                .iter()
                .any(|session| session.id == target || session.name == target)
            {
                anyhow::bail!("can't find session: {target}");
            }
        }

        CliCommand::KillServer => {
            match tokio::time::timeout(Duration::from_secs(2), domain.shutdown()).await {
                Ok(Ok(())) => println!("mux_server shut down"),
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "shutdown RPC failed; treating as already down");
                    println!("mux_server already shut down");
                }
                Err(_) => println!("mux_server already shut down"),
            }
        }

        // §3.10 attach is handled by main as LaunchIntent::Gui (spawn GUI, exit).
        // This arm is a safety net if reached programmatically: print only, no RPC.
        CliCommand::Attach { target } => {
            let label = target.as_deref().unwrap_or("default");
            eprintln!("z3rm: attached to session '{}' in GUI window", label);
        }

        CliCommand::Detach => {
            // §3.10 never hang if the daemon is already gone.
            match tokio::time::timeout(Duration::from_secs(2), domain.detach()).await {
                Ok(Ok(())) => eprintln!("detached"),
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "detach RPC failed; treating as already detached");
                    eprintln!("already detached");
                }
                Err(_) => eprintln!("already detached"),
            }
        }

        CliCommand::SplitWindow {
            target,
            horizontal,
            command,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            let direction = if horizontal {
                SplitDirection::LeftRight
            } else {
                SplitDirection::TopBottom
            };
            let command = command.map(|command| ShellCommand {
                program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                args: vec!["-lc".to_string(), command],
                env: Default::default(),
            });
            let new_pane = domain
                .split_pane_with_command(&pane_id, direction, command)
                .await
                .context("failed to split pane")?;
            println!("split pane: new pane {}", new_pane);
        }

        CliCommand::SendKeys {
            target,
            keys,
            encoding,
            repeat,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            let bytes = encode_send_keys(&keys, encoding)?;
            let bytes = repeated_payload(&bytes, repeat)?;
            domain
                .send_input(&pane_id, &bytes)
                .await
                .context("failed to send input")?;
        }

        CliCommand::PasteBuffer { target } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            let buffer = read_paste_buffer()?;
            domain
                .paste(&pane_id, &buffer)
                .await
                .context("failed to paste buffer")?;
        }

        CliCommand::CapturePane {
            target,
            print,
            start,
            end,
            join_wrapped,
            escape,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadOnly).await?;
            let options = CaptureOptions {
                start,
                end,
                join_wrapped,
                preserve_ansi: escape,
            };
            let text = super::capture::capture_pane(&domain, &pane_id, options)
                .await
                .context("failed to capture pane")?;
            if print {
                print!("{}", text);
            } else {
                println!("{}", text);
            }
        }

        CliCommand::ListPanes { target, format } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let sessions = domain.list_sessions().await?;
            let session_info = sessions.iter().find(|session| session.id == session_id);
            let snapshot = domain
                .attach(&session_id, mux::AttachMode::ReadOnly)
                .await?;
            if let Some(snap) = &snapshot.snapshot {
                // 默认输出里的 `%N` 是 session 内跨 tab 的连续编号 (可直接喂给 `-t %N`),
                // 而 `#{pane_index}` 是 tmux 语义的窗口内编号 (配合 `session:W.P`)。
                let mut flat_pane_index = 0usize;
                for (window_index, tab) in snap.tabs.iter().enumerate() {
                    for (pane_index, pane) in tab.panes.iter().enumerate() {
                        let focused = snap.focused_pane_id == pane.id;
                        match &format {
                            Some(format) => {
                                let scope = FormatScope {
                                    session: session_info,
                                    session_windows: Some(snap.tabs.len()),
                                    window: Some(tab),
                                    window_index: Some(window_index),
                                    window_active: Some(snap.focused_tab_id == tab.id),
                                    pane: Some(pane),
                                    pane_index: Some(pane_index),
                                    pane_active: Some(focused),
                                };
                                println!("{}", expand_format(format, &scope)?);
                            }
                            None => println!(
                                "{}%{}: {} ({}x{})",
                                if focused { "*" } else { " " },
                                flat_pane_index,
                                pane.title,
                                pane.size.as_ref().map(|s| s.cols).unwrap_or(0),
                                pane.size.as_ref().map(|s| s.rows).unwrap_or(0),
                            ),
                        }
                        flat_pane_index += 1;
                    }
                }
            }
        }

        CliCommand::ListWindows { target, format } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let sessions = domain.list_sessions().await?;
            let session_info = sessions.iter().find(|session| session.id == session_id);
            let attached = domain
                .attach(&session_id, mux::AttachMode::ReadOnly)
                .await?;
            let snapshot = attached
                .snapshot
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("session '{session_id}' returned no snapshot"))?;
            for (window_index, tab) in snapshot.tabs.iter().enumerate() {
                let active = snapshot.focused_tab_id == tab.id;
                match &format {
                    Some(format) => {
                        let scope = FormatScope {
                            session: session_info,
                            session_windows: Some(snapshot.tabs.len()),
                            window: Some(tab),
                            window_index: Some(window_index),
                            window_active: Some(active),
                            ..Default::default()
                        };
                        println!("{}", expand_format(format, &scope)?);
                    }
                    None => println!(
                        "{}{}: {} ({} panes)",
                        if active { "*" } else { " " },
                        window_index,
                        tab.title,
                        tab.panes.len(),
                    ),
                }
            }
        }

        CliCommand::SelectPane { target } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            domain
                .focus_pane(&pane_id)
                .await
                .context("failed to focus pane")?;
            eprintln!("selected pane {}", pane_id);
        }

        CliCommand::KillPane { target } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            domain
                .close_pane(&pane_id)
                .await
                .context("failed to close pane")?;
            eprintln!("killed pane {}", pane_id);
        }

        CliCommand::ResizePane {
            target,
            width,
            height,
            zoom,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            let pane_info = find_pane_info(&domain, &pane_id).await?;

            if zoom {
                let zoomed = pane_info.map(|pane| pane.zoomed).unwrap_or(false);
                domain
                    .zoom_pane(&pane_id, !zoomed)
                    .await
                    .context("failed to toggle pane zoom")?;
                eprintln!(
                    "{} pane {}",
                    if zoomed { "unzoomed" } else { "zoomed" },
                    pane_id
                );
                return Ok(());
            }

            // §3.10 Preserve unspecified axis from current pane size (do not wipe to 80x24).
            let (current_cols, current_rows) = pane_info
                .and_then(|pane| pane.size)
                .map(|size| (size.cols, size.rows))
                .unwrap_or((80, 24));

            let cols = width.map(|w| w as u32).unwrap_or(current_cols);
            let rows = height.map(|h| h as u32).unwrap_or(current_rows);
            domain
                .resize_pane(&pane_id, cols, rows)
                .await
                .context("failed to resize pane")?;
            eprintln!("resized pane {} to {}x{}", pane_id, cols, rows);
        }

        CliCommand::NewWindow { target } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;

            // 创建新 tab (通过 spawn_pane 隐式创建)
            let tab_id = format!("tab-{}", nanoid::nanoid!());
            let default_size = mux_protocol::TerminalSize { cols: 80, rows: 24 };
            let pane_id = domain
                .spawn_pane(&session_id, &tab_id, default_size, None, None)
                .await
                .context("failed to spawn pane for new window")?;
            println!("new window created: tab={}, pane={}", tab_id, pane_id);
        }

        CliCommand::RenameWindow { target, title } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadWrite).await?;
            domain
                .set_pane_title(&pane_id, &title)
                .await
                .context("failed to set pane title")?;
            eprintln!("renamed window pane {} to '{}'", pane_id, title);
        }
        CliCommand::Recover { target } => {
            if let Some(session_id) = target {
                let recovered = domain
                    .confirm_recovery(&session_id)
                    .await
                    .with_context(|| format!("failed to recover session {session_id}"))?;
                println!(
                    "recovered session {} with {} fresh shell pane(s)",
                    recovered.session_id,
                    recovered.pane_ids.len()
                );
            } else {
                for candidate in domain.list_recovery_candidates().await? {
                    let state = if candidate.metadata_complete {
                        "ready"
                    } else {
                        "legacy-incomplete"
                    };
                    println!(
                        "{}: {} (cwd={}, panes={}, {})",
                        candidate.id,
                        candidate.name,
                        candidate.cwd,
                        candidate.pane_ids.len(),
                        state
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn send_keys_encodings_produce_distinct_payloads() {
        // 同一个词在按键名模式下是回车，在字面模式下是五个字符 —— 混淆这两者
        // 会把用户想输入的文本当成控制键发进 PTY。
        let keys = strings(&["Enter"]);
        assert_eq!(
            encode_send_keys(&keys, SendKeysEncoding::KeyNames).expect("key names"),
            b"\r".to_vec()
        );
        assert_eq!(
            encode_send_keys(&keys, SendKeysEncoding::Literal).expect("literal"),
            b"Enter".to_vec()
        );
    }

    #[test]
    fn send_keys_literal_joins_arguments_without_separators() {
        let keys = strings(&["echo", " ", "hi"]);
        assert_eq!(
            encode_send_keys(&keys, SendKeysEncoding::Literal).expect("literal"),
            b"echo hi".to_vec()
        );
    }

    #[test]
    fn send_keys_hex_accepts_bare_and_prefixed_bytes() {
        let keys = strings(&["1b", "0x5b", "41"]);
        assert_eq!(
            encode_send_keys(&keys, SendKeysEncoding::Hex).expect("hex"),
            vec![0x1b, 0x5b, 0x41]
        );
    }

    #[test]
    fn send_keys_hex_rejects_non_hex_arguments() {
        let keys = strings(&["zz"]);
        let error = encode_send_keys(&keys, SendKeysEncoding::Hex).expect_err("non-hex must fail");
        assert!(
            error.to_string().contains("zz"),
            "error should name the offending argument: {error}"
        );
    }

    #[test]
    fn send_keys_repeat_payload_is_bounded() {
        assert_eq!(repeated_payload(b"ab", 3).expect("repeat"), b"ababab");
        // 超上限 -> 可恢复错误, 不是 Vec::repeat 的 capacity overflow panic。
        assert!(repeated_payload(&[0u8; 4096], 1024 * 1024).is_err());
        // 乘法溢出 -> 可恢复错误。
        assert!(repeated_payload(&[0u8; 1024], u32::MAX).is_err());
    }

    #[test]
    fn current_pane_from_env_prefers_explicit_pane() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        unsafe {
            std::env::set_var("Z3RM_PANE", "pane-from-env");
            std::env::set_var("Z3RM_SESSION", "session-from-env");
        }

        assert_eq!(current_pane_from_env().as_deref(), Some("pane-from-env"));

        unsafe {
            std::env::remove_var("Z3RM_PANE");
            std::env::remove_var("Z3RM_SESSION");
        }
    }
}
