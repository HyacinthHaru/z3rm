// CLI 命令调度: 连接 daemon, 执行命令, 输出结果
// 来源: spec §3.10

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use mux::MuxDomain;
use mux_protocol::proto::{
    ClipboardEntry, PaneInfo, ShellCommand,
    clipboard_entry::ClipboardContentType as ProtoClipboardContentType, split_node::SplitDirection,
};

use super::capture::{CaptureLine, CaptureOptions, CommandSelector};
use mux::command_history::{CommandSpan, command_output_span};
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
        /// §3.3 `--last-command` / `--command <n>`: 用 OSC 133 marker 算出
        /// `-S`/`-E`，与显式给的行号互斥。
        command: Option<CommandSelector>,
    },
    /// §3.3 `z3rm list-commands [-t <target>] [-n <max>]` — 列出 OSC 133 命令
    ListCommands {
        target: Option<String>,
        max_results: u32,
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
    /// §12 `z3rm search-scrollback [-t <target>] [-n <max>] [-S <line>] [-f] <regex>`
    SearchScrollback {
        target: Option<String>,
        pattern: String,
        start: Option<CaptureLine>,
        forward: bool,
        max_results: u32,
    },
    /// §4 `z3rm list-changes [-t <session>]` — 列出本 session 留有影子版本的文件
    ListChanges {
        target: Option<String>,
    },
    /// §4 `z3rm list-versions [-t <session>] <path>` — 列出某文件的影子版本
    ListVersions {
        target: Option<String>,
        path: String,
    },
    /// §4 `z3rm show-version [-t <session>] <path> <id>` — 把某版本内容写到 stdout
    ShowVersion {
        target: Option<String>,
        path: String,
        version_id: u64,
    },
    /// §4.8 `z3rm restore [-t <session>] <path> <id>` — 把文件回滚到指定版本
    Restore {
        target: Option<String>,
        path: String,
        version_id: u64,
    },
    /// §16.6 `z3rm show-buffer [-I]` — 把服务端剪贴板写到 stdout
    ShowBuffer {
        info: bool,
    },
    /// §16.6 `z3rm set-buffer [--type <type>] [--] <data> | -` — 设置服务端剪贴板
    SetBuffer {
        content_type: ClipboardContentType,
        source: BufferSource,
    },
    /// §16.6 `z3rm list-dir [-t <session>] [<path>]` — 列出会话 worktree 内的目录
    ListDir {
        target: Option<String>,
        path: String,
    },
    /// §16.6 `z3rm stat-file [-t <session>] <path>` — 会话 worktree 内某路径的元数据
    StatFile {
        target: Option<String>,
        path: String,
    },
    /// §16.6 `z3rm show-file [-t <session>] <path>` — 把会话 worktree 内的文件写到 stdout
    ShowFile {
        target: Option<String>,
        path: String,
    },
}

/// §16.6 `set-buffer` 的内容来源。
///
/// 剪贴板存的是字节，而 argv 只能承载 UTF-8，所以二进制内容必须走 stdin。
#[derive(Debug, PartialEq, Eq)]
pub enum BufferSource {
    /// 命令行上的字面文本。
    Literal(String),
    /// `-`：从 stdin 读原始字节。
    Stdin,
}

/// §16.6 剪贴板内容类型的 CLI 拼写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardContentType {
    #[default]
    Text,
    ImagePng,
    FilePath,
}

impl ClipboardContentType {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "png" => Some(Self::ImagePng),
            "path" => Some(Self::FilePath),
            _ => None,
        }
    }

    fn to_proto(self) -> i32 {
        let proto = match self {
            Self::Text => ProtoClipboardContentType::Text,
            Self::ImagePng => ProtoClipboardContentType::ImagePng,
            Self::FilePath => ProtoClipboardContentType::FilePath,
        };
        proto as i32
    }

    /// 服务端回来的是 proto 的 i32；未知值和 UNSPECIFIED 都退回 text，与
    /// `mux_server` 的 `ClipboardContentType::from_proto_value` 保持一致。
    fn from_proto(value: i32) -> Self {
        match ProtoClipboardContentType::from_i32(value) {
            Some(ProtoClipboardContentType::ImagePng) => Self::ImagePng,
            Some(ProtoClipboardContentType::FilePath) => Self::FilePath,
            _ => Self::Text,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::ImagePng => "png",
            Self::FilePath => "path",
        }
    }
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

