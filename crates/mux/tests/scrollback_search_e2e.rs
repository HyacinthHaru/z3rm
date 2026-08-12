//! §12 `SearchScrollback` 的端到端契约。
//!
//! CLI 的 `search-scrollback` 把 tmux 行号换算成这个 RPC 的历史下标 + 方向，
//! 换算对不对完全取决于这里的语义。所以这条测试锁的是那些假设本身：下标从最旧
//! 的一行开始数、direction 0 朝更旧、1 朝更新、`max_results` 从行走的方向截断、
//! 越界的 `from_line` 被钳住而不是报错。
//!
//! 起真实 `z3rm-server` 子进程，走完整协议链路。

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// pane 高度。故意开小，好让少量输出就把内容推进 scrollback。
const PANE_ROWS: u32 = 6;
/// 打印多少行标记。远大于 `PANE_ROWS`，历史段才有足够素材。
const MARKER_COUNT: usize = 60;

struct TestServer {
    child: std::process::Child,
    socket_path: PathBuf,
    _tmp: TempDir,
}

impl TestServer {
    fn spawn() -> Result<Self> {
        let tmp = tempfile::tempdir().context("create temp dir")?;
        let socket_path = tmp.path().join("mux.sock");
        let db_path = tmp.path().join("mux.db");

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

        let child = std::process::Command::new(&exe)
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
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
            _tmp: tmp,
        })
    }

    async fn connect(&self) -> Result<MuxDomain> {
        mux::connect_local(Some(self.socket_path.as_path()))
            .await
            .context("connect_local failed")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("failed to kill scrollback-search mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap scrollback-search mux server: {error}");
        }
    }
}

/// 开一个 pane，打印 `MARKER_COUNT` 行标记，等历史攒够再返回 pane id。
///
/// 尾部的 `cat` 是为了让 shell 不退出：pane 一死，后面所有查询都拿不到它。
async fn pane_with_scrollback(
    domain: &MuxDomain,
    session_id: &str,
    tab_id: &str,
) -> Result<String> {
    let script = format!(
        "index=1; while [ $index -le {MARKER_COUNT} ]; \
         do echo \"scrollback-marker-$index\"; index=$((index+1)); done; cat"
    );
    let pane_id = domain
        .spawn_pane(
            session_id,
            tab_id,
            proto::TerminalSize {
                cols: 40,
                rows: PANE_ROWS,
            },
            Some(proto::ShellCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script],
                env: HashMap::new(),
            }),
            Some(&PathBuf::from("/")),
        )
        .await
        .context("spawn_pane failed")?;

    // 打印的行里有 PANE_ROWS 行还留在可见区，进不了历史。只等确定会滚出去的那部分。
    let expected_history = MARKER_COUNT as u32 - PANE_ROWS;
    wait_for_history(domain, &pane_id, expected_history, Duration::from_secs(20)).await?;
    Ok(pane_id)
}

