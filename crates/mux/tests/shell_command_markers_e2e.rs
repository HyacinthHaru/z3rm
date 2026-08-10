//! §3.3 `ListCommands` 的端到端契约。
//!
//! CLI 的 `list-commands` / `capture-pane --command` 把这个 RPC 的行号直接喂回
//! `capture-pane -S/-E`，所以这里锁的是那些假设本身：一条命令由 A→B→C→D 划出、
//! 缺 marker 不影响其余的、行号是 tmux 模型 (可见区首行 0、负数进历史)、行号不
//! 可用时缺省而不是给一个猜的、以及退出码与行号相互独立。
//!
//! 不依赖用户的 shell 配置：pane 里跑的是一段自己吐 OSC 133 序列的 `/bin/sh`。

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const PANE_COLS: u32 = 40;
const PANE_ROWS: u32 = 12;

struct TestServer {
    child: std::process::Child,
    socket_path: PathBuf,
    bashrc_path: Option<PathBuf>,
    _tmp: TempDir,
}

impl TestServer {
    /// `scrollback_lines` 直接决定"到容量之后行号作废"这个悬崖出现得多快，
    /// 开小才能在一条测试里走到它。
    fn spawn(scrollback_lines: u32) -> Result<Self> {
        Self::spawn_with_shell(scrollback_lines, None)
    }

    fn spawn_with_shell(scrollback_lines: u32, shell: Option<&str>) -> Result<Self> {
        let tmp = tempfile::tempdir().context("create temp dir")?;
        let socket_path = tmp.path().join("mux.sock");
        let db_path = tmp.path().join("mux.db");
        let integration_path = tmp.path().join("shell-integration");
        let bashrc_path = if shell == Some("/bin/bash") {
            let home = tmp.path().join("home");
            std::fs::create_dir_all(&home).context("create isolated shell home")?;
            let bashrc = home.join(".bashrc");
            std::fs::write(&bashrc, "export Z3RM_TEST_RC_SOURCED=yes\nPS1='z3rm$ '\n")
                .context("write isolated user bashrc")?;
            Some(bashrc)
        } else {
            None
        };

        let exe = std::env::var("Z3RM_SERVER_BIN").ok().unwrap_or_else(|| {
            let manifest = std::env::var("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."));
            let candidates = [
                manifest.join("../../target/debug/z3rm-server"),
                manifest.join("../../target/release/z3rm-server"),
            ];
            for candidate in &candidates {
                if candidate.exists() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
            "z3rm-server".to_string()
        });

        let mut server_command = std::process::Command::new(&exe);
        server_command
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("Z3RM_SCROLLBACK_LINES", scrollback_lines.to_string())
            .env("Z3RM_SHELL_INTEGRATION_DIR", &integration_path)
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(shell) = shell {
            server_command
                .env("SHELL", shell)
                .env("HOME", tmp.path().join("home"))
                .env_remove("ZDOTDIR")
                .env_remove("PROMPT_COMMAND");
        }
        let child = server_command
            .spawn()
            .with_context(|| format!("failed to spawn z3rm-server at {exe}"))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if socket_path.exists() {
                break;
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "z3rm-server failed to bind socket at {} within 10s",
                    socket_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));

        Ok(Self {
            child,
            socket_path,
            bashrc_path,
            _tmp: tmp,
        })
    }

    async fn connect(&self) -> Result<MuxDomain> {
        mux::connect_local(Some(self.socket_path.as_path()))
            .await
            .context("connect_local failed")
    }
}

async fn wait_for_running_command(
    domain: &MuxDomain,
    pane_id: &str,
    minimum: usize,
    timeout: Duration,
) -> Result<Vec<proto::CommandRange>> {
    let deadline = Instant::now() + timeout;
    loop {
        let listed = domain.list_commands(pane_id, 0).await?;
        if listed.commands.len() >= minimum
            && listed.commands[minimum - 1].output_start.is_some()
            && listed.commands[minimum - 1].command_end.is_none()
        {
            return Ok(listed.commands);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timeout waiting for running command {minimum} in {pane_id}: {:?}",
                listed.commands
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("failed to kill shell-marker mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap shell-marker mux server: {error}");
        }
    }
}