/// §16.6 `set-buffer -` 的载荷从 stdin 读原始字节。
///
/// stdin 是终端时直接报错 —— 否则命令会静默挂住等用户敲 EOF。空输入是合法的
/// (清空剪贴板), 所以这里不像 `paste-buffer` 那样拒绝空缓冲。
fn read_buffer_bytes() -> Result<Vec<u8>> {
    use std::io::{IsTerminal, Read};

    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        anyhow::bail!(
            "set-buffer - reads the buffer from stdin; pipe it in (e.g. `cat logo.png | z3rm set-buffer --type png -`)"
        );
    }
    let mut buffer = Vec::new();
    stdin
        .read_to_end(&mut buffer)
        .context("failed to read the clipboard buffer from stdin")?;
    Ok(buffer)
}

/// §16.6 剪贴板条目上的 `origin_host` —— 内容是从哪台机器复制来的。
///
/// 沿用 server 侧 OSC 52 中继用的同一个来源 (`HOSTNAME`), 否则同一台机器上
/// 两条路径写进去的标签对不上。
fn clipboard_origin_host() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "z3rm-cli".to_string())
}

/// §16.6 ReadFile / ListDir / StatFile 的路径范围来自**本连接已 attach 的会话**,
/// 没 attach 过的连接服务端一律拒绝。CLI 每条命令都是一条新连接, 所以必须先
/// attach 一次。
///
/// 用 ReadOnly: 这三个 RPC 只需要 ReadOnly 角色, 而 attach 模式会给整条连接定
/// 权限上限, 一条只读命令没有理由拿到写权限。
async fn attach_for_file_access(domain: &MuxDomain, session_id: &str) -> Result<()> {
    domain
        .attach(session_id, mux::AttachMode::ReadOnly)
        .await
        .with_context(|| format!("failed to attach to session {session_id} for file access"))?;
    Ok(())
}

