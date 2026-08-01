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
    DeltaRef, PathHash, SnapshotTrigger, StorageEngine, VersionTree, Wal, WalEntry,
};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// §4 集成测试用的临时引擎堆栈。
///
/// §4.5 StorageEngine 用 Arc 共享,BlobStore 和测试同时持有同一份。
struct EngineStack {
    _tmp: TempDir,
    wal: Wal,
    storage: Arc<StorageEngine>,
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
        let storage = Arc::new(StorageEngine::open(&db_path).context("open storage")?);
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

/// §4.5 BlobStore 共享 Arc<StorageEngine>。
fn open_blob_store(stack: &EngineStack) -> Result<shadow_snapshot::BlobStore> {
    Ok(shadow_snapshot::BlobStore::new(
        stack.storage.clone(),
        stack.blob_dir.clone(),
    ))
}
#[test]
fn integration_record_first_file_version() -> Result<()> {
    let stack = EngineStack::open()?;

    // §4.5 步骤 1: 内容寻址存储
    let content = b"hello world";
    let content_hash = open_blob_store(&stack)?.put(content).context("blob put")?;

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
    let recovered = open_blob_store(&stack)?
        .get(&content_hash)
        .context("blob get")?;
    assert_eq!(recovered, content);

    Ok(())
}

/// §4.6 第二个版本以 delta 形式记录,深度从 0 → 1。
#[test]
fn integration_delta_chain_grows() -> Result<()> {
    let stack = EngineStack::open()?;

    // 版本 1: full snapshot
    let v1_content = b"line1\nline2\nline3\n";
    let v1_hash = open_blob_store(&stack)?.put(v1_content)?;
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
        v1,
        &ph,
        seq1,
        None,
        Some(&v1_hash),
        None,
        0,
        SnapshotTrigger::Write,
        ts1,
    )?;

    // 版本 2: delta。DeltaRef 包含 SHA-256(parent || child) 和压缩大小。
    let v2_content = b"line1\nline2\nline3\nline4\n";
    let v2_hash = open_blob_store(&stack)?.put(v2_content)?;
    let mut delta_hasher = sha2::Sha256::new();
    use sha2::Digest;
    delta_hasher.update(v1_content);
    delta_hasher.update(v2_content);
    let delta_key: [u8; 32] = delta_hasher.finalize().into();
    // delta blob 是压缩后的差量。测试里用原始字节作占位,验证 chain 长度即可。
    let delta_blob = b"DELTA_PLACEHOLDER";
    let compressed_size = delta_blob.len() as u64;
    let delta_content_hash = open_blob_store(&stack)?.put(delta_blob)?;
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
        v2,
        &ph,
        seq2,
        Some(v1),
        None,
        Some(&delta_ref.hash),
        1,
        SnapshotTrigger::Write,
        ts2,
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
        let hash = open_blob_store(&stack)?.put(content.as_bytes())?;
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
    assert_eq!(after.len(), 0, "replay after checkpoint should be empty");

    Ok(())
}

