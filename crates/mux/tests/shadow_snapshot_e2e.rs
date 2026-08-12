//! §4 Shadow snapshot RPC 端到端测试。
//!
//! Shadow review RPCs only make sense against a live daemon: recording flows
//! through the filesystem watcher, debounce queue, and single-writer recorder
//! thread. This suite starts a real server, points a session at a temporary
//! worktree, and exercises list/review/content/restore requests over the socket.

#![cfg(unix)]
#[path = "common/mod.rs"]
mod common;
use common::binary;

use anyhow::{Context, Result};
use mux::{AttachMode, MuxDomain};
use mux_protocol::{ChangedFile, FileContentState, FileVersion};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// 录制是异步 + 去抖的 (watcher → debounce → recorder),所以每个断言都要轮询。
/// 20 秒远大于去抖窗口,只有真正没录上才会耗尽。
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 比 500ms 的默认值短,让轮询更快收敛;仍然远大于一次 `fs::write` 的耗时,
/// 所以单次写入不会被拆成多个版本。
const DEBOUNCE_MILLIS: u64 = 150;

const ALPHA_FIRST: &[u8] = b"alpha version one\n";
const ALPHA_SECOND: &[u8] = b"alpha version two\n";
const BETA_CONTENT: &[u8] = b"beta only write\n";
const ALPHA_INTERVENING: &[u8] = b"ALPHA VERSION TWO\n";

/// 独占一个 `TempDir` 的 mux_server 进程,与 `tests/e2e.rs` 的 `TestServer`
/// 同构,额外多做两件事:
///
/// - `Z3RM_SETTINGS` 指向本测试自己的 settings.json,这样影子快照的开关和
///   去抖窗口不受运行测试的机器上的用户设置影响;
/// - `HOME` 指向 TempDir,因为 daemon 把影子存储放在
///   `$LOCAL_DATA/z3rm/shadow/<session_id>`,不重定向就会往用户真实的
///   Application Support 里写测试数据。
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

        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).context("create sandboxed HOME")?;

        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            format!(
                r#"{{"shadow_snapshot":{{"enabled":true,"debounce_ms":{DEBOUNCE_MILLIS},"git_commit_hook":"skip"}}}}"#
            ),
        )
        .context("write shadow snapshot settings")?;

        let exe = binary("Z3RM_SERVER_BIN", "z3rm-server")?;

        let child = std::process::Command::new(&exe)
            .env("Z3RM_MUX_SOCKET", &socket_path)
            .env("Z3RM_MUX_DB", &db_path)
            .env("Z3RM_SETTINGS", &settings_path)
            .env("HOME", &home)
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn z3rm-server at {}", exe.display()))?;

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
            eprintln!("failed to kill shadow snapshot e2e mux server: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to reap shadow snapshot e2e mux server: {error}");
        }
    }
}

/// 一个 cwd 指向真实临时目录的 session。
///
/// `_worktree` 必须活到测试结束,否则 worktree 会在断言之前被删掉。
struct ShadowSession {
    domain: MuxDomain,
    id: String,
    root: PathBuf,
    _worktree: TempDir,
}

/// 造一个临时 worktree 并规范化它的路径。
///
/// macOS 上临时目录在 `/var` 下,而 `/var` 是 `/private/var` 的符号链接。
/// 服务端在解析路径前会 canonicalize session root,所以这里必须先给出规范化的
/// cwd,否则录制时的路径和查询时的路径对不上。
fn new_worktree() -> Result<(TempDir, PathBuf)> {
    let worktree = tempfile::tempdir().context("create session worktree")?;
    let root = worktree
        .path()
        .canonicalize()
        .context("canonicalize session worktree")?;
    Ok((worktree, root))
}

/// 在 `root` 上开一个 session 并等到影子快照真的挂上。
async fn start_session(
    server: &TestServer,
    name: &str,
    worktree: TempDir,
    root: PathBuf,
) -> Result<ShadowSession> {
    let domain = server.connect().await?;
    let id = domain.create_session(name, &root).await?;
    wait_for_snapshot_watch(server, &id, Duration::from_secs(20)).await?;
    domain.attach(&id, AttachMode::Shared).await?;
    Ok(ShadowSession {
        domain,
        id,
        root,
        _worktree: worktree,
    })
}

