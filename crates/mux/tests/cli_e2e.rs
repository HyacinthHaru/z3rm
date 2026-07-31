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

    /// §3.10 `paste-buffer` 从 stdin 读缓冲区,所以要能把数据喂进子进程。
    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Result<(i32, String, String)> {
        use std::io::Write;

        let mut child = Command::new(&self.z3rm_bin)
            .env("Z3RM_MUX_SOCKET", &self.socket_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn z3rm {:?}", args))?;
        child
            .stdin
            .take()
            .context("child stdin was not piped")?
            .write_all(stdin.as_bytes())
            .context("write child stdin")?;
        let out = child.wait_with_output().context("wait for z3rm")?;
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
    assert!(stdout.contains("killed session to-kill"), "got: {stdout}");

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
fn cli_capture_pane_escape_flag_preserves_ansi() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "ansi-capture"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, _, stderr) = env.run(&[
        "send-keys",
        "-t",
        "ansi-capture",
        "printf",
        " ",
        "'\\033[31mANSI_MARKER\\033[0m'",
        "Enter",
    ])?;
    assert_eq!(code, 0, "send-keys should succeed; stderr={stderr}");
    std::thread::sleep(Duration::from_millis(700));

    let (code, plain, stderr) = env.run(&["capture-pane", "-t", "ansi-capture", "-p"])?;
    assert_eq!(code, 0, "plain capture failed: {stderr}");
    assert!(plain.contains("ANSI_MARKER"), "plain capture: {plain:?}");
    assert!(
        !plain.contains("\u{1b}["),
        "plain capture leaked ANSI: {plain:?}"
    );

    let (code, escaped, stderr) = env.run(&["capture-pane", "-t", "ansi-capture", "-p", "-e"])?;
    assert_eq!(code, 0, "escaped capture failed: {stderr}");
    assert!(
        escaped.contains("\u{1b}[31m")
            || escaped.contains(";31m")
            || escaped.contains("\u{1b}[0;31m"),
        "escaped capture should include red SGR, got: {escaped:?}"
    );
    assert!(
        escaped.contains("\u{1b}[0m"),
        "escaped capture: {escaped:?}"
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

// ============================================================================
// §3.10 list-windows / -F 格式引擎 / 新增命令 e2e
// ============================================================================

#[test]
fn cli_list_windows_lists_session_windows() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "windows-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, stdout, stderr) = env.run(&["list-windows", "-t", "windows-test"])?;
    assert_eq!(code, 0, "list-windows should succeed; stderr={stderr}");
    assert!(
        stdout.contains("0:") && stdout.contains("panes"),
        "list-windows should print an indexed window with a pane count, got: {stdout:?}"
    );

    // `lsw` 是 tmux 的别名,必须走同一条路径。
    let (code, alias_stdout, stderr) = env.run(&["lsw", "-t", "windows-test"])?;
    assert_eq!(code, 0, "lsw alias should succeed; stderr={stderr}");
    assert_eq!(alias_stdout, stdout, "lsw must behave like list-windows");

    Ok(())
}

#[test]
fn cli_format_strings_render_session_window_and_pane_fields() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "fmt-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, stdout, stderr) =
        env.run(&["ls", "-F", "[#{session_name}][#{session_attached}]"])?;
    assert_eq!(code, 0, "ls -F should succeed; stderr={stderr}");
    assert!(
        stdout.contains("[fmt-test]["),
        "ls -F should substitute session_name, got: {stdout:?}"
    );

    // 未知变量按 tmux 语义展开成空串,`##` 是字面 `#`。
    let (code, stdout, stderr) = env.run(&["ls", "-F", "##{session_name}=[#{pane_pid}]"])?;
    assert_eq!(code, 0, "ls -F escaping should succeed; stderr={stderr}");
    assert_eq!(
        stdout.trim(),
        "#{session_name}=[]",
        "unknown variables expand to empty and ## is a literal #"
    );

    let (code, stdout, stderr) = env.run(&[
        "list-windows",
        "-t",
        "fmt-test",
        "-F",
        "#{session_name}:#{window_index} #{window_panes} #{?window_active,active,idle}",
    ])?;
    assert_eq!(code, 0, "list-windows -F should succeed; stderr={stderr}");
    assert!(
        stdout.starts_with("fmt-test:0 1 "),
        "list-windows -F should render window fields, got: {stdout:?}"
    );

    // `session:window.pane` 是可以直接回填给 -t 的目标形式。
    let (code, stdout, stderr) = env.run(&[
        "list-panes",
        "-t",
        "fmt-test",
        "-F",
        "#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height}",
    ])?;
    assert_eq!(code, 0, "list-panes -F should succeed; stderr={stderr}");
    assert!(
        stdout.starts_with("fmt-test:0.0 80x24"),
        "list-panes -F should render a usable target and size, got: {stdout:?}"
    );

    let target = stdout
        .split_whitespace()
        .next()
        .context("format output had no target")?
        .to_string();
    let (code, _, stderr) = env.run(&["capture-pane", "-t", &target, "-p"])?;
    assert_eq!(
        code, 0,
        "the target produced by -F should resolve; stderr={stderr}"
    );

    Ok(())
}

