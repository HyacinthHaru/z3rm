//! ShadowSnapshotEngine: high-level orchestrator tying WAL + VersionTree +
//! Storage + BlobStore together per spec §4.
//!
//! The engine is the primary entry point for recording file changes,
//! querying versions, declining to prior versions, and listing history.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use blake3::Hasher as Blake3Hasher;
use rope::Rope;

use crate::delta_chain::{serialize_delta_ops, DeltaOp, DeltaReplay, D_MAX};
use crate::storage::{BlobStore, StorageEngine};
use crate::version_tree::{
    ContentHash, DeltaRef, PathHash, SeqNo, SnapshotTrigger, VersionId, VersionTree,
};
use crate::wal::{Wal, WalEntry};

/// Compute a Blake3 hash of a file path for use as `PathHash`.
fn compute_path_hash(path: &Path) -> PathHash {
    let mut hasher = Blake3Hasher::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.finalize().into()
}

/// Result of deciding the shape of a recorded version.
struct ProducedSnapshot {
    full_content: Option<ContentHash>,
    delta_ref: Option<DeltaRef>,
    depth: u8,
}

impl ProducedSnapshot {
    fn full(hash: ContentHash) -> Self {
        Self {
            full_content: Some(hash),
            delta_ref: None,
            depth: 0,
        }
    }
}

/// Convert a Rope to bytes (UTF-8; `DeltaReplay` operates on UTF-8 text semantics).
fn rope_to_bytes(rope: &Rope) -> Vec<u8> {
    rope.to_string().into_bytes()
}

/// Compute a bounded, reconstructable delta from `old` → `new`.
///
/// Emits a single `Replace` op over the differing middle region, found as the
/// longest common byte prefix and longest common byte suffix. Correct for any
/// input (applying the op reproduces `new` exactly), bounded in op count, and
/// cheap to replay on a Rope. Producing a minimal LCS-based edit is not required
/// for correctness — only that replay reconstructs the bytes.
fn compute_delta_ops(old: &[u8], new: &[u8]) -> Vec<DeltaOp> {
    let max_prefix = old.len().min(new.len());
    let prefix = (0..max_prefix)
        .take_while(|&i| old[i] == new[i])
        .count();
    let remaining_old = old.len().saturating_sub(prefix);
    let remaining_new = new.len().saturating_sub(prefix);
    let max_suffix = remaining_old.min(remaining_new);
    let mut suffix = 0;
    while suffix < max_suffix {
        let old_idx = old.len().saturating_sub(1).saturating_sub(suffix);
        let new_idx = new.len().saturating_sub(1).saturating_sub(suffix);
        if old[old_idx] != new[new_idx] {
            break;
        }
        suffix += 1;
    }
    let delete_len = remaining_old.saturating_sub(suffix);
    let tail_start = new.len().saturating_sub(suffix);
    if delete_len == 0 && tail_start == prefix {
        return Vec::new();
    }
    let text = &new[prefix..tail_start];
    vec![DeltaOp::Replace {
        offset: prefix,
        delete_len,
        text: Arc::new(Rope::from(String::from_utf8_lossy(text).into_owned())),
    }]
}

/// High-level engine that coordinates WAL, version tree, storage, and blob store.
///
/// Single-writer design: all mutations go through the engine methods.
pub struct ShadowSnapshotEngine {
    wal: Wal,
    storage: Arc<StorageEngine>,
    tree: VersionTree,
    blob_store: BlobStore,
    /// Monotonic sequence counter, shared across all paths.
    seq_no: AtomicU64,
}

