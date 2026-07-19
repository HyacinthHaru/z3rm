//! # CLI e2e integration tests
//!
//! Spec §3.10: tmux-compatible CLI 控制接口。CLI agent (Claude Code、aider 等)
//! 通过 `z3rm send-keys`、`z3rm capture-pane`、`z3rm ls` 这些命令控制 z3rm,
//! 必须在真 mux_server 链路上钉死。
//!
//! 这套测试 spawn `z3rm-server` 子进程 + `z3rm <subcommand>` 子进程,
//! 验证 CLI 真正驱动 server 状态机,捕获 stdout 断言。
//!
//! 测试放在 mux crate 下 (而不是 z3rm) 是为了避开 editor/project 等迁移中
//! crate 的 test-only compile errors,它们会污染 z3rm test build chain。
//! 测试只用编译好的 z3rm 二进制 (subprocess),不依赖任何 z3rm-crate 符号。

#![cfg(unix)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// §16.1 / §3.10 隔离的 mux_server + z3rm 测试环境。
struct CliEnv {
    server: std::process::Child,
    _tmp: TempDir,
    socket_path: PathBuf,
    z3rm_bin: PathBuf,
}

impl CliEnv {
    fn spawn() -> Result<Self> {
        let tmp = tempfile::tempdir().context("temp dir")?;
        let socket_path = tmp.path().join("mux.sock");
        let db_path = tmp.path().join("mux.db");

        let target_dir = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("../../target/debug");
        let server_bin = std::env::var("Z3RM_SERVER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| target_dir.join("z3rm-server"));
        let z3rm_bin = std::env::var("Z3RM_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| target_dir.join("z3rm"));

        anyhow::ensure!(
            server_bin.exists(),
            "z3rm-server binary not found at {}. Run `cargo build -p mux_server` first.",
            server_bin.display()
        );
        anyhow::ensure!(
            z3rm_bin.exists(),
            "z3rm binary not found at {}. Run `cargo build -p z3rm` first.",
            z3rm_bin.display()
        );

        let server = Command::new(&server_bin)
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("RUST_LOG", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn z3rm-server at {}", server_bin.display()))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket_path.exists() {
            if Instant::now() >= deadline {
                anyhow::bail!("server didn't bind socket in 10s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));

        Ok(Self {
            server,
            _tmp: tmp,
            socket_path,
            z3rm_bin,
        })
    }

    fn run(&self, args: &[&str]) -> Result<(i32, String, String)> {
        let out = Command::new(&self.z3rm_bin)
            .env("Z3RM_MUX_SOCKET", &self.socket_path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("spawn z3rm {:?}", args))?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        Ok((code, stdout, stderr))
    }
}

impl Drop for CliEnv {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

// ============================================================================
// §3.10 CLI 命令 e2e 测试
// ============================================================================

#[test]
fn cli_ls_shows_created_session() -> Result<()> {
    let env = CliEnv::spawn()?;

    let (code, stdout, _) = env.run(&["ls"])?;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("no sessions"),
        "fresh daemon ls should say no sessions, got: {stdout}"
    );

    let (code, stdout, _) = env.run(&["new", "-s", "cli-test"])?;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("cli-test"),
        "new should print session name, got: {stdout}"
    );

    let (code, stdout, _) = env.run(&["ls"])?;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("cli-test"),
        "ls should list 'cli-test', got: {stdout}"
    );

    Ok(())
}

#[test]
fn cli_kill_removes_session() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "to-kill"])?;
    env.run(&["new", "-s", "to-keep"])?;

    let (code, stdout, _) = env.run(&["kill", "-t", "to-kill"])?;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("killed session to-kill"),
        "got: {stdout}"
    );

    let (_, ls, _) = env.run(&["ls"])?;
    assert!(
        !ls.contains("to-kill"),
        "to-kill should be gone from ls, got: {ls}"
    );
    assert!(
        ls.contains("to-keep"),
        "to-keep should still be there, got: {ls}"
    );

    Ok(())
}

#[test]
fn cli_kill_nonexistent_target_errors() -> Result<()> {
    let env = CliEnv::spawn()?;
    let (code, _, stderr) = env.run(&["kill", "-t", "nonexistent"])?;
    assert!(
        code != 0 || stderr.contains("not found") || stderr.contains("error"),
        "kill of nonexistent session should fail: code={code} stderr={stderr}"
    );
    Ok(())
}

#[test]
fn cli_send_keys_and_capture_pane_roundtrip() -> Result<()> {
    let env = CliEnv::spawn()?;

    env.run(&["new", "-s", "capture-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    // shell pane: echo a marker, then capture-pane should see it
    let (code, _, stderr) = env.run(&[
        "send-keys",
        "-t",
        "capture-test",
        "echo",
        "SENDKEYS_MARKER",
        "Enter",
    ])?;
    assert_eq!(code, 0, "send-keys should succeed; stderr={stderr}");

    std::thread::sleep(Duration::from_millis(700));

    let (code, stdout, _) = env.run(&["capture-pane", "-t", "capture-test", "-p"])?;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("SENDKEYS_MARKER"),
        "capture-pane should contain the echoed marker, got:\n{stdout}"
    );

    Ok(())
}

#[test]
fn cli_split_window_creates_new_pane() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "split-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, stdout, stderr) = env.run(&["split-window", "-t", "split-test", "-h"])?;
    assert_eq!(code, 0, "split-window should succeed; stderr={stderr}");
    assert!(
        stdout.contains("split pane") || stdout.contains("new pane"),
        "split-window should report new pane, got: {stdout}"
    );

    Ok(())
}

#[test]
fn cli_list_panes_returns_panes() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "panes-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, stdout, stderr) = env.run(&["list-panes", "-t", "panes-test"])?;
    assert_eq!(code, 0, "list-panes should succeed; stderr={stderr}");
    assert!(
        !stdout.trim().is_empty(),
        "list-panes should return at least one pane, got: {stdout:?}"
    );

    Ok(())
}

#[test]
fn cli_attach_prints_confirmation_and_exits() -> Result<()> {
    // §3.10 关键契约: attach 不应阻塞等待 GUI;应打印确认后立即退出
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "attach-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();
    let (code, _, stderr) = env.run(&["attach", "-t", "attach-test"])?;
    let elapsed = start.elapsed();

    assert_eq!(code, 0);
    assert!(
        elapsed < Duration::from_secs(5),
        "attach must not block waiting for GUI; took {:?}",
        elapsed
    );
    assert!(
        stderr.contains("attach"),
        "attach should print confirmation to stderr, got: {stderr}"
    );

    Ok(())
}
