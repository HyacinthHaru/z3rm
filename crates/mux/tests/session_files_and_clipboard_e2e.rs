//! §16.6 `ReadFile` / `StatFile` / `ListDir` / `GetClipboard` / `SetClipboard`
//! 的端到端契约。
//!
//! 这五个 RPC 的客户端语义各不相同，而且几处不同都能悄悄地把失败变成成功或者
//! 反过来：`SetClipboard` 成功时回的是**空** `Error` 体，`StatFile` 对不存在的
//! 路径回的是 `exists=false` 而不是错误，文件三兄弟在连接没有 attach 会话时必须
//! 拒绝而不是退化成整个文件系统。所以这里钉的是那些形态本身。
//!
//! 起真实 `z3rm-server` 子进程，走完整协议链路。

#![cfg(unix)]

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::proto;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

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
            eprintln!("failed to kill session-files mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap session-files mux server: {error}");
        }
    }
}

/// 建一棵可预测的 worktree，返回它 canonicalize 之后的根。
///
/// canonicalize 是必须的：macOS 的临时目录是 `/var/...`，而 `/var` 是指向
/// `/private/var` 的 symlink，服务端比较的是 canonical 前缀。用没解析过的路径
/// 去测"绝对路径必须在 cwd 内"会测出一个假的越界。
fn build_worktree(root: &std::path::Path) -> Result<PathBuf> {
    std::fs::write(root.join("readme.txt"), "hello worktree\n")?;
    // 带 NUL 字节的内容必须被判成 binary —— 这是 `encoding` 字段唯一的信息源。
    std::fs::write(root.join("payload.bin"), [0x00u8, 0x01, 0x02, 0x00, 0xff])?;
    std::fs::create_dir(root.join("nested"))?;
    std::fs::write(root.join("nested/inner.txt"), "inner\n")?;
    root.canonicalize().context("canonicalize worktree root")
}

/// 建会话 + attach，返回 (session_id, canonical worktree root)。
async fn session_with_worktree(
    domain: &MuxDomain,
    name: &str,
    worktree: &TempDir,
) -> Result<(String, PathBuf)> {
    let root = build_worktree(worktree.path())?;
    let session_id = domain.create_session(name, worktree.path()).await?;
    domain.attach(&session_id, AttachMode::Shared).await?;
    Ok((session_id, root))
}