impl ShadowSnapshotEngine {
    /// Open or create the engine at the given paths.
    ///
    /// Restarts preserve monotonicity and in-memory state: every persisted
    /// `version_node` is loaded from SQLite, the in-memory `VersionTree` is
    /// rebuilt (heads + ancestor tables), and the SeqNo / version-id allocators
    /// are seeded strictly above all persisted IDs so the next write never
    /// collides with or overwrites an existing row. Decline-intent WAL recovery
    /// (completing restores interrupted by a crash) is left to the caller via
    /// `recover_incomplete_restores`, since resolving `path_hash → path`
    /// requires mux's path mapping.
    pub fn open(db_path: &Path, wal_path: &Path, blob_dir: &Path) -> Result<Self> {
        let storage = Arc::new(StorageEngine::open(db_path)?);
        let wal = Wal::open(wal_path).map_err(|e| anyhow::anyhow!(e))?;
        let blob_store = BlobStore::new(Arc::clone(&storage), blob_dir.to_path_buf());

        let loaded = storage.load_nodes()?;
        let tree = VersionTree::new();
        tree.rebuild_from_nodes(loaded.nodes, loaded.max_version_id);

        // §4.5 WAL-before-write recovery: rebuild 区块后,回放 WAL 中那些
        // seq_no 高于已持久化最大值、且产生 version_node 的记录 (Write/Close/
        // Debounce)。这类条目来自崩溃在 WAL commit 之后、SQLite 写入之前的
        // 那一瞬;SQLite 缺失,但 WAL 已 fsync,据此补写内存树 + SQLite,
        // 并把 SeqNo 分配器推进到这些条目之上。Decline/DeclineDone 不在此处
        // 处理,它们由 recover_incomplete_restores 按 path 解析后再补完。
        let mut replay_max_seq = loaded.max_seq_no;
        let wal_entries = wal.replay().map_err(|e| anyhow::anyhow!(e))?;
        for entry in wal_entries {
            let produces_node = matches!(
                entry.trigger,
                SnapshotTrigger::Write | SnapshotTrigger::Close | SnapshotTrigger::Debounce
            );
            if !produces_node || entry.seq_no <= loaded.max_seq_no {
                continue;
            }
            let timestamp_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);

            // Rebuild the same node shape record_change would have persisted.
            // A WAL entry with content_ref is a full snapshot; one with delta_ref
            // is a delta node whose depth is parent.depth + 1 (parent is already
            // in the rebuilt tree, whether from SQLite load or an earlier replay).
            let (full_content, delta_ref, depth) = match entry.content_ref {
                Some(full_hash) => (Some(full_hash), None, 0u8),
                None => {
                    let delta_ref = entry
                        .delta_ref
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!(
                            "WAL replay: entry seq {} has neither full nor delta ref",
                            entry.seq_no
                        ))?;
                    let depth = entry
                        .parent_id
                        .and_then(|pid| tree.get_node(pid))
                        .map(|parent| parent.delta_depth.saturating_add(1))
                        .unwrap_or(1);
                    (None, Some(delta_ref), depth)
                }
            };

