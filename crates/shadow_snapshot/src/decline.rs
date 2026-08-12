//! Decline：crash-safe decline 协议，WAL-first（§4.8）
//!
//! 顺序：
//! 1. 将目标内容存入 content-addressed BlobStore（可被 recovery 按 hash 取回）
//! 2. 追加 WAL Decline 意图（content_ref 指向目标内容 blob），fsync
//! 3. 原子写回目标文件（写临时文件 → rename）→ fsync 文件 + fsync 父目录
//! 4. 追加 WAL DeclineDone 完成标记，fsync；recovery 据此跳过已完成条目
//!
//! 崩溃恢复：
//! - 崩溃在 2-3 之间：WAL 有 Decline、无 DeclineDone、文件未变 → replay 取
//!   回 content_ref 对应 blob，重做步骤 3，再写 DeclineDone
//! - 崩溃在 3-4 之间：文件已持久化，replay 重做步骤 3（幂等）并写 DeclineDone
//!
//! `recover` 只返回“无对应 DeclineDone”的 Decline 条目，保证恢复幂等；
//! `check_pending` 同理，避免 watcher 对已完成还原重复打快照。

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use tracing::{info, warn};

use crate::storage::BlobStore;
use crate::version_tree::{ContentHash, PathHash, SeqNo, SnapshotTrigger, VersionId};
use crate::wal::{Wal, WalEntry};

/// Decline 协议执行器
pub struct DeclineProtocol<'a> {
    /// WAL 引用
    wal: &'a Wal,
    /// 本次操作的序列号
    seq_no: SeqNo,
}

impl<'a> DeclineProtocol<'a> {
    /// 创建 Decline 协议
    pub fn new(wal: &'a Wal, seq_no: SeqNo) -> Self {
        Self { wal, seq_no }
    }

    /// Restore the target file after durably recording the decline intent.
    /// The caller must persist the corresponding version node before calling
    /// `mark_done`; otherwise recovery must continue to treat this as pending.
    pub fn prepare_restore(
        &self,
        blob_store: &BlobStore,
        path_hash: PathHash,
        parent_id: Option<VersionId>,
        target_content: &[u8],
        target_path: &Path,
    ) -> Result<ContentHash> {
        // 步骤 1: 目标内容入 BlobStore（content-addressed，可去重），
        // 保证 crash recovery 能按 content_ref 取回原始字节。
        let content_hash = blob_store.put(target_content)?;

        // 步骤 2: 追加 Decline 意图并 fsync，先于任何文件写入。
        let intent = WalEntry {
            seq_no: self.seq_no,
            path_hash,
            parent_id,
            content_ref: Some(content_hash),
            delta_ref: None,
            trigger: SnapshotTrigger::Decline,
        };
        self.wal.append(&intent)?;
        self.wal.commit()?;

        info!(
            seq_no = self.seq_no,
            hash = ?content_hash,
            "decline: intent appended and fsynced"
        );

        Self::restore_file(blob_store, &content_hash, target_path)?;
        Ok(content_hash)
    }

    /// Record and fsync a content-less restore intent before removing a file.
    pub fn prepare_delete(
        &self,
        path_hash: PathHash,
        parent_id: Option<VersionId>,
        target_path: &Path,
    ) -> Result<()> {
        let intent = WalEntry {
            seq_no: self.seq_no,
            path_hash,
            parent_id,
            content_ref: None,
            delta_ref: None,
            trigger: SnapshotTrigger::Decline,
        };
        self.wal.append(&intent)?;
        self.wal.commit()?;
        Self::remove_file(target_path)
    }