#[tokio::test(flavor = "multi_thread")]
async fn read_file_rpc_contract() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let (session_id, root) = session_with_worktree(&domain, "read-file", &worktree).await?;

    let text = domain.read_file("readme.txt").await.context("relative")?;
    assert_eq!(text.content, b"hello worktree\n");
    assert!(!text.is_binary);
    assert_eq!(text.encoding, "utf-8");

    // 绝对路径只要落在 cwd 内就该放行。
    let absolute = root.join("nested/inner.txt");
    let inner = domain
        .read_file(absolute.to_string_lossy().as_ref())
        .await
        .context("absolute path inside the worktree")?;
    assert_eq!(inner.content, b"inner\n");

    let binary = domain.read_file("payload.bin").await.context("binary")?;
    assert_eq!(binary.content, vec![0x00u8, 0x01, 0x02, 0x00, 0xff]);
    assert!(binary.is_binary, "NUL bytes must flip is_binary");
    assert_eq!(binary.encoding, "binary");

    // 不存在的文件是错误 (和 StatFile 不同)，而且原因要活着传到调用方。
    let missing = domain
        .read_file("no-such-file.txt")
        .await
        .expect_err("reading a missing file must fail");
    assert!(
        missing.to_string().contains("read_file"),
        "the server's reason must survive, got {missing:#}"
    );

    // `..` 一律拒绝，哪怕最终仍落在 root 里。
    let traversal = domain
        .read_file("nested/../readme.txt")
        .await
        .expect_err("parent traversal must be rejected");
    assert!(
        traversal.to_string().contains("parent traversal"),
        "got {traversal:#}"
    );

    let escape = domain
        .read_file("/etc/passwd")
        .await
        .expect_err("an absolute path outside the worktree must be rejected");
    assert!(
        escape
            .to_string()
            .contains("outside the attached session worktree"),
        "got {escape:#}"
    );

    // 空路径不能被当成"根目录"放行。
    let empty = domain
        .read_file("")
        .await
        .expect_err("an empty path must be rejected");
    assert!(empty.to_string().contains("path is empty"), "got {empty:#}");

    // 连接必须扛得住上面每一次拒绝。
    domain
        .read_file("readme.txt")
        .await
        .context("the connection must survive rejected paths")?;

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// 没有 attach 过任何会话的连接没有 worktree 范围，三个文件 RPC 都必须拒绝 ——
/// 退化成"整个文件系统"等于把本机读权限交给任何能连上 socket 的进程。
#[tokio::test(flavor = "multi_thread")]
async fn file_rpcs_require_an_attached_session() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    build_worktree(worktree.path())?;
    // 会话存在但**这个连接**没有 attach。
    let session_id = domain.create_session("unattached", worktree.path()).await?;

    for error in [
        domain
            .read_file("/etc/passwd")
            .await
            .expect_err("read_file without attach"),
        domain
            .stat_file("/etc/passwd")
            .await
            .expect_err("stat_file without attach"),
        domain
            .list_dir("/etc")
            .await
            .expect_err("list_dir without attach"),
    ] {
        assert!(
            error.to_string().contains("attached session"),
            "an unattached connection must be told why, got {error:#}"
        );
    }

    domain.kill_session(&session_id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn stat_file_rpc_contract() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let (session_id, root) = session_with_worktree(&domain, "stat-file", &worktree).await?;

    let file = domain.stat_file("readme.txt").await.context("stat file")?;
    assert!(file.exists);
    assert!(!file.is_dir);
    assert_eq!(file.size, "hello worktree\n".len() as u64);
    assert!(
        file.modified_timestamp > 0,
        "mtime must be a real unix timestamp, got {}",
        file.modified_timestamp
    );

    let directory = domain.stat_file("nested").await.context("stat dir")?;
    assert!(directory.exists);
    assert!(directory.is_dir);

    // 会话根本身也要能 stat。
    let session_root = domain
        .stat_file(root.to_string_lossy().as_ref())
        .await
        .context("stat the worktree root")?;
    assert!(session_root.exists && session_root.is_dir);

    // 不存在 ≠ 错误：服务端回类型化的 exists=false，客户端不能把它变成 Err。
    let missing = domain
        .stat_file("nope.txt")
        .await
        .context("a missing path is not an error")?;
    assert!(!missing.exists);
    assert_eq!(missing.size, 0);
    assert!(!missing.is_dir);
    assert_eq!(missing.modified_timestamp, 0);

    let traversal = domain
        .stat_file("../outside")
        .await
        .expect_err("parent traversal must be rejected");
    assert!(
        traversal.to_string().contains("parent traversal"),
        "got {traversal:#}"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn list_dir_rpc_contract() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let (session_id, root) = session_with_worktree(&domain, "list-dir", &worktree).await?;

    // "." 是会话 cwd —— CLI 的缺省目标。
    let listing = domain.list_dir(".").await.context("list the session cwd")?;
    let names: Vec<&str> = listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["nested", "payload.bin", "readme.txt"],
        "目录必须排在前面，其余按名称"
    );
    let readme = listing
        .entries
        .iter()
        .find(|entry| entry.name == "readme.txt")
        .context("readme.txt in the listing")?;
    assert!(!readme.is_dir);
    assert_eq!(readme.size, "hello worktree\n".len() as u64);
    let nested = listing
        .entries
        .iter()
        .find(|entry| entry.name == "nested")
        .context("nested in the listing")?;
    assert!(nested.is_dir);

    let sub = domain.list_dir("nested").await.context("list a subdir")?;
    assert_eq!(sub.entries.len(), 1);
    assert_eq!(sub.entries[0].name, "inner.txt");

    // 绝对路径形式的会话根等价于 "."。
    let absolute = domain
        .list_dir(root.to_string_lossy().as_ref())
        .await
        .context("list the worktree root by absolute path")?;
    assert_eq!(absolute.entries.len(), 3);

    // 列一个文件是错误 (readdir 失败)，不是空列表。
    let not_a_dir = domain
        .list_dir("readme.txt")
        .await
        .expect_err("listing a file must fail");
    assert!(
        not_a_dir.to_string().contains("list_dir"),
        "got {not_a_dir:#}"
    );

    let missing = domain
        .list_dir("no-such-dir")
        .await
        .expect_err("listing a missing directory must fail");
    assert!(missing.to_string().contains("list_dir"), "got {missing:#}");

    let escape = domain
        .list_dir("/etc")
        .await
        .expect_err("a directory outside the worktree must be rejected");
    assert!(
        escape
            .to_string()
            .contains("outside the attached session worktree"),
        "got {escape:#}"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

/// §4.7 `ListDir` 的 `is_modified` 来自影子快照，不是 mtime。CLI 把它印成一列，
/// 所以两边必须对得上：`list_changed_files` 说改过的文件，目录列表里就得带标记。
#[tokio::test(flavor = "multi_thread")]
async fn list_dir_reports_shadow_modifications() -> Result<()> {
    let server = TestServer::spawn()?;
    let worktree = tempfile::tempdir().context("create worktree")?;
    let root = build_worktree(worktree.path())?;

    // `create_session` 只是 spawn 了一个后台任务去装配影子快照，返回时 watch
    // 通常还没挂上，此时任何 shadow RPC 都会失败。探测用一次性连接，探通之后
    // 测试主体再建自己的连接 —— 与 `shadow_snapshot_e2e` 的做法一致。
    let session_id = {
        let setup = server.connect().await?;
        setup.create_session("modified", &root).await?
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let probe = server.connect().await?;
        if probe.list_changed_files(&session_id).await.is_ok() {
            break;
        }
        drop(probe);
        if Instant::now() >= deadline {
            anyhow::bail!("shadow snapshot watch never armed for the test session");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let domain = server.connect().await?;
    domain.attach(&session_id, AttachMode::Shared).await?;

    // watch 挂上之后才动文件，否则这次改动可能落在 armed 之前。
    let target = root.join("readme.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        std::fs::write(&target, format!("changed at {:?}\n", Instant::now()))?;
        let changed = domain.list_changed_files(&session_id).await?;
        if changed
            .files
            .iter()
            .any(|file| file.path.ends_with("readme.txt"))
        {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("shadow snapshot never recorded a version for readme.txt");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let listing = domain.list_dir(".").await.context("list after the edit")?;
    let readme = listing
        .entries
        .iter()
        .find(|entry| entry.name == "readme.txt")
        .context("readme.txt in the listing")?;
    assert!(
        readme.is_modified,
        "a file with shadow versions must be flagged in list_dir"
    );
    let untouched = listing
        .entries
        .iter()
        .find(|entry| entry.name == "payload.bin")
        .context("payload.bin in the listing")?;
    assert!(
        !untouched.is_modified,
        "an untouched file must not be flagged"
    );
    let directory = listing
        .entries
        .iter()
        .find(|entry| entry.name == "nested")
        .context("nested in the listing")?;
    assert!(
        !directory.is_modified,
        "watcher 只给文件建版本，目录恒为 false"
    );

    domain.kill_session(&session_id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_rpc_contract() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    // 从没设置过时服务端回的是一条空 TEXT 条目，不是 Error、也不是 None。
    let initial = domain
        .get_clipboard()
        .await
        .context("an empty clipboard must still answer")?;
    assert!(initial.data.is_empty());
    assert_eq!(
        initial.content_type,
        proto::clipboard_entry::ClipboardContentType::Text as i32
    );
    assert!(initial.origin_host.is_empty());

    // 成功的 SetClipboard 回的是**空** Error 体。客户端要是照抄
    // `Some(Error(e)) => Err(e)` 的写法，这一行就会变成一条错误信息为空的失败。
    domain
        .set_clipboard(proto::ClipboardEntry {
            content_type: proto::clipboard_entry::ClipboardContentType::Text as i32,
            data: b"copied from the cli".to_vec(),
            origin_host: "smoke-host".to_string(),
        })
        .await
        .context("an empty Error body means success, not failure")?;

    let stored = domain.get_clipboard().await.context("read back")?;
    assert_eq!(stored.data, b"copied from the cli");
    assert_eq!(stored.origin_host, "smoke-host");

    // 非 UTF-8 的字节和非 TEXT 类型都必须原样往返 —— 剪贴板存的是 bytes。
    let png_magic = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00];
    domain
        .set_clipboard(proto::ClipboardEntry {
            content_type: proto::clipboard_entry::ClipboardContentType::ImagePng as i32,
            data: png_magic.clone(),
            origin_host: "smoke-host".to_string(),
        })
        .await
        .context("set a binary clipboard entry")?;
    let image = domain
        .get_clipboard()
        .await
        .context("read back the image")?;
    assert_eq!(image.data, png_magic);
    assert_eq!(
        image.content_type,
        proto::clipboard_entry::ClipboardContentType::ImagePng as i32
    );

    // 剪贴板是服务端全局的，另一个连接必须看到同一份内容。
    let other = server.connect().await?;
    let seen = other
        .get_clipboard()
        .await
        .context("a second connection reads the same clipboard")?;
    assert_eq!(seen.data, png_magic);

    Ok(())
}

/// 剪贴板不需要 attach 会话：它是服务端全局状态，`SetClipboard` 只要求
/// ReadWrite 角色 (本地 socket 默认 Admin)。这条测试挡住"哪天给它加上
/// attach 前置条件"这种回退。
#[tokio::test(flavor = "multi_thread")]
async fn clipboard_does_not_require_an_attached_session() -> Result<()> {
    let server = TestServer::spawn()?;
    let domain = server.connect().await?;

    domain
        .set_clipboard(proto::ClipboardEntry {
            content_type: proto::clipboard_entry::ClipboardContentType::FilePath as i32,
            data: b"/tmp/some/path".to_vec(),
            origin_host: String::new(),
        })
        .await
        .context("set_clipboard without any attached session")?;
    let entry = domain.get_clipboard().await?;
    assert_eq!(entry.data, b"/tmp/some/path");
    assert_eq!(
        entry.content_type,
        proto::clipboard_entry::ClipboardContentType::FilePath as i32
    );
    Ok(())
}