            // advance_head 用 WAL 记录的 parent_id,而非当前 tree HEAD:
            // 回放必须复刻崩溃那一瞬的链结构,而非基于部分回放后的 HEAD。
            let version_id = tree.advance_head(
                entry.path_hash,
                entry.seq_no,
                timestamp_ns,
                entry.parent_id,
                full_content,
                delta_ref.clone(),
                depth,
                entry.trigger,
            );
            storage.write_node(
                version_id,
                &entry.path_hash,
                entry.seq_no,
                entry.parent_id,
                full_content.as_ref(),
                delta_ref.as_ref().map(|d| &d.hash),
                depth,
                entry.trigger,
                timestamp_ns,
            )?;
            replay_max_seq = replay_max_seq.max(entry.seq_no);
        }

        // SeqNo 分配器从回放后的最大值 +1 起,保证跨重启严格单调。
        let start_seq = replay_max_seq.saturating_add(1).max(1);

        Ok(Self {
            wal,
            storage,
            tree,
            blob_store,
            seq_no: AtomicU64::new(start_seq),
        })
    }

    /// Record a file change. Called by the file watcher.
    ///
    /// §4.6 delta generation: when the path already has a HEAD and appending a
    /// delta would keep the chain at depth ≤ `D_MAX`, record a delta snapshot
    /// (serialized `DeltaOp`s into the blob store, keyed by content hash); the
    /// 17th change forces a full snapshot so reconstruction stays bounded
    /// within `D_MAX` rope-replay steps. WAL is appended and fsynced before any
    /// in-memory or SQLite mutation (§4.5/§4.8), so a crash after the WAL commit
    /// but before the node row is written is recovered by `open`'s WAL replay.
    pub fn record_change(&self, path: &Path, new_content: &[u8]) -> Result<VersionId> {
        let path_hash = compute_path_hash(path);
        let seq_no = self.seq_no.fetch_add(1, Ordering::AcqRel) as SeqNo;
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let parent_id = self.tree.get_head(&path_hash);

        // §4.6 Decide the snapshot shape and store exactly the blob(s) it needs,
        // before touching the WAL. A delta stores only the serialized DeltaOp
        // blob; a full stores only the content blob. This keeps refcounts honest
        // and lets WAL replay reconstruct the same node shape (no orphan blobs).
        let snapshot = self.produce_snapshot(parent_id, new_content)?;

        let entry = WalEntry {
            seq_no,
            path_hash,
            parent_id,
            // content_ref names the full snapshot blob; for a delta node the
            // content lives only in the delta, so content_ref is None and the
            // replay path rebuilds a delta node from delta_ref.
            content_ref: snapshot.full_content,
            delta_ref: snapshot.delta_ref.clone(),
            trigger: SnapshotTrigger::Write,
        };
        self.wal.append(&entry)?;
        // 单写线程路径直接 fsync 一次,保证 WAL 顺序先于任何持久状态变更。
        self.wal.commit()?;

        // WAL 已落盘：可以安全地推进内存树与 SQLite。
        let version_id = self.tree.advance_head(
            path_hash,
            seq_no,
            timestamp_ns,
            parent_id,
            snapshot.full_content,
            snapshot.delta_ref.clone(),
            snapshot.depth,
            SnapshotTrigger::Write,
        );

        self.storage.write_node(
            version_id,
            &path_hash,
            seq_no,
            parent_id,
            snapshot.full_content.as_ref(),
            snapshot.delta_ref.as_ref().map(|d| &d.hash),
            snapshot.depth,
            SnapshotTrigger::Write,
            timestamp_ns,
        )?;

        Ok(version_id)
    }

    /// Decide whether the next version is a delta or a full snapshot, and store
    /// the corresponding blob into the blob store.
    ///
    /// - Full snapshot: store `new_content`, return `full_content = Some(hash)`,
    ///   `depth = 0`. Emitted when there is no parent, the parent is missing, the
    ///   parent cannot be reconstructed, or a delta would exceed `D_MAX`.
    /// - Delta snapshot: store the serialized `DeltaOp`s transforming parent →
    ///   child, return `full_content = None`, `delta_ref = Some(...)`, `depth =
    ///   parent.depth + 1`. The chain stays ≤ `D_MAX`; the 17th change forces
    ///   full. Only the delta blob is stored (the content lives in the delta).
    fn produce_snapshot(
        &self,
        parent_id: Option<VersionId>,
        new_content: &[u8],
    ) -> Result<ProducedSnapshot> {
        let Some(parent_id) = parent_id else {
            return Ok(ProducedSnapshot::full(self.blob_store.put(new_content)?));
        };
        let Some(parent_node) = self.tree.get_node(parent_id) else {
            return Ok(ProducedSnapshot::full(self.blob_store.put(new_content)?));
        };

        let next_depth = parent_node.delta_depth.saturating_add(1);
        if next_depth > D_MAX {
            return Ok(ProducedSnapshot::full(self.blob_store.put(new_content)?));
        }

        // Reconstructing the parent needs its bytes; if the delta chain is broken
        // (missing blobs) fall back to a full snapshot rather than emit a
        // delta that can never replay.
        let parent_content = match self.read_node_content(&parent_node) {
            Ok(content) => content,
            Err(_) => return Ok(ProducedSnapshot::full(self.blob_store.put(new_content)?)),
        };
        let delta_ops = compute_delta_ops(&parent_content, new_content);
        let delta_bytes = serialize_delta_ops(&delta_ops);
        let delta_hash = self.blob_store.put(&delta_bytes)?;
        let compressed_size = delta_bytes.len() as u64;

        Ok(ProducedSnapshot {
            full_content: None,
            delta_ref: Some(DeltaRef {
                hash: delta_hash,
                compressed_size,
            }),
            depth: next_depth,
        })
    }

    /// Materialize a node's bytes: full snapshot reads the blob directly;
    /// delta-only nodes reconstruct via the delta chain.
    fn read_node_content(&self, node: &crate::version_tree::VersionNode) -> Result<Vec<u8>> {
        if let Some(full_hash) = &node.full_content {
            return self.blob_store.get(full_hash);
        }
        let rope = DeltaReplay::reconstruct(
            node,
            |id| self.tree.get_node(id),
            |hash: &[u8; 32]| self.blob_store.get(hash).ok(),
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "node {} is delta-only and not reconstructable",
                node.version_id
            )
        })?;
        Ok(rope_to_bytes(&rope))
    }

    /// Query content at a specific version. Full snapshots read the blob
    /// directly; delta-only nodes reconstruct from the delta chain (§4.6).
    pub fn query_version(&self, version_id: VersionId) -> Result<Option<Vec<u8>>> {
        let Some(node) = self.tree.get_node(version_id) else {
            return Ok(None);
        };
        self.read_node_content(&node).map(Some)
    }

    /// Return the in-memory node for a version, if present. Useful for
    /// inspecting snapshot shape (full vs delta, depth) in tests and for
    /// callers that need the chain metadata without materializing bytes.
    pub fn get_version_node(
        &self,
        version_id: VersionId,
    ) -> Option<std::sync::Arc<crate::version_tree::VersionNode>> {
        self.tree.get_node(version_id)
    }

    /// Decline (undo) to a specific version. Crash-safe per §4.8.
    ///
    /// 用真实 decline 协议把文件内容还原到目标版本：目标内容先入 BlobStore，
    /// WAL 意图 fsync 后才写回文件并 fsync，最后写 DeclineDone 完成标记。
    /// 全快照目标直接从 blob 取内容；delta-only 目标用 delta 链重建。
    pub fn decline(&self, path: &Path, target_version: VersionId) -> Result<()> {
        use crate::decline::DeclineProtocol;
        use crate::delta_chain::DeltaReplay;

        let node = self.tree.get_node(target_version).ok_or_else(|| {
            anyhow::anyhow!("version {} not found", target_version)
        })?;

        // 取回目标版本的原始字节。full 快照直接读 blob；
        // delta-only 节点走 delta 链重建。
        let target_content: Vec<u8> = if let Some(full_hash) = &node.full_content {
            self.blob_store.get(full_hash)?
        } else {
            let rope = DeltaReplay::reconstruct(
                &node,
                |id| self.tree.get_node(id),
                |hash: &[u8; 32]| self.blob_store.get(hash).ok(),
            )
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "decline: target version {} is delta-only and not reconstructable",
                    target_version
                )
            })?;
            rope.to_string().into_bytes()
        };

        let seq_no = self.seq_no.fetch_add(1, Ordering::AcqRel) as SeqNo;
        let protocol = DeclineProtocol::new(&self.wal, seq_no);
        protocol.execute(
            &self.blob_store,
            node.path_hash,
            Some(target_version),
            &target_content,
            path,
        )?;

        tracing::info!(
            version_id = target_version,
            path = ?path,
            seq_no,
            "decline: version restored"
        );
        Ok(())
    }

    /// List all versions of a file.
    pub fn list_versions(
        &self,
        path: &Path,
    ) -> Result<Vec<(VersionId, SeqNo, SnapshotTrigger)>> {
        let path_hash = compute_path_hash(path);

        let nodes = self.tree.iter_nodes();
        let mut versions: Vec<_> = nodes
            .into_iter()
            .filter(|(_, n)| n.path_hash == path_hash)
            .map(|(id, n)| (id, n.seq_no, n.trigger))
            .collect();

        versions.sort_by_key(|(_, seq, _)| *seq);
        Ok(versions)
    }
    /// 崩溃恢复：补完所有未完成的 decline 还原操作。
    ///
    /// 扫描 WAL 找出有 Decline 意图但无对应 DeclineDone 的条目，
    /// 对每个调用方提供的 `path_resolver`（path_hash → 真实路径）
    /// 解析出的路径，按 content_ref 取回 blob 重写文件并写 DeclineDone。
    /// 返回这次实际完成的数量。
    ///
    /// 路径解析失败的条目会被跳过并记录日志——上层（mux）持有 path 映射，
    /// 若映射缺失应自行告警，而非在 shadow_snapshot 层静默丢弃。
    pub fn recover_incomplete_restores(
        &self,
        path_resolver: impl Fn(&crate::version_tree::PathHash) -> Option<std::path::PathBuf>,
    ) -> Result<usize> {
        use crate::decline::DeclineProtocol;

        let pending = DeclineProtocol::recover(&self.wal)?;
        let mut completed = 0;
        for entry in &pending {
            let Some(target_path) = path_resolver(&entry.path_hash) else {
                tracing::warn!(
                    seq_no = entry.seq_no,
                    "decline recovery: path unresolvable, skipping pending intent"
                );
                continue;
            };
            DeclineProtocol::finish_restore(
                &self.wal,
                &self.blob_store,
                entry,
                &target_path,
            )?;
            completed += 1;
        }
        tracing::info!(completed, total = pending.len(), "decline recovery finished");
        Ok(completed)
    }
}
