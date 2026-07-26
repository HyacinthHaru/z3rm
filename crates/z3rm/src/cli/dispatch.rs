// CLI 命令调度: 连接 daemon, 执行命令, 输出结果
// 来源: spec §3.10

use anyhow::{Context, Result};
use std::path::PathBuf;

use mux::MuxDomain;
use mux_protocol::proto::{split_node::SplitDirection, ShellCommand};

use super::keys::parse_keys;
use super::target::Target;

/// CLI 控制命令枚举
/// 来源: spec §3.10 — tmux 兼容的 CLI 命令，让 agent 零学习成本操控 z3rm
#[derive(Debug)]
pub enum CliCommand {
    /// `z3rm ls` — 列出所有 session
    ListSessions,
    /// `z3rm new -s <name>` — 创建新 session
    NewSession {
        name: Option<String>,
        cwd: Option<PathBuf>,
    },
    /// `z3rm kill -t <target>` — 终止 session
    KillSession { target: String },
    /// `z3rm kill-server` — 优雅关闭 mux_server (结束所有 session 并退出)
    KillServer,
    /// `z3rm attach -t <target>` — 连接到 session (打开 GUI)
    Attach { target: Option<String> },
    /// `z3rm detach` — 断开当前 client
    /// `z3rm attach --ssh <ssh://uri>` — 通过 SSH 隧道连接到远程 mux_server
    Ssh { target: String },
    Detach,
    /// `z3rm split-window -t <target> [-h|-v]` — 分割 pane
    SplitWindow {
        target: Option<String>,
        horizontal: bool,
        command: Option<String>,
    },
    /// `z3rm send-keys -t <target> <keys...>` — 发送输入到 pane
    SendKeys {
        target: Option<String>,
        keys: Vec<String>,
    },
    /// `z3rm capture-pane -t <target> [-p] [-S <-N>] [-e]` — 捕获 pane 内容
    CapturePane {
        target: Option<String>,
        print: bool,
        scrollback: Option<i32>,
        escape: bool,
    },
    /// `z3rm list-panes -t <target>` — 列出 session 中的 pane
    ListPanes { target: Option<String> },
    /// `z3rm select-pane -t <target>` — 聚焦 pane
    SelectPane { target: Option<String> },
    /// `z3rm kill-pane -t <target>` — 关闭 pane
    KillPane { target: Option<String> },
    /// `z3rm resize-pane -t <target> -x <W> -y <H>` — 调整 pane 大小
    ResizePane {
        target: Option<String>,
        width: Option<u16>,
        height: Option<u16>,
    },
    /// `z3rm new-window -t <target>` — 创建新 tab
    NewWindow { target: Option<String> },
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
        .or_else(|| std::env::var("Z3RM_PANE_ID").ok().filter(|pane| !pane.is_empty()))
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

/// 解析 target, 从 snapshot 中找到对应的 pane ID
async fn resolve_pane_id(
    domain: &MuxDomain,
    target: &Target,
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

            let snapshot = domain.attach(&session_id, mux::AttachMode::ReadOnly).await?;
            let pane_id = snapshot
                .snapshot
                .as_ref()
                .map(|s| s.focused_pane_id.clone())
                .unwrap_or_default();
            ensure_non_empty_pane_id(pane_id, "current session")
        }
        Target::Session(name) => {
            let session_id = resolve_named_session_id(domain, name).await?;
            let snapshot = domain.attach(&session_id, mux::AttachMode::ReadOnly).await?;
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
                .attach(&session_info.id, mux::AttachMode::ReadOnly)
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
                let snapshot = domain
                    .attach(&session.id, mux::AttachMode::ReadOnly)
                    .await?;
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
            } else {
                Ok(default_session.to_string())
            }
        }
        Target::Session(name) => resolve_named_session_id(domain, name).await,
        Target::PaneInSession { session, .. } => resolve_named_session_id(domain, session).await,
    }
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

    // 连接到 daemon
    let domain = mux::connect_local(None)
        .await
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
        CliCommand::ListSessions => {
            let sessions = domain
                .list_sessions()
                .await
                .context("failed to list sessions")?;
            if sessions.is_empty() {
                println!("no sessions");
            } else {
                for s in &sessions {
                    println!(
                        "{}: {} ({} clients)",
                        s.name, s.id, s.attached_clients
                    );
                }
            }
        }
        CliCommand::NewSession { name, cwd } => {
            let name = name.unwrap_or_else(|| format!("session-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()));
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

        CliCommand::KillServer => {
            domain
                .shutdown()
                .await
                .context("failed to shut down mux_server")?;
            println!("mux_server shut down");
        }

        CliCommand::Attach { target } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            domain
                .attach(&session_id, mux::AttachMode::Shared)
                .await
                .context("failed to attach")?;
            eprintln!("attached to session {}", session_id);
        }

        CliCommand::Detach => {
            domain.detach().await.context("failed to detach")?;
            eprintln!("detached");
        }

        CliCommand::SplitWindow {
            target,
            horizontal,
            command,
        } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;
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

        CliCommand::SendKeys { target, keys } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;
            let bytes = parse_keys(&keys);
            domain
                .send_input(&pane_id, &bytes)
                .await
                .context("failed to send input")?;
        }

        CliCommand::CapturePane {
            target,
            print,
            scrollback,
            escape,
        } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;
            let text = super::capture::capture_pane(
                &domain,
                &pane_id,
                scrollback,
                escape,
            )
            .await
            .context("failed to capture pane")?;
            if print {
                print!("{}", text);
            } else {
                println!("{}", text);
            }
        }

        CliCommand::ListPanes { target } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let snapshot = domain
                .attach(&session_id, mux::AttachMode::ReadOnly)
                .await?;
            if let Some(snap) = &snapshot.snapshot {
                let mut pane_index = 0usize;
                for tab in &snap.tabs {
                    for pane in &tab.panes {
                        let focused = snap.focused_pane_id == pane.id;
                        let marker = if focused { "*" } else { " " };
                        println!(
                            "{}%{}: {} ({}x{})",
                            marker,
                            pane_index,
                            pane.title,
                            pane.size.as_ref().map(|s| s.cols).unwrap_or(0),
                            pane.size.as_ref().map(|s| s.rows).unwrap_or(0),
                        );
                        pane_index += 1;
                    }
                }
            }
        }

        CliCommand::SelectPane { target } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;
            domain
                .focus_pane(&pane_id)
                .await
                .context("failed to focus pane")?;
            eprintln!("selected pane {}", pane_id);
        }

        CliCommand::KillPane { target } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;
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
        } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;

            // §3.10 Preserve unspecified axis from current pane size (do not wipe to 80x24).
            let (current_cols, current_rows) = {
                let sessions = domain.list_sessions().await?;
                let mut found = (80u32, 24u32);
                for session in &sessions {
                    if let Ok(attach) = domain.attach(&session.id, mux::AttachMode::ReadOnly).await {
                        if let Some(snap) = &attach.snapshot {
                            for tab in &snap.tabs {
                                if let Some(pane) = tab.panes.iter().find(|p| p.id == pane_id) {
                                    found = (
                                        pane.size.as_ref().map(|s| s.cols).unwrap_or(80),
                                        pane.size.as_ref().map(|s| s.rows).unwrap_or(24),
                                    );
                                }
                            }
                        }
                    }
                }
                found
            };

            let cols = width.map(|w| w as u32).unwrap_or(current_cols);
            let rows = height.map(|h| h as u32).unwrap_or(current_rows);
            domain
                .resize_pane(&pane_id, cols, rows)
                .await
                .context("failed to resize pane")?;
            eprintln!("resized pane {} to {}x{}", pane_id, cols, rows);
        }

        CliCommand::NewWindow { target } => {
            let target = super::target::parse_target(&target).context("invalid target")?;
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
            let target = super::target::parse_target(&target).context("invalid target")?;
            let pane_id = resolve_pane_id(&domain, &target).await?;
            domain
                .set_pane_title(&pane_id, &title)
                .await
                .context("failed to set pane title")?;
            eprintln!("renamed window pane {} to '{}'", pane_id, title);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