#[test]
fn cli_format_string_error_is_reported() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "fmt-error"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, _, stderr) = env.run(&["ls", "-F", "#{session_name"])?;
    assert_ne!(code, 0, "an unterminated #{{ must fail");
    assert!(
        stderr.contains("unterminated"),
        "error should explain the unterminated variable, got: {stderr:?}"
    );

    Ok(())
}

#[test]
fn cli_rename_session_changes_the_name() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "old-name"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, _, stderr) = env.run(&["rename-session", "-t", "old-name", "new-name"])?;
    assert_eq!(code, 0, "rename-session should succeed; stderr={stderr}");

    let (_, ls, _) = env.run(&["ls"])?;
    assert!(ls.contains("new-name"), "ls should show the new name: {ls}");
    assert!(
        !ls.contains("old-name"),
        "ls should not show the old name: {ls}"
    );

    Ok(())
}

#[test]
fn cli_has_session_exit_codes_match_tmux() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "present"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, stdout, stderr) = env.run(&["has-session", "-t", "present"])?;
    assert_eq!(code, 0, "existing session should exit 0; stderr={stderr}");
    assert!(
        stdout.trim().is_empty(),
        "has-session should be silent on success, got: {stdout:?}"
    );

    let (code, _, stderr) = env.run(&["has-session", "-t", "absent"])?;
    assert_ne!(code, 0, "missing session must exit non-zero");
    assert!(
        stderr.contains("absent"),
        "error should name the session, got: {stderr:?}"
    );

    Ok(())
}

#[test]
fn cli_paste_buffer_writes_stdin_into_the_pane() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "paste-test"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, _, stderr) = env.run_with_stdin(
        &["paste-buffer", "-t", "paste-test"],
        "echo PASTED_MARKER\n",
    )?;
    assert_eq!(code, 0, "paste-buffer should succeed; stderr={stderr}");
    std::thread::sleep(Duration::from_millis(700));

    let (code, stdout, _) = env.run(&["capture-pane", "-t", "paste-test", "-p"])?;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("PASTED_MARKER"),
        "pasted text should reach the pane, got:\n{stdout}"
    );

    // 空 stdin 是错误,不能装作粘贴成功。
    let (code, _, stderr) = env.run_with_stdin(&["paste-buffer", "-t", "paste-test"], "")?;
    assert_ne!(code, 0, "an empty buffer must fail");
    assert!(stderr.contains("empty buffer"), "stderr={stderr:?}");

    Ok(())
}

#[test]
fn cli_resize_pane_zoom_toggles() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "zoom-test"])?;
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["split-window", "-t", "zoom-test", "-h"])?;
    std::thread::sleep(Duration::from_millis(300));

    let (code, _, stderr) = env.run(&["resize-pane", "-t", "zoom-test", "-Z"])?;
    assert_eq!(code, 0, "resize-pane -Z should succeed; stderr={stderr}");
    assert!(stderr.contains("zoomed"), "stderr={stderr:?}");

    let (code, _, stderr) = env.run(&["resize-pane", "-t", "zoom-test", "-Z"])?;
    assert_eq!(code, 0, "a second -Z should unzoom; stderr={stderr}");
    assert!(
        stderr.contains("unzoomed"),
        "-Z must toggle rather than always zoom, stderr={stderr:?}"
    );

    Ok(())
}

#[test]
fn cli_capture_pane_end_line_bounds_the_output() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "end-line-test"])?;
    // 两次 capture 要逐行相等,先等 shell 把 prompt 画完再抓。
    std::thread::sleep(Duration::from_millis(900));

    let (code, full, stderr) = env.run(&["capture-pane", "-t", "end-line-test", "-p"])?;
    assert_eq!(code, 0, "capture-pane should succeed; stderr={stderr}");
    assert_eq!(full.lines().count(), 24, "default capture is the 24 rows");

    let (code, bounded, stderr) = env.run(&[
        "capture-pane",
        "-t",
        "end-line-test",
        "-p",
        "-S",
        "0",
        "-E",
        "2",
    ])?;
    assert_eq!(code, 0, "capture-pane -E should succeed; stderr={stderr}");
    assert_eq!(
        bounded.lines().count(),
        3,
        "-S 0 -E 2 is an inclusive 3-row window, got: {bounded:?}"
    );
    assert_eq!(
        bounded.lines().collect::<Vec<_>>(),
        full.lines().take(3).collect::<Vec<_>>(),
        "the bounded capture must be the top of the visible region"
    );

    Ok(())
}