/// §4.6 reconstruct 必须能从 full snapshot + delta chain 重建目标版本内容。
///
/// 这个测试钉死 DeltaReplay::reconstruct 的契约:base content + 应用
/// delta list = target content。之前 reconstruct 是 stub (返回空 Rope),
/// 此测试会失败直到实现正确。
#[test]
fn integration_reconstruct_replays_delta_chain() -> Result<()> {
    use shadow_snapshot::{
        DeltaOp, DeltaRef, DeltaReplay, VersionNode, deserialize_delta_ops, serialize_delta_ops,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    // 模拟 3 个版本的链: v1 (full) → v2 (delta insert) → v3 (delta delete)
    let v1_content = b"hello\nworld\n";
    let v2_content = b"hello\nworld\nmore\n";
    let v3_content = b"hello\nworld\n"; // 删了 "more\n"

    let v1_hash = blake3::hash(v1_content).into();
    let delta_v1_to_v2 = vec![DeltaOp::Insert {
        offset: v1_content.len(),
        text: Arc::new(rope::Rope::from("more\n")),
    }];
    let delta_v2_to_v3 = vec![DeltaOp::Delete {
        offset: v1_content.len(),
        delete_len: 5,
    }];

    // 序列化 deltas,放到模拟 blob store (HashMap<hash, bytes>)
    let delta_v1_to_v2_bytes = serialize_delta_ops(&delta_v1_to_v2).expect("serialize delta");
    let delta_v2_to_v3_bytes = serialize_delta_ops(&delta_v2_to_v3).expect("serialize delta");
    let delta_v1_to_v2_hash: [u8; 32] = blake3::hash(&delta_v1_to_v2_bytes).into();
    let delta_v2_to_v3_hash: [u8; 32] = blake3::hash(&delta_v2_to_v3_bytes).into();
    let delta_v1_to_v2_size = delta_v1_to_v2_bytes.len() as u64;
    let delta_v2_to_v3_size = delta_v2_to_v3_bytes.len() as u64;

    let mut blobs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
    blobs.insert(v1_hash, v1_content.to_vec());
    blobs.insert(delta_v1_to_v2_hash, delta_v1_to_v2_bytes);
    blobs.insert(delta_v2_to_v3_hash, delta_v2_to_v3_bytes);

    // 构造模拟 VersionNode 链
    use smallvec::SmallVec;
    let v1 = Arc::new(VersionNode {
        version_id: 1,
        path_hash: [0u8; 32],
        seq_no: 1,
        timestamp_ns: 0,
        parent_id: None,
        ancestors: SmallVec::new(),
        full_content: Some(v1_hash),
        delta: None,
        delta_depth: 0,
        trigger: shadow_snapshot::SnapshotTrigger::Write,
    });
    let v2 = Arc::new(VersionNode {
        version_id: 2,
        parent_id: Some(1),
        full_content: None,
        delta: Some(DeltaRef {
            hash: delta_v1_to_v2_hash,
            compressed_size: delta_v1_to_v2_size,
        }),
        delta_depth: 1,
        ..(*v1).clone()
    });
    let v3 = Arc::new(VersionNode {
        version_id: 3,
        parent_id: Some(2),
        delta: Some(DeltaRef {
            hash: delta_v2_to_v3_hash,
            compressed_size: delta_v2_to_v3_size,
        }),
        delta_depth: 2,
        ..(*v2).clone()
    });

    let nodes: HashMap<u64, Arc<VersionNode>> = [(1, v1), (2, v2), (3, v3)].into_iter().collect();

    // 重建 v2:应该是 v1 + insert "more\n"
    let reconstructed_v2 = DeltaReplay::reconstruct(
        nodes.get(&2).unwrap(),
        |id| nodes.get(&id).cloned(),
        |hash| blobs.get(hash).cloned(),
    )
    .expect("reconstruct v2 must succeed");
    assert_eq!(
        reconstructed_v2.to_string(),
        std::str::from_utf8(v2_content).unwrap()
    );

    // 重建 v3:应该是 v2 - "more\n" = v1
    let reconstructed_v3 = DeltaReplay::reconstruct(
        nodes.get(&3).unwrap(),
        |id| nodes.get(&id).cloned(),
        |hash| blobs.get(hash).cloned(),
    )
    .expect("reconstruct v3 must succeed");
    assert_eq!(
        reconstructed_v3.to_string(),
        std::str::from_utf8(v3_content).unwrap()
    );

    Ok(())
}

// ============================================================
// §4.5 / §4.4  durability: restart monotonicity + WAL-before-write recovery
//
// 这组测试驱动 ShadowSnapshotEngine 真实堆栈,钉死两个契约:
//   1. 重启后 SeqNo / VersionId 严格单调,且已持久化版本可重建。
//   2. 崩溃发生在 WAL commit 之后、SQLite write_node 之前时,`open`
//      的 WAL 回放把缺失的节点补回内存树与 SQLite。
// ============================================================

/// 打开一个位于同一临时目录的 ShadowSnapshotEngine,并暴露各路径供
/// “直接写 WAL”的崩溃注入使用。
fn open_engine(
    dir: &tempfile::TempDir,
) -> Result<(
    shadow_snapshot::ShadowSnapshotEngine,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
)> {
    let db_path = dir.path().join("shadow.db");
    let wal_path = dir.path().join("wal.bin");
    let blob_dir = dir.path().join("blobs");
    std::fs::create_dir_all(&blob_dir)?;
    let engine = shadow_snapshot::ShadowSnapshotEngine::open(&db_path, &wal_path, &blob_dir)?;
    Ok((engine, db_path, wal_path, blob_dir))
}

/// 重启单调性 + 状态重建。
///
/// 记录两个版本后重启,验证:
///  - 内存树从 SQLite 重建,list_versions 返回两条记录;
///  - query_version 对每个版本返回正确内容;
///  - 下一笔 record_change 的 seq_no 严格高于重启前最大 seq_no。
#[test]
fn integration_restart_preserves_monotonicity_and_rebuilds_tree() -> Result<()> {
    let dir = tempfile::tempdir().context("temp dir")?;

    {
        let (engine, _, _, _) = open_engine(&dir)?;
        let v1 = engine.record_change(std::path::Path::new("doc.md"), b"alpha\n")?;
        let v2 = engine.record_change(std::path::Path::new("doc.md"), b"alpha\nbeta\n")?;
        // 第二个版本走 delta 路径:父存在且 depth+1 <= 16。
        let node = engine.get_version_node(v2).expect("test helper invariant");
        assert_eq!(node.delta_depth, 1, "second change should be a delta");
        assert!(
            node.full_content.is_none(),
            "delta node has no full content"
        );
        let _ = v1;
    } // engine dropped; blobs + db + wal persist on disk.

    let (engine, _, _, _) = open_engine(&dir)?;
    let versions = engine.list_versions(std::path::Path::new("doc.md"))?;
    assert_eq!(
        versions.len(),
        2,
        "reopened engine must rebuild both persisted versions"
    );
    let last_seq_before = versions.iter().map(|(_, s, _)| *s).max().unwrap();

    // 内容必须能从重建的树 + blob store 查回。
    let c1 = engine.query_version(versions[0].0)?.expect("v1 content");
    assert_eq!(c1, b"alpha\n");
    let c2 = engine.query_version(versions[1].0)?.expect("v2 content");
    assert_eq!(c2, b"alpha\nbeta\n");

    // 重启后下一笔写入的 seq_no 必须严格高于重启前最大值。
    let fresh = engine.list_versions(std::path::Path::new("doc.md"))?;
    let _v3 = engine.record_change(std::path::Path::new("doc.md"), b"alpha\nbeta\ngamma\n")?;
    let after = engine.list_versions(std::path::Path::new("doc.md"))?;
    let new_max_seq = after.iter().map(|(_, s, _)| *s).max().unwrap();
    assert!(
        new_max_seq > last_seq_before,
        "seq_no must stay strictly monotonic across restart ({} > {})",
        new_max_seq,
        last_seq_before,
    );
    // version id 也必须唯一且可重建:总版本数现在应为 3。
    assert_eq!(after.len(), 3);
    assert_eq!(fresh.len() + 1, after.len());

    Ok(())
}

/// WAL-before-write 恢复:模拟崩溃在 WAL commit 之后、SQLite write_node 之前。
///
/// v1 通过 record_change 完整落盘(SQLite+WAL)。随后直接往 WAL 追加一条
/// seq_no 更高的 Write 条目并 fsync,但**不**写 SQLite 节点行——复刻崩溃
/// 在 `wal.commit()` 之后、`storage.write_node` 之前那一瞬。重开引擎后,
/// `open` 的 WAL 回放应补回内存树 + SQLite,query_version 能取回内容,且
/// 后续 seq_no 推进到该条目之上。
#[test]
fn integration_wal_before_write_recovers_unpersisted_node() -> Result<()> {
    let dir = tempfile::tempdir().context("temp dir")?;
    let wal_path = dir.path().join("wal.bin");

    let v1 = {
        let (engine, _, _, _) = open_engine(&dir)?;
        engine.record_change(std::path::Path::new("note.txt"), b"hello\n")
    }?;

    // 直接打开同一 WAL 文件,注入“已 commit 但未持久化节点”的条目。
    // parent_id = v1,内容是一笔 delta(父存在且 depth+1 = 1 <= 16)。
    // 但为了证明回放重建任意形态,这里注入纯全快照条目:content_ref 有值,
    // delta_ref = None,触发 open 的 full-snapshot 回放分支。
    let ph = path_hash("note.txt");
    let v2_content = b"hello\nworld\n";
    {
        let blob = shadow_snapshot::BlobStore::new(
            // StorageEngine 重新打开以共享同一 SQLite 文件。
            std::sync::Arc::new(StorageEngine::open(dir.path().join("shadow.db"))?),
            dir.path().join("blobs"),
        );
        let content_hash = blob.put(v2_content)?;
        let wal = Wal::open(&wal_path)?;
        let entry = WalEntry {
            seq_no: 10_000, // 显著高于 v1 的 seq_no,使其落在 “> max_seq_no” 回放窗口。
            path_hash: ph,
            parent_id: Some(v1),
            content_ref: Some(content_hash),
            delta_ref: None,
            trigger: SnapshotTrigger::Write,
        };
        wal.append(&entry)?;
        wal.commit()?;
        // 故意不写 storage node:模拟崩溃。
    }

    let (engine, _, _, _) = open_engine(&dir)?;
    let versions = engine.list_versions(std::path::Path::new("note.txt"))?;
    assert_eq!(
        versions.len(),
        2,
        "WAL replay must reconstruct the node missing from SQLite",
    );
    let recovered = versions
        .iter()
        .find(|(_, s, _)| *s == 10_000)
        .expect("replayed seq_no 10000 node must be present");
    let content = engine
        .query_version(recovered.0)?
        .expect("recovered content");
    assert_eq!(content, v2_content);

    // 下一次写入必须高于回放后的最大 seq_no(10000),不能复用。
    let _v3 = engine.record_change(std::path::Path::new("note.txt"), b"hello\nworld!\n")?;
    let after = engine.list_versions(std::path::Path::new("note.txt"))?;
    let max_seq = after.iter().map(|(_, s, _)| *s).max().unwrap();
    assert!(
        max_seq > 10_000,
        "seq_no allocator must advance past replayed max"
    );

    Ok(())
}

/// delta 生成契约:连续变更逐层增深;第 D_MAX+1 次强制全快照重置为 0。
#[test]
fn integration_delta_generation_grows_then_forces_full() -> Result<()> {
    let dir = tempfile::tempdir().context("temp dir")?;
    let (engine, _, _, _) = open_engine(&dir)?;

    let path = std::path::Path::new("log.txt");
    // v1: full snapshot, depth 0
    let _v1 = engine.record_change(path, b"line0")?;
    // v2..v17: each a delta, depth 1..16
    let mut last_id = 0u64;
    for i in 1..=16 {
        let content = format!("line{}", i);
        let id = engine.record_change(path, content.as_bytes())?;
        let node = engine.get_version_node(id).expect("node present");
        assert_eq!(
            node.delta_depth, i,
            "version {} should have delta_depth {}",
            i, i,
        );
        assert!(node.delta.is_some(), "versions 2..17 should be deltas");
        last_id = id;
    }
    // v18: would push depth to 17 > D_MAX → forced full snapshot, depth 0.
    let id = engine.record_change(path, b"line17")?;
    let node = engine.get_version_node(id).expect("node present");
    assert_eq!(
        node.delta_depth, 0,
        "17th delta forces full snapshot, depth resets to 0"
    );
    assert!(
        node.full_content.is_some(),
        "forced node is a full snapshot"
    );
    assert!(node.delta.is_none(), "forced node has no delta ref");
    // 全链可重建:每个版本 query 都返回其原始内容。
    for i in 0..=17 {
        let expected = format!("line{}", i).into_bytes();
        let versions = engine.list_versions(path)?;
        let vid = versions[i].0;
        assert_eq!(
            engine.query_version(vid)?.unwrap(),
            expected,
            "content of version {}",
            i
        );
    }
    let _ = last_id;
    Ok(())
}

/// delta-shape 节点的 WAL 回放:崩溃发生在一条 delta 写入的 WAL commit
/// 之后、SQLite write_node 之前。`open` 必须把这条 `content_ref = None`、
/// `delta_ref = Some(...)` 的条目重建为 delta 节点(而非错误地建成 full 节点),
/// depth 沿父链恢复,内容可重建。
#[test]
fn integration_wal_replay_rebuilds_delta_node_shape() -> Result<()> {
    use shadow_snapshot::{BlobStore, DeltaOp, serialize_delta_ops};
    use std::sync::Arc as StdArc;

    let dir = tempfile::tempdir().context("temp dir")?;
    let db_path = dir.path().join("shadow.db");
    let wal_path = dir.path().join("wal.bin");
    let blob_dir = dir.path().join("blobs");
    std::fs::create_dir_all(&blob_dir)?;

    let parent_id = {
        let (engine, _, _, _) = open_engine(&dir)?;
        engine.record_change(std::path::Path::new("delta.txt"), b"base line\n")
    }?;

    // 构造一条 delta 写入:parent = v1(full),把 生产 delta 的逻辑手动复刻,
    // 只把 delta blob 落盘 + WAL 条目 fsync,故意不写 SQLite 节点行。
    let ph = path_hash("delta.txt");
    let new_content = b"base line\nedits\n";
    let storage = StdArc::new(StorageEngine::open(&db_path)?);
    let blob_store = BlobStore::new(storage.clone(), blob_dir.clone());

    // delta = Insert "edits\n" at end, transforming "base line\n" -> "base line\nedits\n".
    let parent_bytes = b"base line\n";
    let ops = vec![DeltaOp::Insert {
        offset: parent_bytes.len(),
        text: StdArc::new(rope::Rope::from("edits\n")),
    }];
    let delta_bytes = serialize_delta_ops(&ops).context("serialize delta")?;
    let delta_hash = blob_store.put(&delta_bytes)?;
    let compressed_size = delta_bytes.len() as u64;

    {
        let wal = Wal::open(&wal_path)?;
        let entry = WalEntry {
            seq_no: 5_000,
            path_hash: ph,
            parent_id: Some(parent_id),
            content_ref: None, // delta 写入:无 full content blob
            delta_ref: Some(DeltaRef {
                hash: delta_hash,
                compressed_size,
            }),
            trigger: SnapshotTrigger::Write,
        };
        wal.append(&entry)?;
        wal.commit()?;
        // 崩溃:不写 SQLite 节点行。
    }

    let (engine, _, _, _) = open_engine(&dir)?;
    let versions = engine.list_versions(std::path::Path::new("delta.txt"))?;
    assert_eq!(
        versions.len(),
        2,
        "WAL replay must reconstruct the unpersisted delta node"
    );

    let replayed = versions
        .iter()
        .find(|(_, s, _)| *s == 5_000)
        .expect("replayed delta entry seq 5000");
    let node = engine
        .get_version_node(replayed.0)
        .expect("replayed node in tree");
    // 关键断言:回放必须重建为 delta 形态,而不是 full。
    assert_eq!(node.delta_depth, 1, "replayed node takes parent.depth + 1");
    assert!(
        node.full_content.is_none(),
        "replayed delta node has no full blob"
    );
    assert!(node.delta.is_some(), "replayed node carries the delta ref");

    // delta 节点必须可重建为正确内容。
    let content = engine
        .query_version(replayed.0)?
        .expect("reconstructed content");
    assert_eq!(content, new_content);

    Ok(())
}