/// 轮询到 pane 的历史至少有 `minimum` 行。
///
/// PTY 输出 → alacritty → history 是异步的，写死一个 sleep 会在慢机器上翻车。
async fn wait_for_history(
    domain: &MuxDomain,
    pane_id: &str,
    minimum: u32,
    timeout: Duration,
) -> Result<u32> {
    let deadline = Instant::now() + timeout;
    let mut last = 0;
    loop {
        let response = domain.fetch_grid_update(pane_id, 0).await?;
        if let Some(proto::fetch_grid_update_response::Update::FullSnapshot(snapshot)) =
            response.update.as_ref()
        {
            last = snapshot.history_size;
            if last >= minimum {
                return Ok(last);
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timeout waiting for {minimum} history lines in {pane_id}, got {last}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 把一条命中的上下文单元格拼回纯文本。
fn match_text(found: &proto::SearchMatch) -> String {
    found
        .context
        .iter()
        .map(|cell| cell.char.as_str())
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn search_scrollback_rpc_contract() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let session_id = domain
        .create_session("scrollback-search", worktree.path())
        .await?;
    let attached = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attached
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.tabs.first())
        .map(|tab| tab.id.clone())
        .context("attach snapshot has no tabs")?;

    let pane_id = pane_with_scrollback(&domain, &session_id, &tab_id).await?;
    let history_size = wait_for_history(&domain, &pane_id, 1, Duration::from_secs(5)).await?;

    // 从最新一行历史朝更旧走：CLI 的缺省方向。
    let newest_first = domain
        .search_scrollback(&pane_id, "scrollback-marker-", history_size - 1, 0, 5)
        .await
        .context("backward search")?;
    assert_eq!(
        newest_first.matches.len(),
        5,
        "max_results must cap the walk, got {:?}",
        newest_first
            .matches
            .iter()
            .map(match_text)
            .collect::<Vec<_>>()
    );
    let backward_lines: Vec<u32> = newest_first
        .matches
        .iter()
        .map(|found| found.line_number)
        .collect();
    assert!(
        backward_lines.windows(2).all(|pair| pair[0] > pair[1]),
        "direction 0 must walk toward older lines, got {backward_lines:?}"
    );

    // 从最旧一行朝更新走。
    let oldest_first = domain
        .search_scrollback(&pane_id, "scrollback-marker-", 0, 1, 5)
        .await
        .context("forward search")?;
    let forward_lines: Vec<u32> = oldest_first
        .matches
        .iter()
        .map(|found| found.line_number)
        .collect();
    assert!(
        forward_lines.windows(2).all(|pair| pair[0] < pair[1]),
        "direction 1 must walk toward newer lines, got {forward_lines:?}"
    );
    assert!(
        forward_lines[0] < backward_lines[0],
        "the two directions must start at opposite ends: {forward_lines:?} vs {backward_lines:?}"
    );

    // 下标 0 是最旧的一行历史，所以正向的第一条命中就是第一行标记。
    let first = oldest_first.matches.first().context("a forward match")?;
    assert!(
        match_text(first).contains("scrollback-marker-1"),
        "index 0 must be the oldest history line, got {:?}",
        match_text(first)
    );

    // 越界的 from_line 必须被钳住 —— CLI 会把一个用户给的行号直接换算过来，
    // 钳不住就意味着一次拼错的 -S 换来空结果而不是最接近的那一段。
    let clamped = domain
        .search_scrollback(&pane_id, "scrollback-marker-", u32::MAX, 0, 3)
        .await
        .context("backward search from an out-of-range line")?;
    assert!(
        !clamped.matches.is_empty(),
        "an out-of-range from_line must clamp to the newest history line, not return nothing"
    );

    // 正向越界没有可搜的行，但也不该报错。
    let past_the_end = domain
        .search_scrollback(&pane_id, "scrollback-marker-", u32::MAX, 1, 3)
        .await
        .context("forward search past the end")?;
    assert!(past_the_end.matches.is_empty());

    // 搜不到的模式返回空列表，不是错误。
    let no_match = domain
        .search_scrollback(
            &pane_id,
            "definitely-not-in-this-pane",
            history_size - 1,
            0,
            5,
        )
        .await
        .context("search for a pattern with no matches")?;
    assert!(no_match.matches.is_empty());

    // 未知 pane 必须回一条能读的错误，而且不能把连接拆掉。
    let error = domain
        .search_scrollback("no-such-pane", "x", 0, 0, 1)
        .await
        .expect_err("searching an unknown pane must fail");
    assert!(
        error.to_string().contains("pane not found"),
        "the reason must survive to the caller, got {error:#}"
    );
    domain
        .search_scrollback(&pane_id, "scrollback-marker-", history_size - 1, 0, 1)
        .await
        .context("the connection must survive a failed search")?;

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// 服务端对编译不了的正则是静默返回空列表的 —— 和"真的没搜到"无法区分。
/// CLI 因此在本地先编译一次把语法错误变成报错；这条测试把服务端那个行为钉住，
/// 免得哪天它改成回 Error 体而 CLI 那层的先行校验被当成多余删掉。
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_regex_comes_back_empty_rather_than_as_an_error() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let session_id = domain.create_session("bad-regex", worktree.path()).await?;
    let attached = domain.attach(&session_id, AttachMode::Shared).await?;
    let tab_id = attached
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.tabs.first())
        .map(|tab| tab.id.clone())
        .context("attach snapshot has no tabs")?;

    let pane_id = pane_with_scrollback(&domain, &session_id, &tab_id).await?;
    let response = domain
        .search_scrollback(&pane_id, "scrollback-marker-[", 0, 1, 5)
        .await
        .context("an unparseable regex must not fail the request")?;
    assert!(
        response.matches.is_empty(),
        "an unparseable regex yields no matches, got {:?}",
        response.matches.iter().map(match_text).collect::<Vec<_>>()
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}
