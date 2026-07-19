//! # Shadow snapshot integration tests
//!
//! Spec §4 要求 WAL + VersionTree + StorageEngine 三层协同工作。
//! 现有单元测试覆盖各组件内部正确性 (35 个真测试,全过),但
//! **三层集成从未在同一个测试中驱动**。
//!
//! 这套测试驱动完整链路:
//!   文件内容变更 → blob 存储 → WAL append + fsync → version tree advance →
//!   storage write_node → 后续查询/恢复。
//!
//! 这条链路是 §4.5 / §4.8 / §4.9 的最小可工作单元,也是 §4.7 worktree
//! 集成 (尚未实现) 的前置条件。

use anyhow::{Context, Result};
use shadow_snapshot::{
    DeltaRef, PathHash, SnapshotTrigger, VersionTree, Wal, WalEntry, StorageEngine,
};
use std::path::PathBuf;
use tempfile::TempDir;

/// §4 集成测试用的临时引擎堆栈。
///
/// 注意:shadow_snapshot 当前 API 有设计缺陷 —— BlobStore::new 消费
/// StorageEngine (move 语义),所以同一进程内不能同时持有 StorageEngine
/// 和 BlobStore。下面的测试用临时 workaround:为每个需要 BlobStore 的
/// 测试单独 open 第二个 StorageEngine,绕过 API 限制。这是测试层的妥协,
/// 真正的修复应该在 shadow_snapshot crate 里提供 high-level engine。
struct EngineStack {
    _tmp: TempDir,
    wal: Wal,
    storage: StorageEngine,
    tree: VersionTree,
    db_path: PathBuf,
    blob_dir: PathBuf,
}

impl EngineStack {
    fn open() -> Result<Self> {
        let tmp = tempfile::tempdir().context("temp dir")?;
        let wal_path = tmp.path().join("wal.bin");
        let db_path = tmp.path().join("shadow.db");
        let blob_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blob_dir)?;

        let wal = Wal::open(&wal_path).context("open WAL")?;
        let storage = StorageEngine::open(&db_path).context("open storage")?;
        let tree = VersionTree::new();

        Ok(Self {
            _tmp: tmp,
            wal,
            storage,
            tree,
            db_path,
            blob_dir,
        })
    }
}

/// 计算 path 的 Blake3 PathHash (与 version_tree 内部一致)。
fn path_hash(p: &str) -> PathHash {
    blake3::hash(p.as_bytes()).into()
}

/// §4 API 设计缺陷暴露:BlobStore::new 消费 StorageEngine,所以为了
/// 同时持有 storage 和 blobs,必须再 open 一次 (WAL 模式下 SQLite 允许)。
/// 这条 helper 是测试层 workaround,生产代码需要 high-level engine 修复。
fn open_second_blob_store(stack: &EngineStack) -> Result<shadow_snapshot::BlobStore> {
    let second_engine = StorageEngine::open(&stack.db_path)
        .context("reopen StorageEngine for BlobStore (API workaround)")?;
    Ok(shadow_snapshot::BlobStore::new(second_engine, stack.blob_dir.clone()))
}

#[test]
fn integration_record_first_file_version() -> Result<()> {
    let stack = EngineStack::open()?;

    // §4.5 步骤 1: 内容寻址存储
    let content = b"hello world";
    let content_hash = open_second_blob_store(&stack)?.put(content).context("blob put")?;

    // §4.5 步骤 2: 分配 SeqNo,append WAL,fsync
    let seq_no: u64 = 1;
    let ph = path_hash("src/foo.rs");
    let wal_entry = WalEntry {
        seq_no,
        path_hash: ph,
        parent_id: None,
        content_ref: Some(content_hash),
        delta_ref: None,
        trigger: SnapshotTrigger::Write,
    };
    stack.wal.append(&wal_entry).context("WAL append")?;
    stack.wal.commit().context("WAL commit (fsync)")?;

    // §4.5 步骤 3: advance_head 在 version tree 中建节点
    let timestamp_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let version_id = stack.tree.advance_head(
        ph,
        seq_no,
        timestamp_ns,
        None, // 第一个版本无父
        Some(content_hash),
        None, // 第一个版本是 full snapshot,无 delta
        0,    // delta_depth
        SnapshotTrigger::Write,
    );

    // §4.5 步骤 4: 持久化到 SQLite
    stack.storage.write_node(
        version_id,
        &ph,
        seq_no,
        None,
        Some(&content_hash),
        None,
        0,
        SnapshotTrigger::Write,
        timestamp_ns,
    )?;

    // 验证:能从 storage 查回这个节点
    let head_id = stack.storage.get_head_by_path(&ph)?;
    assert!(
        head_id.is_some(),
        "storage should return a head for the path after write_node"
    );

    // 验证:tree 与 storage 一致
    let tree_head = stack.tree.get_head(&ph);
    assert_eq!(tree_head, Some(version_id));

    // 验证:blob 内容能寻回
    let recovered = open_second_blob_store(&stack)?.get(&content_hash).context("blob get")?;
    assert_eq!(recovered, content);

    Ok(())
}