    fn remove_file(target_path: &Path) -> Result<()> {
        match std::fs::remove_file(target_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("decline: failed to remove target file"),
        }
        if let Some(parent_dir) = target_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::File::open(parent_dir)
                .context("decline: failed to open parent dir for fsync")?
                .sync_all()
                .context("decline: failed to fsync parent dir")?;
        }
        Ok(())
    }

    /// 写回目标文件并 fsync 文件 + 父目录（原子 replace）。
    ///
    /// 用临时文件 + rename 保证原子性：读者要么看到旧版本，要么看到完整新版本，
    /// 不会读到部分写入。父目录 fsync 保证目录条目在崩溃后仍然存在。
    fn restore_file(
        blob_store: &BlobStore,
        content_hash: &ContentHash,
        target_path: &Path,
    ) -> Result<()> {
        let content = blob_store
            .get(content_hash)
            .context("decline: target content blob missing")?;

        let parent = target_path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent_dir) = parent {
            std::fs::create_dir_all(parent_dir)
                .context("decline: failed to create parent directory")?;
        }

        let tmp_path = target_path.with_extension("decline_tmp");
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
                .context("decline: failed to open temp file")?;
            file.write_all(&content)
                .context("decline: failed to write restored content")?;
            file.sync_all()
                .context("decline: failed to fsync restored file")?;
        }

        std::fs::rename(&tmp_path, target_path)
            .context("decline: failed to atomically replace target")?;

        if let Some(parent_dir) = parent {
            let dir = std::fs::File::open(parent_dir)
                .context("decline: failed to open parent dir for fsync")?;
            dir.sync_all()
                .context("decline: failed to fsync parent dir")?;
        }

        info!(path = ?target_path, "decline: file restored and fsynced");
        Ok(())
    }

    pub fn mark_done(&self, path_hash: PathHash, content_hash: Option<ContentHash>) -> Result<()> {
        let done = WalEntry {
            seq_no: self.seq_no,
            path_hash,
            parent_id: None,
            content_ref: content_hash,
            delta_ref: None,
            trigger: SnapshotTrigger::DeclineDone,
        };
        self.wal.append(&done)?;
        self.wal.commit()?;
        Ok(())
    }

    /// 计算内容 SHA-256 哈希（与 BlobStore 一致）
    pub fn compute_hash(data: &[u8]) -> ContentHash {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher.finalize().into()
    }

    /// 检查是否有尚未完成的 decline 意图匹配给定路径。
    ///
    /// Watcher 调用：若 WAL 中存在 Decline 但无对应 DeclineDone，
    /// 说明该路径上的还原操作尚未结束（或恢复未完成），watcher 应跳过这次快照。
    pub fn check_pending(wal: &Wal, path_hash: PathHash) -> Result<Option<ContentHash>> {
        let pending = Self::recover(wal)?;
        for entry in &pending {
            if entry.path_hash == path_hash {
                return Ok(entry.content_ref);
            }
        }
        Ok(None)
    }

    /// 崩溃恢复：返回尚未完成的 decline 意图（有 Decline 但无匹配 DeclineDone）。
    ///
    /// 匹配键为 (path_hash, seq_no, content_ref)：同一操作的意图与完成标记
    /// 三者一致才视为已完成。这样恢复是幂等的——重启多次只返回相同的未完成集合。
    pub fn recover(wal: &Wal) -> Result<Vec<WalEntry>> {
        let entries = wal.replay()?;

        let mut done: std::collections::HashSet<(PathHash, SeqNo, Option<ContentHash>)> =
            std::collections::HashSet::new();
        for entry in &entries {
            if entry.trigger == SnapshotTrigger::DeclineDone {
                done.insert((entry.path_hash, entry.seq_no, entry.content_ref));
            }
        }

        let pending: Vec<WalEntry> = entries
            .into_iter()
            .filter(|e| {
                e.trigger == SnapshotTrigger::Decline
                    && !done.contains(&(e.path_hash, e.seq_no, e.content_ref))
            })
            .collect();

        if !pending.is_empty() {
            warn!(
                count = pending.len(),
                "decline: incomplete intents to recover"
            );
        }

        Ok(pending)
    }

    pub(crate) fn apply_intent(
        blob_store: &BlobStore,
        entry: &WalEntry,
        target_path: &Path,
    ) -> Result<()> {
        match entry.content_ref {
            Some(content_hash) => Self::restore_file(blob_store, &content_hash, target_path),
            None => Self::remove_file(target_path),
        }
    }

    pub(crate) fn mark_entry_done(wal: &Wal, entry: &WalEntry) -> Result<()> {
        let done = WalEntry {
            seq_no: entry.seq_no,
            path_hash: entry.path_hash,
            parent_id: None,
            content_ref: entry.content_ref,
            delta_ref: None,
            trigger: SnapshotTrigger::DeclineDone,
        };
        wal.append(&done)?;
        wal.commit()?;
        Ok(())
    }

    pub fn finish_restore(
        wal: &Wal,
        blob_store: &BlobStore,
        entry: &WalEntry,
        target_path: &Path,
    ) -> Result<()> {
        Self::apply_intent(blob_store, entry, target_path)?;
        Self::mark_entry_done(wal, entry)?;
        info!(
            seq_no = entry.seq_no,
            "decline: recovery completed and marked done"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 构造一个临时的 (StorageEngine, BlobStore, Wal, dir) 测试栈。
    struct DeclineStack {
        _dir: TempDir,
        blob_store: BlobStore,
        wal: Wal,
    }

    impl DeclineStack {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let storage = std::sync::Arc::new(
                crate::storage::StorageEngine::open(dir.path().join("test.db")).unwrap(),
            );
            let blob_store = BlobStore::new(storage, dir.path().join("blobs"));
            let wal = Wal::open(dir.path().join("test.wal")).unwrap();
            Self {
                _dir: dir,
                blob_store,
                wal,
            }
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self._dir.path().join(name)
        }
    }

    #[test]
    fn test_decline_protocol_full() {
        let stack = DeclineStack::new();
        let file_path = stack.path("target.txt");
        let path_hash: PathHash = [0xAA; 32];

        let protocol = DeclineProtocol::new(&stack.wal, 1);
        let content = b"decline target content";
        let hash = protocol
            .prepare_restore(&stack.blob_store, path_hash, None, content, &file_path)
            .unwrap();
        protocol.mark_done(path_hash, Some(hash)).unwrap();

        // 文件已写回
        let written = std::fs::read(&file_path).unwrap();
        assert_eq!(written, content.as_slice());

        // WAL 里有 Decline 意图 + DeclineDone 完成标记两条
        let entries = stack.wal.replay().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].trigger, SnapshotTrigger::Decline);
        assert_eq!(entries[0].content_ref, Some(hash));
        assert_eq!(entries[1].trigger, SnapshotTrigger::DeclineDone);
        assert_eq!(entries[1].content_ref, Some(hash));

        // 已完成的操作不应再出现在恢复集合里
        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert!(pending.is_empty());

        // blob 可按 hash 取回（recovery 依赖此）
        let blob = stack.blob_store.get(&hash).unwrap();
        assert_eq!(blob, content.as_slice());
    }

    #[test]
    fn test_decline_pending_check() {
        let stack = DeclineStack::new();

        let content_hash = DeclineProtocol::compute_hash(b"test");
        // 只有 Decline 意图、没有 DeclineDone → pending
        stack
            .wal
            .append(&WalEntry {
                seq_no: 1,
                path_hash: [0xBB; 32],
                parent_id: None,
                content_ref: Some(content_hash),
                delta_ref: None,
                trigger: SnapshotTrigger::Decline,
            })
            .unwrap();
        stack.wal.commit().unwrap();

        let found = DeclineProtocol::check_pending(&stack.wal, [0xBB; 32]).unwrap();
        assert_eq!(found, Some(content_hash));
        let found = DeclineProtocol::check_pending(&stack.wal, [0xCC; 32]).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_decline_pending_cleared_after_done() {
        // 加上 DeclineDone 后，pending 应清空（恢复幂等性）
        let stack = DeclineStack::new();
        let content_hash = DeclineProtocol::compute_hash(b"done");
        stack
            .wal
            .append(&WalEntry {
                seq_no: 7,
                path_hash: [0x11; 32],
                parent_id: None,
                content_ref: Some(content_hash),
                delta_ref: None,
                trigger: SnapshotTrigger::Decline,
            })
            .unwrap();
        stack
            .wal
            .append(&WalEntry {
                seq_no: 7,
                path_hash: [0x11; 32],
                parent_id: None,
                content_ref: Some(content_hash),
                delta_ref: None,
                trigger: SnapshotTrigger::DeclineDone,
            })
            .unwrap();
        stack.wal.commit().unwrap();

        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert!(
            pending.is_empty(),
            "completed decline must not be recovered"
        );
        let found = DeclineProtocol::check_pending(&stack.wal, [0x11; 32]).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_decline_recover_skips_write_entries() {
        let stack = DeclineStack::new();
        // 只有 Decline 意图（无 Done）+ 一个普通 Write → recover 只返回 Decline
        stack
            .wal
            .append(&WalEntry {
                seq_no: 1,
                path_hash: [0xDD; 32],
                parent_id: None,
                content_ref: Some(DeclineProtocol::compute_hash(b"recovery test")),
                delta_ref: None,
                trigger: SnapshotTrigger::Decline,
            })
            .unwrap();
        stack
            .wal
            .append(&WalEntry {
                seq_no: 2,
                path_hash: [0xEE; 32],
                parent_id: None,
                content_ref: None,
                delta_ref: None,
                trigger: SnapshotTrigger::Write,
            })
            .unwrap();
        stack.wal.commit().unwrap();

        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].trigger, SnapshotTrigger::Decline);
    }

    #[test]
    fn test_decline_crash_before_file_write_then_recover() {
        // 模拟崩溃在 WAL 意图 fsync 之后、文件写入之前。
        let stack = DeclineStack::new();
        let file_path = stack.path("target.txt");

        let content = b"crash recovery content";
        let content_hash = stack.blob_store.put(content).unwrap();
        stack
            .wal
            .append(&WalEntry {
                seq_no: 1,
                path_hash: [0xFF; 32],
                parent_id: None,
                content_ref: Some(content_hash),
                delta_ref: None,
                trigger: SnapshotTrigger::Decline,
            })
            .unwrap();
        stack.wal.commit().unwrap();

        // 文件尚未写入（崩溃点）
        assert!(!file_path.exists());

        // recovery 发现一个未完成意图
        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].content_ref, Some(content_hash));

        // 完成恢复：按 content_ref 取回 blob，重写文件，写 DeclineDone
        DeclineProtocol::finish_restore(&stack.wal, &stack.blob_store, &pending[0], &file_path)
            .unwrap();

        // 文件已写回，内容匹配
        let written = std::fs::read(&file_path).unwrap();
        assert_eq!(written, content);

        // 恢复后再次 recover → 空（幂等）
        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_decline_crash_after_file_write_before_done() {
        // 模拟崩溃在文件 fsync 之后、DeclineDone 之前。
        // 此时文件已持久化，recovery 应幂等重写并补写 DeclineDone。
        let stack = DeclineStack::new();
        let file_path = stack.path("target.txt");

        let content = b"after file write before done";
        let content_hash = stack.blob_store.put(content).unwrap();
        stack
            .wal
            .append(&WalEntry {
                seq_no: 3,
                path_hash: [0x42; 32],
                parent_id: None,
                content_ref: Some(content_hash),
                delta_ref: None,
                trigger: SnapshotTrigger::Decline,
            })
            .unwrap();
        stack.wal.commit().unwrap();
        // 直接写文件（模拟 execute 走到了步骤 3 之后崩溃，步骤 4 未写）
        std::fs::write(&file_path, content).unwrap();

        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert_eq!(pending.len(), 1);

        DeclineProtocol::finish_restore(&stack.wal, &stack.blob_store, &pending[0], &file_path)
            .unwrap();

        // 内容仍正确（幂等重写）
        let written = std::fs::read(&file_path).unwrap();
        assert_eq!(written, content);

        // 再次 recover → 空
        let pending = DeclineProtocol::recover(&stack.wal).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_decline_atomic_replace_preserves_concurrent_view() {
        // 已存在文件被 decline 还原：用原子 rename，不会出现部分写入。
        let stack = DeclineStack::new();
        let file_path = stack.path("existing.txt");
        std::fs::write(&file_path, b"old content that should be fully replaced").unwrap();

        let new_content = b"new restored content";
        let protocol = DeclineProtocol::new(&stack.wal, 1);
        protocol
            .prepare_restore(&stack.blob_store, [0x77; 32], None, new_content, &file_path)
            .unwrap();

        let written = std::fs::read(&file_path).unwrap();
        assert_eq!(written, new_content.as_slice());
        // 临时文件应被 rename 消费，不再残留
        let tmp = file_path.with_extension("decline_tmp");
        assert!(
            !tmp.exists(),
            "temp file must not linger after atomic rename"
        );
    }
}
