// CLI 命令调度: 连接 daemon, 执行命令, 输出结果
// 来源: spec §3.10

use anyhow::{Context, Result};
use std::path::PathBuf;

use mux::MuxDomain;
use mux_protocol::proto::split_node::SplitDirection;

use super::keys::parse_key;
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
    /// `z3rm attach -t <target>` — 连接到 session (打开 GUI)
    Attach { target: Option<String> },
    /// `z3rm detach` — 断开当前 client
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

/// 解析 target, 从 snapshot 中找到对应的 pane ID
async fn resolve_pane_id(
    domain: &MuxDomain,
    target: &Target,
    default_session: &str,
) -> Result<String> {
    match target {
        Target::Current => {
            // 使用第一个 session 的 focused pane
            let sessions = domain.list_sessions().await?;
            if sessions.is_empty() {
                return Err(anyhow::anyhow!("no active sessions"));
            }
            let session_id = &sessions[0].id;
            let snapshot = domain
                .attach(session_id, mux::AttachMode::ReadOnly)
                .await?;
            Ok(snapshot
                .snapshot
                .as_ref()
                .map(|s| s.focused_pane_id.clone())
                .unwrap_or_default())
        }
        Target::Session(name) => {
            let sessions = domain.list_sessions().await?;
            let session = sessions
                .iter()
                .find(|s| s.id == *name || s.name == *name)
                .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;
            let snapshot = domain
                .attach(&session.id, mux::AttachMode::ReadOnly)
                .await?;
            Ok(snapshot
                .snapshot
                .as_ref()
                .map(|s| s.focused_pane_id.clone())
                .unwrap_or_default())
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
        Target::PaneByIndex(_idx) => {
            // 使用第一个 session 的 focused pane
            let sessions = domain.list_sessions().await?;
            if sessions.is_empty() {
                return Err(anyhow::anyhow!("no active sessions"));
            }
            let session_id = &sessions[0].id;
            let snapshot = domain
                .attach(session_id, mux::AttachMode::ReadOnly)
                .await?;
            Ok(snapshot
                .snapshot
                .as_ref()
                .map(|s| s.focused_pane_id.clone())
                .unwrap_or_default())
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
        Target::Current | Target::PaneByIndex(_) => Ok(default_session.to_string()),
        Target::Session(name) => {
            let sessions = domain.list_sessions().await?;
            let session = sessions
                .iter()
                .find(|s| s.id == *name || s.name == *name)
                .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;
            Ok(session.id.clone())
        }
        Target::PaneInSession { session, .. } => {
            let sessions = domain.list_sessions().await?;
            let session_info = sessions
                .iter()
                .find(|s| s.id == *session || s.name == *session)
                .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session))?;
            Ok(session_info.id.clone())
        }
    }
}

/// 获取 session 的第一个 tab ID
async fn get_first_tab_id(domain: &MuxDomain, session_id: &str) -> Result<String> {
    let snapshot = domain
        .attach(session_id, mux::AttachMode::ReadOnly)
        .await?;
    if let Some(snap) = &snapshot.snapshot {
        if let Some(tab) = snap.tabs.first() {
            return Ok(tab.id.clone());
        }
    }
    Err(anyhow::anyhow!("no tabs in session '{}'", session_id))
}

/// 执行 CLI 命令。
/// 来源: spec §3.10
pub async fn run_cli_command(cmd: CliCommand) -> Result<()> {
    // 连接到 daemon
    let domain = mux::connect_local(None)
        .await
        .context("failed to connect to mux_server. Is the daemon running?")?;

    // 获取默认 session (第一个)
    let default_session = {
        let sessions = domain.list_sessions().await.unwrap_or_default();
        sessions.first().map(|s| s.id.clone()).unwrap_or_default()
    };

    match cmd {
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
            let cwd = cwd.unwrap_or_else(|| PathBuf::from("/"));
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

        CliCommand::Attach { target } => {
            let target = super::target::parse_target(&target);
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
            command: _,
        } => {
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
            let direction = if horizontal {
                SplitDirection::LeftRight
            } else {
                SplitDirection::TopBottom
            };
            let new_pane = domain
                .split_pane(&pane_id, direction)
                .await
                .context("failed to split pane")?;
            println!("split pane: new pane {}", new_pane);
        }

        CliCommand::SendKeys { target, keys } => {
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
            for key in &keys {
                let bytes = parse_key(key);
                domain
                    .send_input(&pane_id, &bytes)
                    .await
                    .context("failed to send input")?;
            }
        }

        CliCommand::CapturePane {
            target,
            print,
            scrollback,
            escape,
        } => {
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
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
            let target = super::target::parse_target(&target);
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let snapshot = domain
                .attach(&session_id, mux::AttachMode::ReadOnly)
                .await?;
            if let Some(snap) = &snapshot.snapshot {
                for tab in &snap.tabs {
                    for (j, pane) in tab.panes.iter().enumerate() {
                        let focused = snap.focused_pane_id == pane.id;
                        let marker = if focused { "*" } else { " " };
                        println!(
                            "{}%d: {} {} ({}x{})",
                            marker,
                            j,
                            pane.title,
                            pane.size.as_ref().map(|s| s.cols).unwrap_or(0),
                            pane.size.as_ref().map(|s| s.rows).unwrap_or(0),
                        );
                    }
                }
            }
        }

        CliCommand::SelectPane { target } => {
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
            domain
                .focus_pane(&pane_id)
                .await
                .context("failed to focus pane")?;
            eprintln!("selected pane {}", pane_id);
        }

        CliCommand::KillPane { target } => {
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
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
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
            let cols = width.map(|w| w as u32).unwrap_or(80);
            let rows = height.map(|h| h as u32).unwrap_or(24);
            domain
                .resize_pane(&pane_id, cols, rows)
                .await
                .context("failed to resize pane")?;
            eprintln!("resized pane {} to {}x{}", pane_id, cols, rows);
        }

        CliCommand::NewWindow { target } => {
            let target = super::target::parse_target(&target);
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
            let target = super::target::parse_target(&target);
            let pane_id = resolve_pane_id(&domain, &target, &default_session).await?;
            domain
                .set_pane_title(&pane_id, &title)
                .await
                .context("failed to set pane title")?;
            eprintln!("renamed window pane {} to '{}'", pane_id, title);
        }
    }

    Ok(())
}