/// 一段自己吐 OSC 133 的 shell 脚本。
///
/// 三条已完成的命令 + 一条还在跑的：
/// 第 1 条四个 marker 齐全并带退出码 0；第 2 条只发 A 和 D (退出码 7)，模拟
/// 只报告命令边界的 shell；第 3 条发到 C 为止但退出码缺席 (`D` 不带状态)；
/// 最后一条停在 C，代表还没结束。收尾的 `cat` 让 pane 不退出。
const EMIT_SCRIPT: &str = r#"
osc() { printf '\033]133;%s\007' "$1"; }
osc A; printf 'p$ '; osc B; printf 'first\r\n'; osc C; printf 'alpha\r\nbravo\r\n'; osc 'D;0'
osc A; printf 'p$ second\r\n'; printf 'charlie\r\n'; osc 'D;7'
osc A; printf 'p$ '; osc B; printf 'third\r\n'; osc C; printf 'delta\r\n'; osc D
osc A; printf 'p$ '; osc B; printf 'running\r\n'; osc C; printf 'echo\r\n'
cat
"#;

async fn spawn_marker_pane(domain: &MuxDomain, session_id: &str, tab_id: &str) -> Result<String> {
    let pane_id = domain
        .spawn_pane(
            session_id,
            tab_id,
            proto::TerminalSize {
                cols: PANE_COLS,
                rows: PANE_ROWS,
            },
            Some(proto::ShellCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), EMIT_SCRIPT.into()],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await
        .context("spawn_pane failed")?;
    wait_for_commands(domain, &pane_id, 4, Duration::from_secs(20)).await?;
    Ok(pane_id)
}