#[test]
fn cli_capture_pane_join_merges_wrapped_lines() -> Result<()> {
    let env = CliEnv::spawn()?;
    env.run(&["new", "-s", "join-test"])?;
    std::thread::sleep(Duration::from_millis(500));

    // 200 个字符在 80 列的 pane 里必然折行,alacritty 会给折行处打 WRAPLINE。
    let payload = "A".repeat(200);
    let command = format!("echo {payload}");
    let (code, _, stderr) = env.run(&["send-keys", "-t", "join-test", "-l", &command])?;
    assert_eq!(code, 0, "send-keys -l should succeed; stderr={stderr}");
    env.run(&["send-keys", "-t", "join-test", "Enter"])?;
    std::thread::sleep(Duration::from_millis(900));

    let (code, plain, stderr) = env.run(&["capture-pane", "-t", "join-test", "-p"])?;
    assert_eq!(code, 0, "capture-pane should succeed; stderr={stderr}");
    assert!(
        !plain.lines().any(|line| line.contains(&payload)),
        "without -J the wrapped output must stay split across rows:\n{plain}"
    );

    let (code, joined, stderr) = env.run(&["capture-pane", "-t", "join-test", "-p", "-J"])?;
    assert_eq!(code, 0, "capture-pane -J should succeed; stderr={stderr}");
    assert!(
        joined.lines().any(|line| line.contains(&payload)),
        "-J must rejoin the wrapped output into one line:\n{joined}"
    );

    Ok(())
}

#[test]
fn cli_extension_help_does_not_fall_through_to_the_gui() -> Result<()> {
    let env = CliEnv::spawn()?;

    let (code, stdout, stderr) = env.run(&["extension", "--help"])?;
    assert_eq!(code, 0, "extension --help should exit 0; stderr={stderr}");
    for expected in ["search", "install", "update", "uninstall", "list"] {
        assert!(
            stdout.contains(expected),
            "extension --help should advertise '{expected}', got:\n{stdout}"
        );
    }

    // 未知子命令必须报错,而不是静默启动 GUI。
    let (code, _, stderr) = env.run(&["extension", "bogus-subcommand"])?;
    assert_ne!(code, 0, "an unknown extension subcommand must fail");
    assert!(!stderr.is_empty(), "clap should explain the failure");

    Ok(())
}

#[test]
fn cli_extension_uninstall_removes_the_directory() -> Result<()> {
    let env = CliEnv::spawn()?;
    let extensions_dir = env._tmp.path().join("extensions");
    let installed = extensions_dir.join("demo-extension");
    std::fs::create_dir_all(&installed).context("create fake extension")?;
    std::fs::write(
        installed.join("extension.toml"),
        "id = \"demo-extension\"\nname = \"Demo\"\nversion = \"1.2.3\"\n\n[grammars.demo]\nversion = \"9.9.9\"\n",
    )
    .context("write fake manifest")?;

    let extensions_dir_arg = extensions_dir.to_string_lossy().into_owned();
    let (code, stdout, stderr) =
        env.run(&["extension", "list", "--extensions-dir", &extensions_dir_arg])?;
    assert_eq!(code, 0, "extension list should succeed; stderr={stderr}");
    assert!(
        stdout.contains("demo-extension") && stdout.contains("1.2.3"),
        "list must read the top-level version, not the grammar's, got:\n{stdout}"
    );

    let (code, stdout, stderr) = env.run(&[
        "extension",
        "uninstall",
        "demo-extension",
        "--extensions-dir",
        &extensions_dir_arg,
        "--yes",
    ])?;
    assert_eq!(
        code, 0,
        "extension uninstall should succeed; stderr={stderr}"
    );
    assert!(stdout.contains("uninstalled"), "stdout={stdout:?}");
    assert!(!installed.exists(), "the extension directory must be gone");

    let (code, _, stderr) = env.run(&[
        "extension",
        "uninstall",
        "demo-extension",
        "--extensions-dir",
        &extensions_dir_arg,
        "--yes",
    ])?;
    assert_ne!(code, 0, "uninstalling a missing extension must fail");
    assert!(stderr.contains("not installed"), "stderr={stderr:?}");

    Ok(())
}