/// 执行 CLI 命令。
/// 来源: spec §3.10
pub async fn run_cli_command(cmd: CliCommand) -> Result<()> {
    // §16.6 `attach --ssh <uri>` 不再出现在这里: 它是 GUI 启动意图
    // (LaunchIntent::Ssh), 由 main.rs 转交给 GUI 子进程持有 SshSession。
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
        // `attach` opens a window, so main.rs intercepts it through
        // `parse_launch_intent_from` before the CLI dispatcher ever sees it.
        // Reaching here means that interception broke; printing a success
        // message would hide it.
        CliCommand::Attach { target } => {
            anyhow::bail!(
                "attach reached the CLI dispatcher instead of launching a window (target: {})",
                target.as_deref().unwrap_or("default")
            );
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
            print: _,
            start,
            end,
            join_wrapped,
            escape,
            command,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadOnly).await?;
            let (start, end) = match command {
                Some(selector) => {
                    let listed = domain
                        .list_commands(&pane_id, 0)
                        .await
                        .context("failed to list shell commands")?;
                    let selected = super::capture::select_command(&listed.commands, selector)?;
                    let (start, end) = super::capture::command_capture_lines(selected)?;
                    (Some(start), end)
                }
                None => (start, end),
            };
            let options = CaptureOptions {
                start,
                end,
                join_wrapped,
                preserve_ansi: escape,
            };
            let text = super::capture::capture_pane(&domain, &pane_id, options)
                .await
                .context("failed to capture pane")?;
            // The renderer already terminates each captured row. `println!`
            // here would add a spurious empty row, including for `-p`.
            print!("{}", text);
        }

        CliCommand::ListCommands {
            target,
            max_results,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadOnly).await?;
            let listed = domain
                .list_commands(&pane_id, max_results)
                .await
                .context("failed to list shell commands")?;
            // marker 有而命令没有, 说明 shell 只画提示符、不报告命令边界。
            // 这与"什么都没跑过"是两回事, 得说清楚。
            if listed.commands.is_empty() && listed.recorded_markers == 0 {
                println!("no OSC 133 markers recorded: this shell has no shell integration");
            } else if listed.commands.is_empty() {
                println!(
                    "no commands recorded: this shell emits only prompt starts \
                     ({} marker(s)), never command boundaries",
                    listed.recorded_markers,
                );
            } else {
                let mut unaddressable = 0usize;
                for command in &listed.commands {
                    let (start, end) = match command_output_span(command) {
                        CommandSpan::Located { start, end } => (
                            start.to_string(),
                            end.map_or_else(|| "-".to_string(), |end| end.to_string()),
                        ),
                        CommandSpan::Unaddressable => {
                            unaddressable += 1;
                            ("?".to_string(), "?".to_string())
                        }
                        CommandSpan::Unmarked => ("?".to_string(), "?".to_string()),
                    };
                    let status = match (&command.command_end, command.exit_code) {
                        (Some(_), Some(code)) => format!("exit={code}"),
                        (Some(_), None) => "done".to_string(),
                        (None, _) => "running".to_string(),
                    };
                    println!("{}\t{}\t{}\t{}", command.id, status, start, end);
                }
                if unaddressable > 0 {
                    eprintln!(
                        "note: {unaddressable} of {} command(s) have no usable line numbers — \
                         their rows left the scrollback or the numbering was retired by a \
                         resize, a clear, or scrollback reaching capacity. Exit statuses are \
                         unaffected.",
                        listed.commands.len(),
                    );
                }
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
        CliCommand::SearchScrollback {
            target,
            pattern,
            start,
            forward,
            max_results,
        } => {
            let target = super::target::parse_target(&target)?;
            let pane_id = resolve_pane_id(&domain, &target, ResolveAccess::ReadOnly).await?;
            let hits = mux::scrollback_search::search_scrollback(
                &domain,
                &pane_id,
                &pattern,
                mux::scrollback_search::SearchOptions {
                    // `-` 是"这一侧的极端边界", 也就是缺省覆盖整个 pane。
                    start: start.and_then(|line| match line {
                        super::capture::CaptureLine::Edge => None,
                        super::capture::CaptureLine::Line(line) => Some(line),
                    }),
                    forward,
                    max_results,
                },
            )
            .await?;
            // grep 式的 `行号:内容`, 行号沿用 capture-pane 的 tmux 模型, 调用方可以
            // 直接拿它去 `capture-pane -S <line> -E <line>` 取上下文。
            for hit in &hits {
                println!("{}:{}", hit.line, hit.text.trim_end());
            }
            // grep 契约: 没有命中就退出非 0, 让调用方能直接 `search-scrollback ... ||`
            // 分支, 而不必去数输出行数。
            anyhow::ensure!(!hits.is_empty(), "no matches for {pattern}");
        }
        CliCommand::ListChanges { target } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let changed = domain
                .list_changed_files(&session_id)
                .await
                .context("failed to list changed files")?;
            if changed.files.is_empty() {
                println!("no shadow versions recorded");
            } else {
                for file in &changed.files {
                    println!(
                        "{}\t{} version(s)\tseq {}",
                        file.path, file.version_count, file.latest_seq_no,
                    );
                }
            }
        }
        CliCommand::ListVersions { target, path } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let versions = domain
                .list_file_versions(&session_id, &path)
                .await
                .with_context(|| format!("failed to list versions of {path}"))?;
            if versions.versions.is_empty() {
                println!("no shadow versions for {path}");
            } else {
                for version in &versions.versions {
                    println!(
                        "{}\tseq {}\t{}",
                        version.version_id, version.seq_no, version.trigger,
                    );
                }
            }
        }
        CliCommand::ShowVersion {
            target,
            path,
            version_id,
        } => {
            use std::io::Write as _;
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let version = domain
                .get_file_version(&session_id, &path, version_id)
                .await
                .with_context(|| format!("failed to read version {version_id} of {path}"))?;
            // 影子快照存的是字节, 不保证是 UTF-8; 原样写出去才能让调用方拿它和
            // 磁盘上的文件逐字节比对。内容通常不以换行结尾, 显式 flush 才能保证
            // 最后一段不被留在缓冲区里。
            let mut stdout = std::io::stdout();
            stdout
                .write_all(&version.content)
                .context("failed to write version content to stdout")?;
            stdout
                .flush()
                .context("failed to flush version content to stdout")?;
        }
        CliCommand::Restore {
            target,
            path,
            version_id,
        } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            let response = domain
                .decline_file_version(&session_id, &path, version_id)
                .await
                .with_context(|| format!("failed to restore {path} to version {version_id}"))?;
            anyhow::ensure!(
                response.restored,
                "server did not confirm the restore of {path} to version {version_id}"
            );
            eprintln!("restored {path} to version {version_id}");
        }
        CliCommand::ShowBuffer { info } => {
            use std::io::Write as _;
            let entry = domain
                .get_clipboard()
                .await
                .context("failed to read the server clipboard")?;
            if info {
                let origin = if entry.origin_host.is_empty() {
                    "-"
                } else {
                    entry.origin_host.as_str()
                };
                println!(
                    "{}\t{}\t{} bytes",
                    ClipboardContentType::from_proto(entry.content_type).label(),
                    origin,
                    entry.data.len(),
                );
            } else {
                // 剪贴板存的是字节, 不保证是 UTF-8 也不保证以换行结尾; 原样写出去
                // 再显式 flush, 否则最后一段会留在缓冲区里。
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(&entry.data)
                    .context("failed to write the clipboard to stdout")?;
                stdout
                    .flush()
                    .context("failed to flush the clipboard to stdout")?;
            }
        }
        CliCommand::SetBuffer {
            content_type,
            source,
        } => {
            let data = match source {
                BufferSource::Literal(text) => text.into_bytes(),
                BufferSource::Stdin => read_buffer_bytes()?,
            };
            let byte_count = data.len();
            domain
                .set_clipboard(ClipboardEntry {
                    content_type: content_type.to_proto(),
                    data,
                    origin_host: clipboard_origin_host(),
                })
                .await
                .context("failed to set the server clipboard")?;
            eprintln!(
                "clipboard set ({byte_count} bytes, {})",
                content_type.label()
            );
        }
        CliCommand::ListDir { target, path } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            attach_for_file_access(&domain, &session_id).await?;
            let listing = domain
                .list_dir(&path)
                .await
                .with_context(|| format!("failed to list {path}"))?;
            // 固定四列 `<kind> <size> <modified> <name>`, 便于 awk/grep。目录里
            // 空无一物是合法结果, 不是错误, 所以这里不像 search-scrollback 那样
            // 在空结果上退非 0。
            for entry in &listing.entries {
                println!(
                    "{}\t{}\t{}\t{}",
                    if entry.is_dir { "dir" } else { "file" },
                    entry.size,
                    if entry.is_modified { "modified" } else { "-" },
                    entry.name,
                );
            }
        }
        CliCommand::StatFile { target, path } => {
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            attach_for_file_access(&domain, &session_id).await?;
            let stat = domain
                .stat_file(&path)
                .await
                .with_context(|| format!("failed to stat {path}"))?;
            println!("exists\t{}", stat.exists);
            // 路径不存在时服务端把 is_dir 填成 false; 照着印 "file" 会把一个不
            // 存在的路径描述成一个文件。
            let kind = match (stat.exists, stat.is_dir) {
                (false, _) => "-",
                (true, true) => "dir",
                (true, false) => "file",
            };
            println!("type\t{kind}");
            println!("size\t{}", stat.size);
            println!("modified\t{}", stat.modified_timestamp);
            // `test -e` 契约: 路径不在 = 非 0 退出。服务端刻意把"不存在"编码成
            // exists=false 而不是错误, 所以这个区分只能在这里做出来。
            anyhow::ensure!(stat.exists, "no such path in the session worktree: {path}");
        }
        CliCommand::ShowFile { target, path } => {
            use std::io::Write as _;
            let target = super::target::parse_target(&target)?;
            let session_id = resolve_session_id(&domain, &target, &default_session).await?;
            attach_for_file_access(&domain, &session_id).await?;
            // 二进制文件也要原样落到 stdout, 调用方才能拿它和磁盘上的文件逐字节
            // 比对。逐页写出避免 CLI 为大文件保留一份完整的内存副本。
            let mut stdout = std::io::stdout();
            let mut offset_bytes = 0;
            let mut expected_total_bytes = None;
            loop {
                let page = domain
                    .read_file_page(
                        &path,
                        offset_bytes,
                        mux_protocol::DEFAULT_READ_FILE_PAGE_BYTES,
                    )
                    .await
                    .with_context(|| format!("failed to read {path}"))?;
                if let Some(expected_total_bytes) = expected_total_bytes {
                    anyhow::ensure!(
                        page.total_bytes == expected_total_bytes,
                        "read_file size changed while paging {path}: {expected_total_bytes} became {} bytes",
                        page.total_bytes
                    );
                } else {
                    expected_total_bytes = Some(page.total_bytes);
                }
                stdout
                    .write_all(&page.content)
                    .context("failed to write the file to stdout")?;
                let Some(next_offset_bytes) = page.next_offset_bytes else {
                    break;
                };
                anyhow::ensure!(
                    next_offset_bytes > offset_bytes,
                    "read_file returned a non-advancing byte page for {path}"
                );
                offset_bytes = next_offset_bytes;
            }
            stdout
                .flush()
                .context("failed to flush the file to stdout")?;
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
                let listing = domain.list_recovery_candidates().await?;
                for candidate in listing.candidates {
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
                for rejected in listing.rejected {
                    println!("unrecoverable: {rejected}");
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