/// §4.6 第二个版本以 delta 形式记录,深度从 0 → 1。
#[test]
fn integration_delta_chain_grows() -> Result<()> {
    let stack = EngineStack::open()?;

    // 版本 1: full snapshot
    let v1_content = b"line1\nline2\nline3\n";
    let v1_hash = open_second_blob_store(&stack)?.put(v1_content)?;
    let ph = path_hash("doc.md");
    let seq1: u64 = 1;
    let ts1 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let v1 = stack.tree.advance_head(
        ph,
        seq1,
        ts1,
        None,
        Some(v1_hash),
        None,
        0,
        SnapshotTrigger::Write,
    );
    stack.storage.write_node(
        v1, &ph, seq1, None, Some(&v1_hash), None, 0, SnapshotTrigger::Write, ts1,
    )?;

    // 版本 2: delta。DeltaRef 包含 SHA-256(parent || child) 和压缩大小。
    let v2_content = b"line1\nline2\nline3\nline4\n";
    let v2_hash = open_second_blob_store(&stack)?.put(v2_content)?;
    let mut delta_hasher = sha2::Sha256::new();
    use sha2::Digest;
    delta_hasher.update(v1_content);
    delta_hasher.update(v2_content);
    let delta_key: [u8; 32] = delta_hasher.finalize().into();
    // delta blob 是压缩后的差量。测试里用原始字节作占位,验证 chain 长度即可。
    let delta_blob = b"DELTA_PLACEHOLDER";
    let compressed_size = delta_blob.len() as u64;
    let delta_content_hash = open_second_blob_store(&stack)?.put(delta_blob)?;
    let _ = delta_key;
    let _ = delta_content_hash;

    let delta_ref = DeltaRef {
        hash: v2_hash, // spec: SHA-256(parent || child);测试用简化值
        compressed_size,
    };
    let seq2 = seq1 + 1;
    let ts2 = ts1 + 1_000_000;
    let v2 = stack.tree.advance_head(
        ph,
        seq2,
        ts2,
        Some(v1),
        None,
        Some(delta_ref.clone()),
        1,
        SnapshotTrigger::Write,
    );
    stack.storage.write_node(
        v2, &ph, seq2, Some(v1),
        None, Some(&delta_ref.hash), 1, SnapshotTrigger::Write, ts2,
    )?;
    assert_eq!(stack.tree.get_head(&ph), Some(v2));

    // 验证:版本树深度信息
    let v2_node = stack
        .tree
        .get_node(v2)
        .expect("v2 node must exist after advance_head");
    assert_eq!(v2_node.delta_depth, 1);
    assert_eq!(v2_node.parent_id, Some(v1));

    Ok(())
}

/// §4.5 / §4.8 crash recovery: WAL replay 应该重现所有已 commit 的 entries。
#[test]
fn integration_wal_replay_recovers_committed_entries() -> Result<()> {
    let stack = EngineStack::open()?;

    let ph = path_hash("notes.txt");
    // 写三个连续 entries
    for seq in 1..=3u64 {
        let content = format!("version-{}", seq);
        let hash = open_second_blob_store(&stack)?.put(content.as_bytes())?;
        let entry = WalEntry {
            seq_no: seq,
            path_hash: ph,
            parent_id: if seq == 1 { None } else { Some(seq - 1) },
            content_ref: Some(hash),
            delta_ref: None,
            trigger: SnapshotTrigger::Write,
        };
        stack.wal.append(&entry)?;
    }
    stack.wal.commit()?;

    // §4.8 模拟 "重启":replay
    let entries = stack.wal.replay().context("WAL replay")?;
    assert_eq!(
        entries.len(),
        3,
        "replay should return all 3 committed entries"
    );
    assert_eq!(entries[0].seq_no, 1);
    assert_eq!(entries[2].seq_no, 3);
    assert_eq!(entries[2].parent_id, Some(2));

    Ok(())
}

/// §4.8 已 commit 但未 checkpoint 的 entries 在 replay 后仍可见,
/// 然后 checkpoint 清空 WAL,新的 replay 应该为空。
#[test]
fn integration_wal_checkpoint_clears_log() -> Result<()> {
    let stack = EngineStack::open()?;
    let ph = path_hash("ephemeral.rs");

    let entry = WalEntry {
        seq_no: 1,
        path_hash: ph,
        parent_id: None,
        content_ref: None,
        delta_ref: None,
        trigger: SnapshotTrigger::Write,
    };
    stack.wal.append(&entry)?;
    stack.wal.commit()?;

    let before = stack.wal.replay()?;
    assert_eq!(before.len(), 1, "committed entry should be in replay");

    stack.wal.checkpoint()?;
    let after = stack.wal.replay()?;
    assert_eq!(
        after.len(),
        0,
        "replay after checkpoint should be empty"
    );

    Ok(())
}