/// 轮询到至少 `minimum` 条命令被记录下来。
///
/// PTY 输出 → alacritty → marker 是异步的，写死一个 sleep 会在慢机器上翻车。
async fn wait_for_commands(
    domain: &MuxDomain,
    pane_id: &str,
    minimum: usize,
    timeout: Duration,
) -> Result<Vec<proto::CommandRange>> {
    let deadline = Instant::now() + timeout;
    loop {
        let listed = domain.list_commands(pane_id, 0).await?;
        let found = listed.commands.len();
        if found >= minimum {
            return Ok(listed.commands);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timeout waiting for {minimum} commands in {pane_id}, got {found}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 把一段 tmux 行区间读成文本，走的正是 CLI `capture-pane -S/-E` 的那条路径。
async fn read_lines(domain: &MuxDomain, pane_id: &str, from: i64, to: i64) -> Result<Vec<String>> {
    let grid = domain.fetch_grid_update(pane_id, 0).await?;
    let Some(proto::fetch_grid_update_response::Update::FullSnapshot(snapshot)) =
        grid.update.as_ref()
    else {
        anyhow::bail!("expected a full grid snapshot");
    };
    let history_size = i64::from(snapshot.history_size);
    let mut lines = Vec::new();
    for line in from..=to {
        let cells = if line < 0 {
            let index = u32::try_from(history_size + line)
                .with_context(|| format!("line {line} is before the oldest history row"))?;
            let scrollback = domain.fetch_scrollback(pane_id, index, 1, 1).await?;
            scrollback
                .lines
                .first()
                .map(|row| row.cells.clone())
                .unwrap_or_default()
        } else {
            let offset = usize::try_from(line)? * snapshot.cols as usize;
            snapshot
                .cells
                .iter()
                .skip(offset)
                .take(snapshot.cols as usize)
                .cloned()
                .collect()
        };
        lines.push(
            cells
                .iter()
                .map(|cell| cell.char.as_str())
                .collect::<String>()
                .trim_end()
                .to_string(),
        );
    }
    Ok(lines)
}

fn marker_line(marker: &Option<proto::CommandMarker>) -> Option<i64> {
    marker.as_ref().and_then(|marker| marker.line)
}

#[tokio::test(flavor = "multi_thread")]
async fn default_bash_emits_markers_and_preserves_shell_behavior() -> Result<()> {
    if !std::path::Path::new("/bin/bash").is_file() {
        return Ok(());
    }

    let server = TestServer::spawn_with_shell(2_000, Some("/bin/bash"))?;
    let bashrc_path = server
        .bashrc_path
        .clone()
        .context("bash test server has no isolated bashrc")?;
    let original_bashrc = std::fs::read_to_string(&bashrc_path)?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let session_id = domain
        .create_session("default-bash-markers", worktree.path())
        .await?;
    let attached = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attached
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.tabs.first())
        .map(|tab| tab.id.clone())
        .context("attach snapshot has no tabs")?;
    let pane_id = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            proto::TerminalSize {
                cols: PANE_COLS,
                rows: PANE_ROWS,
            },
            None,
            Some(worktree.path()),
        )
        .await?;

    domain
        .send_input(&pane_id, b"printf 'rc:%s\\n' \"$Z3RM_TEST_RC_SOURCED\"\r")
        .await?;
    wait_for_commands(&domain, &pane_id, 1, Duration::from_secs(20)).await?;
    domain.send_input(&pane_id, b"false\r").await?;
    wait_for_commands(&domain, &pane_id, 2, Duration::from_secs(20)).await?;
    domain
        .send_input(&pane_id, b"z3rm_command_that_does_not_exist\r")
        .await?;
    wait_for_commands(&domain, &pane_id, 3, Duration::from_secs(20)).await?;
    domain.send_input(&pane_id, b"sleep 30\r").await?;
    wait_for_running_command(&domain, &pane_id, 4, Duration::from_secs(20)).await?;
    domain.send_input(&pane_id, &[0x03]).await?;
    wait_for_commands(&domain, &pane_id, 4, Duration::from_secs(20)).await?;
    domain
        .send_input(&pane_id, b"printf 'after-interrupt\\n'\r")
        .await?;
    let commands = wait_for_commands(&domain, &pane_id, 5, Duration::from_secs(20)).await?;

    assert_eq!(
        commands
            .iter()
            .take(5)
            .map(|command| command.exit_code)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(127), Some(130), Some(0)],
        "default bash integration must preserve command statuses: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .take(5)
            .all(|command| command.prompt.is_some()
                && command.command.is_some()
                && command.output_start.is_some()
                && command.command_end.is_some()),
        "default bash commands must have complete OSC 133 ranges: {commands:?}"
    );
    let first = &commands[0];
    let first_start = marker_line(&first.output_start).context("first output start")?;
    let first_end = marker_line(&first.command_end).context("first command end")?;
    let first_end = if first
        .command_end
        .as_ref()
        .is_some_and(|marker| marker.column == 0)
    {
        first_end - 1
    } else {
        first_end
    };
    assert!(
        read_lines(&domain, &pane_id, first_start, first_end)
            .await?
            .iter()
            .any(|line| line.contains("rc:yes")),
        "the managed rcfile must source the user's bashrc"
    );
    assert_eq!(std::fs::read_to_string(&bashrc_path)?, original_bashrc);

    domain.kill_session(&session_id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn list_commands_rpc_contract() -> Result<()> {
    let server = TestServer::spawn(2_000)?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let session_id = domain
        .create_session("shell-markers", worktree.path())
        .await?;
    let attached = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attached
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.tabs.first())
        .map(|tab| tab.id.clone())
        .context("attach snapshot has no tabs")?;

    let pane_id = spawn_marker_pane(&domain, &session_id, &tab_id).await?;
    let listed = domain.list_commands(&pane_id, 0).await?;
    assert_eq!(
        listed.commands.len(),
        4,
        "one command per A→D run, got {:?}",
        listed.commands
    );
    assert!(listed.recorded_markers >= 12, "{listed:?}");

    // 第 1 条: 四个 marker 齐全, 退出码 0。
    let first = &listed.commands[0];
    assert_eq!(first.exit_code, Some(0));
    assert!(first.prompt.is_some() && first.command.is_some());
    assert!(first.output_start.is_some() && first.command_end.is_some());

    // 第 2 条: 只发了 A 和 D。缺 B/C 不该妨碍它成为一条命令, 退出码照样拿得到。
    let second = &listed.commands[1];
    assert_eq!(second.exit_code, Some(7));
    assert!(second.command.is_none(), "{second:?}");
    assert!(second.output_start.is_none(), "{second:?}");
    assert!(second.command_end.is_some(), "{second:?}");

    // 第 3 条: D 不带状态码。"结束了但不知道状态" 与 "还在跑" 必须能区分开。
    let third = &listed.commands[2];
    assert!(third.command_end.is_some(), "{third:?}");
    assert_eq!(third.exit_code, None, "{third:?}");

    // 第 4 条: 停在 C, 还在跑。
    let running = &listed.commands[3];
    assert!(running.command_end.is_none(), "{running:?}");
    assert!(running.output_start.is_some(), "{running:?}");

    // id 单调递增, 且跨调用稳定 —— `capture-pane --command <id>` 全指望这一点。
    let ids: Vec<u64> = listed.commands.iter().map(|command| command.id).collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    let again = domain.list_commands(&pane_id, 0).await?;
    assert_eq!(
        again.commands.iter().map(|c| c.id).collect::<Vec<_>>(),
        ids,
        "ids must not shift between calls"
    );

    // max_results 取最近的 N 条。
    let newest = domain.list_commands(&pane_id, 2).await?;
    assert_eq!(
        newest.commands.iter().map(|c| c.id).collect::<Vec<_>>(),
        ids[2..],
        "max_results keeps the newest commands"
    );

    // 行号必须真的指向那条命令的输出。第 1 条输出是 alpha / bravo。
    let start = marker_line(&first.output_start).context("first command output start")?;
    let end = marker_line(&first.command_end).context("first command end")?;
    let end = if first
        .command_end
        .as_ref()
        .is_some_and(|marker| marker.column == 0)
    {
        end - 1
    } else {
        end
    };
    assert_eq!(
        read_lines(&domain, &pane_id, start, end).await?,
        vec!["alpha".to_string(), "bravo".to_string()],
        "the output range must cover exactly that command's output"
    );

    // 还在跑的那条: 起点找得到, 终点没有 —— `-E` 缺省即"到可见区末尾"。
    let running_start = marker_line(&running.output_start).context("running output start")?;
    assert_eq!(
        read_lines(&domain, &pane_id, running_start, running_start).await?,
        vec!["echo".to_string()],
    );

    // 未知 pane 必须回一条能读的错误, 而且不能把连接拆掉。
    let error = domain
        .list_commands("no-such-pane", 0)
        .await
        .expect_err("listing an unknown pane must fail");
    assert!(
        error.to_string().contains("pane not found"),
        "the reason must survive to the caller, got {error:#}"
    );
    domain
        .list_commands(&pane_id, 0)
        .await
        .context("the connection must survive a failed list")?;

    // 没有 shell integration 的 pane: 空列表, 且明确说明一个 marker 都没有。
    let plain = domain
        .spawn_pane(
            &session_id,
            &tab_id,
            proto::TerminalSize {
                cols: PANE_COLS,
                rows: PANE_ROWS,
            },
            Some(proto::ShellCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "printf 'no markers here\\r\\n'; cat".into()],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await?;
    let plain_listed = domain.list_commands(&plain, 0).await?;
    assert!(plain_listed.commands.is_empty(), "{plain_listed:?}");
    assert_eq!(plain_listed.recorded_markers, 0);

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// 行号一旦不可用就必须缺省, 绝不猜一个 —— 错的行号比查不到糟得多。而退出码
/// 与行号是两件独立的事, 不该被一起拖下水。
#[tokio::test(flavor = "multi_thread")]
async fn evicted_rows_drop_their_line_numbers_but_keep_exit_codes() -> Result<()> {
    // scrollback 一到容量, 每追加一行就静默逐出一行, 行号随即作废。开到 60
    // 行是为了几十行填充就能走到那个悬崖。
    let server = TestServer::spawn(60)?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let session_id = domain
        .create_session("marker-eviction", worktree.path())
        .await?;
    let attached = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attached
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.tabs.first())
        .map(|tab| tab.id.clone())
        .context("attach snapshot has no tabs")?;

    let pane_id = spawn_marker_pane(&domain, &session_id, &tab_id).await?;
    let before = domain.list_commands(&pane_id, 0).await?;
    let located_before = before
        .commands
        .iter()
        .filter(|command| marker_line(&command.output_start).is_some())
        .count();
    assert!(
        located_before > 0,
        "a fresh pane must resolve its markers: {before:?}"
    );

    // pane 里跑的是 `cat`, 送进去的每一行都会被回显, 用它把历史推到容量。
    for _ in 0..8 {
        domain
            .send_input(&pane_id, b"filler\r".repeat(20).as_slice())
            .await?;
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let listed = loop {
        let listed = domain.list_commands(&pane_id, 0).await?;
        let unlocated = listed
            .commands
            .iter()
            .all(|command| marker_line(&command.output_start).is_none());
        if !listed.commands.is_empty() && unlocated {
            break listed;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timeout waiting for marker rows to become unaddressable: {listed:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    for command in &listed.commands {
        for marker in [
            &command.prompt,
            &command.command,
            &command.output_start,
            &command.command_end,
        ] {
            assert!(
                marker.as_ref().is_none_or(|marker| marker.line.is_none()),
                "an unaddressable row must be reported as absent, never guessed: {command:?}"
            );
        }
    }
    // 退出码不受行号影响。
    assert_eq!(
        listed
            .commands
            .iter()
            .map(|command| command.exit_code)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(7), None, None],
        "exit codes must survive losing the row numbering: {listed:?}",
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}