/// `create_session` 只是 spawn 了一个后台任务去装配影子快照,返回时 watch 通常
/// 还没挂上。此时任何 shadow RPC 都会在服务端拿到 "shadow snapshot is not
/// active",而服务端把这个错误用 `?` 抛出整个连接处理循环 —— 也就是说探测失败
/// 会连带把这条连接打死。所以这里每次探测都用一条一次性连接,探通之后测试主体
/// 再建自己的连接。
async fn wait_for_snapshot_watch(
    server: &TestServer,
    session_id: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let probe = server.connect().await?;
        match probe.list_changed_files(session_id).await {
            Ok(_) => return Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error)
                        .context("shadow snapshot watch never armed for the test session");
                }
            }
        }
        drop(probe);
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 轮询 `list_changed_files` 直到 `file_name` 出现且版本数达到 `minimum_versions`。
async fn wait_for_changed_file(
    session: &ShadowSession,
    file_name: &str,
    minimum_versions: u64,
    timeout: Duration,
) -> Result<ChangedFile> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = session.domain.list_changed_files(&session.id).await?;
        if let Some(found) = response
            .files
            .iter()
            .find(|file| Path::new(&file.path).ends_with(file_name))
            && found.version_count >= minimum_versions
        {
            return Ok(found.clone());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for {file_name} to reach {minimum_versions} shadow version(s). \
                 list_changed_files returned: {:?}",
                response.files
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 轮询 `list_file_versions` 直到 `path` 至少有 `minimum` 个版本。
async fn wait_for_file_versions(
    session: &ShadowSession,
    path: &Path,
    minimum: usize,
    timeout: Duration,
) -> Result<Vec<FileVersion>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = session
            .domain
            .list_file_versions(&session.id, path.to_string_lossy().as_ref())
            .await?;
        if response.versions.len() >= minimum {
            return Ok(response.versions);
        }
        if Instant::now() >= deadline {
            let changed = session.domain.list_changed_files(&session.id).await?;
            anyhow::bail!(
                "timed out waiting for {minimum} shadow version(s) of {}. \
                 list_file_versions returned: {:?}; list_changed_files returned: {:?}",
                path.display(),
                response.versions,
                changed.files
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 轮询 `list_file_versions` 直到 `path` 的最新版本是一个删除墓碑。
///
/// 不能改成"等版本数 +1":FSEvents 可能把同一次写入拆成两批上报,凭空多出一个
/// write 版本就会让计数提前满足,于是断言撞上的是那个 write 而不是墓碑。
/// 条件本身就是要等的东西,所以直接轮询条件。
async fn wait_for_tombstone(
    session: &ShadowSession,
    path: &Path,
    timeout: Duration,
) -> Result<Vec<FileVersion>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = session
            .domain
            .list_file_versions(&session.id, path.to_string_lossy().as_ref())
            .await?;
        if response
            .versions
            .last()
            .is_some_and(|version| version.trigger == "delete")
        {
            return Ok(response.versions);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for a delete tombstone on {}. \
                 list_file_versions returned: {:?}",
                path.display(),
                response.versions,
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 轮询磁盘直到 `path` 的内容等于 `expected`。
///
/// §4.8 的 decline 在服务端是同步写回的,返回时文件应该已经落盘;留一小段轮询
/// 是为了在失败时报出"实际读到了什么"而不是一个裸断言。回滚目标可能是一个已被
/// 删除的文件,所以读不到文件在轮询期间是正常状态,只有超时才算失败。
async fn wait_for_file_contents(path: &Path, expected: &[u8], timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let actual = std::fs::read(path);
        if actual.as_deref().is_ok_and(|bytes| bytes == expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let found = match &actual {
                Ok(bytes) => format!("{:?}", String::from_utf8_lossy(bytes)),
                Err(error) => format!("<unreadable: {error}>"),
            };
            anyhow::bail!(
                "decline did not restore {} on disk. expected {:?}, found {found}",
                path.display(),
                String::from_utf8_lossy(expected),
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 写两版 alpha.txt,等每一版都真的录上。
///
/// 必须等第一版录完再写第二版:去抖窗口内的两次写会被合并成一个版本,而且
/// recorder 是在 flush 时才读文件的 —— 提前写第二版会让"第一个版本"记录到
/// 第二版的内容。
async fn record_two_alpha_revisions(session: &ShadowSession) -> Result<PathBuf> {
    let alpha = session.root.join("alpha.txt");
    std::fs::write(&alpha, ALPHA_FIRST).context("write alpha.txt first revision")?;
    wait_for_changed_file(session, "alpha.txt", 1, RECORD_TIMEOUT).await?;

    std::fs::write(&alpha, ALPHA_SECOND).context("write alpha.txt second revision")?;
    wait_for_changed_file(session, "alpha.txt", 2, RECORD_TIMEOUT).await?;

    Ok(alpha)
}

/// §4 四个 shadow RPC 的完整链路 + 排序 + 错误路径。
///
/// 合并成一条测试是刻意的:每个 `TestServer` 都要 spawn 一个真实 daemon 并等它
/// 装配影子引擎,拆成多条就要付多次这个成本,而这些场景共享同一个 session 的
/// 语义没有冲突。
///
/// 取版本内容 / 回滚这一段刻意走"文件已被删除"的形状,这是影子快照最有代表性的
/// 用途 (误删恢复)。文件仍留在磁盘上时的同一组调用见下面那条回归测试。
#[tokio::test(flavor = "multi_thread")]
async fn shadow_snapshot_rpc_round_trip() -> Result<()> {
    let server = TestServer::spawn()?;
    let (worktree, root) = new_worktree()?;

    // 在 session 存在之前写好,watcher 因此永远看不到它的写入:这是"从没被改过
    // 的路径"这条错误路径的素材。
    let untouched = root.join("untouched.txt");
    std::fs::write(&untouched, b"never modified\n").context("seed untouched file")?;

    let session = start_session(&server, "shadow-e2e", worktree, root).await?;

    // ------------------------------------------------------------------
    // 1. 录制 + list_changed_files
    // ------------------------------------------------------------------
    let alpha = record_two_alpha_revisions(&session).await?;
    let listed = session.domain.list_changed_files(&session.id).await?.files;
    let changed_alpha = listed
        .iter()
        .find(|file| Path::new(&file.path).ends_with("alpha.txt"))
        .with_context(|| format!("alpha.txt missing from list_changed_files: {listed:?}"))?;
    assert!(
        changed_alpha.version_count >= 2,
        "alpha.txt was written twice, so list_changed_files must report at least 2 versions: \
         {changed_alpha:?}"
    );
    assert!(
        changed_alpha.latest_seq_no > 0,
        "a recorded file must carry a non-zero latest_seq_no: {changed_alpha:?}"
    );
    assert!(
        !listed
            .iter()
            .any(|file| Path::new(&file.path).ends_with("untouched.txt")),
        "a file written before the watch was armed must not be reported as changed: {listed:?}"
    );

    // ------------------------------------------------------------------
    // 2. 排序: 后改的文件 latest_seq_no 更大,并排在前面
    // ------------------------------------------------------------------
    let beta = session.root.join("beta.txt");
    std::fs::write(&beta, BETA_CONTENT).context("write beta.txt")?;
    wait_for_changed_file(&session, "beta.txt", 1, RECORD_TIMEOUT).await?;

    let ordered = session.domain.list_changed_files(&session.id).await?.files;
    let beta_index = ordered
        .iter()
        .position(|file| Path::new(&file.path).ends_with("beta.txt"))
        .with_context(|| format!("beta.txt missing from list_changed_files: {ordered:?}"))?;
    let alpha_index = ordered
        .iter()
        .position(|file| Path::new(&file.path).ends_with("alpha.txt"))
        .with_context(|| format!("alpha.txt missing from list_changed_files: {ordered:?}"))?;
    assert!(
        ordered[beta_index].latest_seq_no > ordered[alpha_index].latest_seq_no,
        "beta.txt was written last, so its latest_seq_no must exceed alpha.txt's: {ordered:?}"
    );
    assert!(
        beta_index < alpha_index,
        "list_changed_files is ordered newest-first, so beta.txt must precede alpha.txt: {ordered:?}"
    );
    assert!(
        ordered
            .windows(2)
            .all(|pair| pair[0].latest_seq_no >= pair[1].latest_seq_no),
        "list_changed_files must be sorted by latest_seq_no descending: {ordered:?}"
    );

    // ------------------------------------------------------------------
    // 3. list_file_versions → get_file_version → decline_file_version
    // ------------------------------------------------------------------
    // 两次写入的版本要在删除之前取,删除本身会再追加一个墓碑版本。
    let written = wait_for_file_versions(&session, &alpha, 2, RECORD_TIMEOUT).await?;
    assert!(
        written
            .windows(2)
            .all(|pair| pair[0].seq_no < pair[1].seq_no),
        "list_file_versions must be ordered by strictly increasing SeqNo: {written:?}"
    );

    let oldest = written
        .first()
        .cloned()
        .context("list_file_versions returned an empty version list")?;
    assert_eq!(
        oldest.trigger, "create",
        "the first retained event for a newly created file must remain distinguishable: {oldest:?}"
    );
    let oldest_content = session
        .domain
        .get_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            oldest.version_id,
        )
        .await
        .context("get_file_version for the oldest alpha.txt version")?;
    assert_eq!(
        oldest_content.content,
        ALPHA_FIRST,
        "the oldest shadow version must hold the first bytes written, got {:?}",
        String::from_utf8_lossy(&oldest_content.content)
    );

    let newest = written
        .last()
        .cloned()
        .context("list_file_versions returned an empty version list")?;
    let newest_content = session
        .domain
        .get_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            newest.version_id,
        )
        .await
        .context("get_file_version for the newest alpha.txt version")?;
    assert_eq!(
        newest_content.content,
        ALPHA_SECOND,
        "the newest shadow version must hold the second bytes written, got {:?}",
        String::from_utf8_lossy(&newest_content.content)
    );

    // §4.4 删除必须留下墓碑版本。这条曾经全程落空:macOS 上 FSEvents 把一个路径
    // 的 create/modify/remove 标志合并成一批上报,notify 按标志顺序展开,于是一次
    // `rm` 到达 recorder 的是 `Created, Deleted, Modified`,去抖队列保留了最后
    // 那个 `Modified`,recorder 于是去读一个已经不存在的文件,只留下一条
    // "read failed: No such file or directory" 的 warn。现在由磁盘上的实际状态
    // 定夺,而不是由事件顺序定夺。
    std::fs::remove_file(&alpha).context("delete alpha.txt")?;
    let after_delete = wait_for_tombstone(&session, &alpha, RECORD_TIMEOUT).await?;
    assert!(
        after_delete.len() > written.len(),
        "the tombstone must be a new version on top of the writes, not one of them: \
         before={written:?} after={after_delete:?}"
    );
    assert!(
        after_delete
            .windows(2)
            .all(|pair| pair[0].seq_no < pair[1].seq_no),
        "the tombstone must keep SeqNo strictly increasing: {after_delete:?}"
    );

    let deleted_review = session
        .domain
        .get_file_review_state(&session.id, alpha.to_string_lossy().as_ref())
        .await
        .context("get atomic review state for deleted alpha.txt")?;
    assert_eq!(
        deleted_review.current_state,
        FileContentState::Deleted as i32
    );
    assert!(!deleted_review.current_exists);
    assert!(deleted_review.current_sha256.is_empty());

    // §4.8 decline 走 WAL-first 协议并真的把文件写回磁盘。
    let declined = session
        .domain
        .decline_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            oldest.version_id,
            deleted_review.latest_seq_no,
            deleted_review.current_exists,
            deleted_review.current_sha256,
        )
        .await
        .context("decline_file_version for the oldest alpha.txt version")?;
    assert!(
        declined.restored,
        "decline_file_version must report restored=true for a valid version"
    );
    wait_for_file_contents(&alpha, ALPHA_FIRST, Duration::from_secs(5)).await?;
    assert!(declined.restored_version_id > 0);
    assert!(declined.restored_seq_no > deleted_review.latest_seq_no);

    let deletion_target = after_delete
        .last()
        .context("deleted history must end with a tombstone")?;
    let restored_review = session
        .domain
        .get_file_review_state(&session.id, alpha.to_string_lossy().as_ref())
        .await
        .context("review restored alpha.txt before restoring its tombstone")?;
    let removed = session
        .domain
        .decline_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            deletion_target.version_id,
            restored_review.latest_seq_no,
            restored_review.current_exists,
            restored_review.current_sha256,
        )
        .await
        .context("restore the historical deletion target")?;
    assert!(removed.restored);
    assert!(
        !alpha.exists(),
        "restoring a deletion target must remove the current file"
    );
    let removed_review = session
        .domain
        .get_file_review_state(&session.id, alpha.to_string_lossy().as_ref())
        .await
        .context("refresh review after restoring deletion")?;
    assert_eq!(
        removed_review.current_state,
        FileContentState::Deleted as i32
    );
    assert_eq!(
        removed_review.versions.len(),
        restored_review.versions.len() + 1,
        "restoring a deletion must append exactly one history node"
    );

    // ------------------------------------------------------------------
    // 4. 错误路径
    // ------------------------------------------------------------------
    // 没有影子历史的路径不是错误,只是没有版本。
    let never_created = session.root.join("never-created.txt");
    let no_versions = session
        .domain
        .list_file_versions(&session.id, never_created.to_string_lossy().as_ref())
        .await
        .context("list_file_versions for a path that has no shadow history")?;
    assert!(
        no_versions.versions.is_empty(),
        "a path with no shadow history must yield an empty version list, got {:?}",
        no_versions.versions
    );
    // 同样的要求,针对一个存在但从未被改过的文件。
    let untouched_versions = session
        .domain
        .list_file_versions(&session.id, untouched.to_string_lossy().as_ref())
        .await
        .context("list_file_versions for a file the watcher never saw change")?;
    assert!(
        untouched_versions.versions.is_empty(),
        "a file that was never modified must have no shadow versions, got {:?}",
        untouched_versions.versions
    );

    // 不存在的 version_id 必须报错,而且必须是一条 Error 响应而不是把连接拆掉:
    // 紧接着复用同一条连接再发一个请求,能成功才说明服务端没有因为这个错误退出
    // 读循环。
    let error = session
        .domain
        .get_file_version(&session.id, alpha.to_string_lossy().as_ref(), u64::MAX)
        .await
        .expect_err("get_file_version with an unknown version_id must fail");
    println!("get_file_version(u64::MAX) surfaced: {error:#}");
    session
        .domain
        .list_changed_files(&session.id)
        .await
        .context("the connection must survive a failed shadow request")?;

    session.domain.kill_session(&session.id).await?;
    Ok(())
}

/// §4.7 同一组调用,但文件仍然留在磁盘上 —— 这是 GUI 里最常见的形状
/// (改了文件,想看历史版本或回滚)。
///
/// 这条曾经全程落空:`resolve_path_within_root` 最后一步是
/// `canonical_ancestor.join(suffix)`,而请求路径指向一个**已存在**的文件时
/// `strip_prefix` 得到空后缀,`PathBuf::join("")` 会补一个分隔符,于是返回
/// `/root/alpha.txt/`。`compute_path_hash` 对 `to_string_lossy()` 做 blake3,
/// 带尾斜杠的写法与 recorder 记下的哈希不同,三个按路径查询的 RPC 全部查不到。
/// `Path` 的 `PartialEq` 忽略结尾分隔符,所以 `connection.rs` 里用 `assert_eq!`
/// 的单元测试看不见这个差异 —— 只有走完整链路才暴露得出来。
#[tokio::test(flavor = "multi_thread")]
async fn shadow_snapshot_round_trip_for_a_file_still_on_disk() -> Result<()> {
    let server = TestServer::spawn()?;
    let (worktree, root) = new_worktree()?;
    let session = start_session(&server, "shadow-e2e-on-disk", worktree, root).await?;

    let alpha = record_two_alpha_revisions(&session).await?;

    let versions = wait_for_file_versions(&session, &alpha, 2, RECORD_TIMEOUT).await?;
    let oldest = versions
        .first()
        .cloned()
        .context("list_file_versions returned an empty version list")?;

    let oldest_content = session
        .domain
        .get_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            oldest.version_id,
        )
        .await
        .context("get_file_version for the oldest alpha.txt version")?;
    assert_eq!(
        oldest_content.content,
        ALPHA_FIRST,
        "the oldest shadow version must hold the first bytes written, got {:?}",
        String::from_utf8_lossy(&oldest_content.content)
    );

    assert_eq!(
        std::fs::read(&alpha).context("read alpha.txt before decline")?,
        ALPHA_SECOND,
        "alpha.txt must still hold the second revision before the decline"
    );
    let review = session
        .domain
        .get_file_review_state(&session.id, alpha.to_string_lossy().as_ref())
        .await
        .context("get atomic review state for alpha.txt")?;
    assert_eq!(review.current_state, FileContentState::Text as i32);
    assert_eq!(review.current_content, ALPHA_SECOND);
    assert_eq!(review.current_sha256.len(), 32);

    let declined = session
        .domain
        .decline_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            oldest.version_id,
            review.latest_seq_no,
            review.current_exists,
            review.current_sha256,
        )
        .await
        .context("decline_file_version for the oldest alpha.txt version")?;
    assert!(
        declined.restored,
        "decline_file_version must report restored=true for a valid version"
    );
    wait_for_file_contents(&alpha, ALPHA_FIRST, Duration::from_secs(5)).await?;

    session.domain.kill_session(&session.id).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_rejects_an_intervening_same_size_write_without_overwriting_it() -> Result<()> {
    let server = TestServer::spawn()?;
    let (worktree, root) = new_worktree()?;
    let session = start_session(&server, "shadow-e2e-stale-review", worktree, root).await?;
    let alpha = record_two_alpha_revisions(&session).await?;
    let versions = wait_for_file_versions(&session, &alpha, 2, RECORD_TIMEOUT).await?;
    let oldest = versions
        .first()
        .context("list_file_versions returned an empty version list")?;
    let review = session
        .domain
        .get_file_review_state(&session.id, alpha.to_string_lossy().as_ref())
        .await
        .context("capture review baseline")?;
    assert_eq!(review.current_size, ALPHA_INTERVENING.len() as u64);

    std::fs::write(&alpha, ALPHA_INTERVENING).context("write intervening revision")?;
    let error = session
        .domain
        .decline_file_version(
            &session.id,
            alpha.to_string_lossy().as_ref(),
            oldest.version_id,
            review.latest_seq_no,
            review.current_exists,
            review.current_sha256,
        )
        .await
        .expect_err("an intervening same-size write must stale the review");
    assert!(
        error.to_string().contains("stale review"),
        "stale restore must surface an actionable error, got: {error:#}"
    );
    assert_eq!(
        std::fs::read(&alpha).context("read alpha.txt after rejected restore")?,
        ALPHA_INTERVENING,
        "a rejected restore must not overwrite the intervening bytes"
    );

    let refreshed = session
        .domain
        .get_file_review_state(&session.id, alpha.to_string_lossy().as_ref())
        .await
        .context("refresh review after stale restore")?;
    assert_eq!(refreshed.current_content, ALPHA_INTERVENING);
    assert!(
        refreshed
            .versions
            .iter()
            .all(|version| version.trigger != "decline"),
        "a stale restore must not append a restore node: {:?}",
        refreshed.versions
    );

    session.domain.kill_session(&session.id).await?;
    Ok(())
}
