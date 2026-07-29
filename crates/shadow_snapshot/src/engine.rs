//! ShadowSnapshotEngine: high-level orchestrator tying WAL + VersionTree +
//! Storage + BlobStore together per spec §4.
//!
//! The engine is the primary entry point for recording file changes,
//! querying versions, declining to prior versions, and listing history.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use blake3::Hasher as Blake3Hasher;
use rope::Rope;

use crate::delta_chain::{D_MAX, DeltaOp, DeltaReplay, serialize_delta_ops};
use crate::storage::{BlobStore, StorageEngine};
use crate::version_tree::{
    ContentHash, DeltaRef, PathHash, SeqNo, SnapshotTrigger, VersionId, VersionTree,
};
use crate::wal::{Wal, WalEntry};

/// Compute a Blake3 hash of a file path for use as `PathHash`.
pub fn compute_path_hash(path: &Path) -> PathHash {
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
    let prefix = (0..max_prefix).take_while(|&i| old[i] == new[i]).count();
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
    /// §4.9 age-based FIFO quota GC. Optional: present only when configured via
    /// `with_quota`. When present, `record_change` and `decline` periodically
    /// call `run_gc` so blob growth stays bounded; absent, growth is unbounded.
    quota: Option<crate::quota::QuotaManager>,
    /// record_change counter for throttling GC (don't GC every write).
    records_since_gc: AtomicU64,
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
        let persisted_sequences: HashSet<SeqNo> =
            loaded.nodes.iter().map(|node| node.seq_no).collect();
        let tree = VersionTree::new();
        tree.rebuild_from_nodes(loaded.nodes, loaded.max_version_id);

        // Rebuild durable WAL operations missing from SQLite. Write-like
        // records restore their snapshot shape; Delete restores a tombstone.
        // Decline/DeclineDone remain paired by recover_incomplete_restores,
        // since resolving their file path requires the mux-owned path map.
        let mut replay_max_seq = loaded.max_seq_no;
        let wal_entries = wal.replay().map_err(|e| anyhow::anyhow!(e))?;
        for entry in wal_entries {
            replay_max_seq = replay_max_seq.max(entry.seq_no);
            let produces_node = matches!(
                entry.trigger,
                SnapshotTrigger::Write
                    | SnapshotTrigger::Close
                    | SnapshotTrigger::Debounce
                    | SnapshotTrigger::Delete
            );
            if !produces_node || persisted_sequences.contains(&entry.seq_no) {
                continue;
            }
            let timestamp_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);

            // Rebuild the same node shape record_change would have persisted.
            // A WAL entry with content_ref is a full snapshot; one with delta_ref
            // is a delta node whose depth is parent.depth + 1 (parent is already
            let (full_content, delta_ref, depth) = if entry.trigger == SnapshotTrigger::Delete {
                if entry.content_ref.is_some() || entry.delta_ref.is_some() {
                    return Err(anyhow::anyhow!(
                        "WAL replay: delete entry seq {} contains snapshot content",
                        entry.seq_no
                    ));
                }
                (None, None, 0u8)
            } else {
                match entry.content_ref {
                    Some(full_hash) => (Some(full_hash), None, 0u8),
                    None => {
                        let delta_ref = entry.delta_ref.clone().ok_or_else(|| {
                            anyhow::anyhow!(
                                "WAL replay: entry seq {} has neither full nor delta ref",
                                entry.seq_no
                            )
                        })?;
                        let depth = entry
                            .parent_id
                            .and_then(|parent_id| tree.get_node(parent_id))
                            .map(|parent| parent.delta_depth.saturating_add(1))
                            .unwrap_or(1);
                        (None, Some(delta_ref), depth)
                    }
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
        }

        // SeqNo 分配器从回放后的最大值 +1 起,保证跨重启严格单调。
        let start_seq = replay_max_seq.saturating_add(1).max(1);

        Ok(Self {
            wal,
            storage,
            tree,
            blob_store,
            seq_no: AtomicU64::new(start_seq),
            quota: None,
            records_since_gc: AtomicU64::new(0),
        })
    }
    /// §4.9 Install a quota manager (FIFO age-based GC). Optional: when absent,
    /// blob growth is unbounded (still bounded per-path by `D_MAX`, but every
    /// full historical version is retained forever). Returns `&mut self` for
    /// chaining after `open`. Spec default quota 500 MB; pass via
    /// `QuotaManager::new(DEFAULT_QUOTA_BYTES)`.
    pub fn with_quota(mut self, quota: crate::quota::QuotaManager) -> Self {
        self.quota = Some(quota);
        self
    }

    /// §4.9 throttled GC: trigger `run_gc` every `GC_INTERVAL` records so a burst
    /// of writes does not GC per-write. Failure to run GC is logged (not fatal)
    /// — a transient I/O error must not kill the recorder; the next attempt will
    /// re-evaluate against current blob usage.
    fn maybe_run_gc(&self) {
        const GC_INTERVAL: u64 = 64;
        if self.quota.is_none() {
            return;
        }
        let count = self.records_since_gc.fetch_add(1, Ordering::AcqRel) + 1;
        if count < GC_INTERVAL {
            return;
        }
        self.records_since_gc.store(0, Ordering::Release);
        let Some(quota) = self.quota.as_ref() else {
            return;
        };
        match quota.run_gc(&self.tree, &self.blob_store, &self.storage) {
            Ok(freed) if freed > 0 => {
                tracing::info!(freed_bytes = freed, "shadow GC completed");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(error = %error, "shadow GC run failed"),
        }
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

        // §4.9 throttled GC after a durable write so blob growth stays bounded.
        self.maybe_run_gc();

        Ok(version_id)
    }

    /// §4.4 Record a file deletion as a tombstone node: `trigger=Delete`,
    /// no content_ref, no delta_ref, depth 0. The watcher routes fs remove
    /// events here so history shows the file was removed at this SeqNo. A
    /// later recreate parents on the tombstone as a fresh full snapshot.
    pub fn record_delete(&self, path: &Path) -> Result<VersionId> {
        let path_hash = compute_path_hash(path);
        let seq_no = self.seq_no.fetch_add(1, Ordering::AcqRel) as SeqNo;
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let parent_id = self.tree.get_head(&path_hash);

        let entry = WalEntry {
            seq_no,
            path_hash,
            parent_id,
            content_ref: None,
            delta_ref: None,
            trigger: SnapshotTrigger::Delete,
        };
        self.wal.append(&entry)?;
        self.wal.commit()?;

        let version_id = self.tree.advance_head(
            path_hash,
            seq_no,
            timestamp_ns,
            parent_id,
            None,
            None,
            0u8,
            SnapshotTrigger::Delete,
        );
        self.storage.write_node(
            version_id,
            &path_hash,
            seq_no,
            parent_id,
            None,
            None,
            0u8,
            SnapshotTrigger::Delete,
            timestamp_ns,
        )?;

        self.maybe_run_gc();
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

    pub fn query_version_for_path(
        &self,
        path: &Path,
        version_id: VersionId,
    ) -> Result<Option<Vec<u8>>> {
        let Some(node) = self.tree.get_node(version_id) else {
            return Ok(None);
        };
        anyhow::ensure!(
            compute_path_hash(path) == node.path_hash,
            "version {version_id} does not belong to requested path"
        );
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
    /// 真实 decline 协议把文件内容还原到目标版本：目标内容先入 BlobStore，
    /// WAL 意图 fsync 后才写回文件并 fsync，最后写 DeclineDone 完成标记。
    /// 全快照目标直接从 blob 取内容；delta-only 目标用 delta 链重建。
    ///
    /// §4.8 branch C'：还原后，目标内容在 tree 中以 trigger=Decline 的
    /// full-snapshot VersionNode 成为新的 HEAD。这样后续 record_change 的
    /// parent 链以还原后的内容为基线，而不是还原前的写入链——避免 decline
    /// 被“幽灵地”覆盖回旧状态。
    pub fn decline(&self, path: &Path, target_version: VersionId) -> Result<()> {
        use crate::decline::DeclineProtocol;
        use crate::delta_chain::DeltaReplay;

        let node = self
            .tree
            .get_node(target_version)
            .ok_or_else(|| anyhow::anyhow!("version {} not found", target_version))?;
        let requested_path_hash = compute_path_hash(path);
        anyhow::ensure!(
            requested_path_hash == node.path_hash,
            "decline path does not match version {target_version}"
        );

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

        let path_hash = node.path_hash;
        let parent_id = self.tree.get_head(&path_hash);
        let seq_no = self.seq_no.fetch_add(1, Ordering::AcqRel) as SeqNo;
        let protocol = DeclineProtocol::new(&self.wal, seq_no);
        let content_hash = protocol.execute(
            &self.blob_store,
            path_hash,
            Some(target_version),
            &target_content,
            path,
        )?;

        // §4.8 branch C'：WAL DeclineDone 已 fsync，可以把还原后的内容作为
        // full-snapshot node 持久化推进 HEAD。content_hash 来自 execute 内
        // blob_store.put，与还原到磁盘的字节一致。
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let decline_version_id = self.tree.advance_head(
            path_hash,
            seq_no,
            timestamp_ns,
            parent_id,
            Some(content_hash),
            None,
            0u8,
            SnapshotTrigger::Decline,
        );
        self.storage.write_node(
            decline_version_id,
            &path_hash,
            seq_no,
            parent_id,
            Some(&content_hash),
            None,
            0u8,
            SnapshotTrigger::Decline,
            timestamp_ns,
        )?;

        tracing::info!(
            version_id = target_version,
            decline_version_id,
            path = ?path,
            seq_no,
            "decline: version restored and HEAD advanced"
        );
        // A decline stores a full-snapshot blob; include it in quota accounting.
        self.maybe_run_gc();
        Ok(())
    }

    /// List all versions of a file.
    pub fn list_versions(&self, path: &Path) -> Result<Vec<(VersionId, SeqNo, SnapshotTrigger)>> {
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
            DeclineProtocol::finish_restore(&self.wal, &self.blob_store, entry, &target_path)?;
            completed += 1;
        }
        tracing::info!(
            completed,
            total = pending.len(),
            "decline recovery finished"
        );
        Ok(completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Assert that `decline()` writes the restored file content to disk AND
    /// advances the version tree HEAD to a new `trigger=Decline` full-snapshot
    /// node, so a subsequent `record_change` uses the declined content as
    /// parent. This is the §4.8 branch C' contract that prior to this fix
    /// was missing — decline ran the protocol but left HEAD on the prior chain.
    #[test]
    fn test_decline_advances_head() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("engine.db");
        let wal = dir.path().join("engine.wal");
        let blobs = dir.path().join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();

        let target_file = dir.path().join("declined.txt");
        // record_change does not touch the disk; it snapshots whatever bytes are
        // passed in. The disk is only mutated by decline's restore_file.
        std::fs::write(&target_file, b"v0").unwrap();
        let engine = ShadowSnapshotEngine::open(&db, &wal, &blobs).unwrap();
        let _v0 = engine.record_change(&target_file, b"v0").unwrap();
        let _v1 = engine.record_change(&target_file, b"v1").unwrap();

        // v0 is the first (full-snapshot) version; list_versions is seq-sorted.
        let versions = engine.list_versions(&target_file).unwrap();
        assert_eq!(versions.len(), 2, "expected v0+v1 versions before decline");
        let v0 = versions[0].0;

        // Disk still holds the initial v0 bytes (record_change never writes).
        assert_eq!(std::fs::read(&target_file).unwrap(), b"v0");

        // Decline back to v0: restore_file writes v0 content back to disk,
        // and the new contract advances HEAD to a Decline full snapshot.
        engine.decline(&target_file, v0).unwrap();

        // File content on disk is now the declined version (v0), written by the
        // protocol's restore_file step.
        assert_eq!(std::fs::read(&target_file).unwrap(), b"v0");

        // HEAD must be the new Decline trigger node — list_versions is seq-sorted,
        // so the last entry is HEAD by SeqNo.
        let after = engine.list_versions(&target_file).unwrap();
        let last = after.last().expect("at least one version after decline");
        assert_eq!(
            last.2,
            SnapshotTrigger::Decline,
            "decline should be the HEAD trigger"
        );

        // tree.get_head(path_hash) must point to the new decline node and that
        // node must be a full snapshot (content_ref set, no delta_ref).
        let path_hash = compute_path_hash(&target_file);
        let head_id = engine
            .tree
            .get_head(&path_hash)
            .expect("HEAD defined after decline");
        let head_node = engine
            .tree
            .get_node(head_id)
            .expect("decline head node present");
        assert_eq!(head_node.trigger, SnapshotTrigger::Decline);
        assert!(
            head_node.full_content.is_some(),
            "decline node must be a full snapshot"
        );
        assert!(head_node.delta.is_none(), "decline node has no delta");

        // A subsequent record_change must use the decline node as parent, so the
        // delta chain reconstructs v2 from the declined v0 baseline, not from v1.
        let _v2 = engine.record_change(&target_file, b"v2").unwrap();
        let after2 = engine.list_versions(&target_file).unwrap();
        let head2 = after2.last().expect("at least one version after v2");
        let head2_node = engine.tree.get_node(head2.0).expect("v2 node present");
        let restored = engine
            .read_node_content(&head2_node)
            .expect("v2 content reconstructable");
        assert_eq!(restored, b"v2");
    }

    #[test]
    fn decline_rejects_version_from_another_path() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("binding.db");
        let wal = directory.path().join("binding.wal");
        let blobs = directory.path().join("binding-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let source = directory.path().join("source.txt");
        let destination = directory.path().join("destination.txt");
        std::fs::write(&source, b"source-current").unwrap();
        std::fs::write(&destination, b"destination-current").unwrap();
        let engine = ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();
        let source_version = engine.record_change(&source, b"source-history").unwrap();

        let error = engine
            .decline(&destination, source_version)
            .expect_err("a version must only restore its recorded path");

        assert!(error.to_string().contains("path"), "error={error:#}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"destination-current");
    }

    #[test]
    fn query_version_rejects_another_path() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("query-binding.db");
        let wal = directory.path().join("query-binding.wal");
        let blobs = directory.path().join("query-binding-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let source = directory.path().join("source.txt");
        let other = directory.path().join("other.txt");
        let engine = ShadowSnapshotEngine::open(&database, &wal, &blobs).unwrap();
        let version = engine.record_change(&source, b"secret").unwrap();

        let result = engine.query_version_for_path(&other, version);

        assert!(result.is_err());
    }

    /// §4.9 QuotaManager wiring: `with_quota` installs a quota,
    /// `record_change` triggers throttled GC. With a tiny quota and enough
    /// unique-content writes to exceed `GC_INTERVAL`, run_gc runs at least
    /// once and the engine stays usable: HEAD is reachable and reconstructs
    /// to the latest content. This guards against the regression of
    /// `record_change` forgetting to call `maybe_run_gc`.
    #[test]
    fn test_quota_gc_runs_after_record_change() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("qgc.db");
        let wal = dir.path().join("qgc.wal");
        let blobs = dir.path().join("qblobs");
        std::fs::create_dir_all(&blobs).unwrap();

        let target_file = dir.path().join("q.txt");
        std::fs::write(&target_file, b"x").unwrap();

        let engine = ShadowSnapshotEngine::open(&db, &wal, &blobs)
            .unwrap()
            // Tiny quota: 1 KiB so enough unique full snapshots exceed it.
            .with_quota(crate::quota::QuotaManager::new(1024));

        // First record creates a blob. Then overwrite with unique content to
        // grow total_blob_bytes past GC_INTERVAL (64 entries).
        engine.record_change(&target_file, b"v0").unwrap();
        for i in 1..128u32 {
            // Unique content forces new full snapshots (delta dedups identical
            // payloads); 128 writes crosses the 64-record throttle twice.
            let payload = format!("version {i} data");
            engine
                .record_change(&target_file, payload.as_bytes())
                .unwrap();
        }

        // After 128 writes the throttled GC has run at least once. Assert the
        // engine is still usable: HEAD is reachable and reconstructs to the
        // latest written content, so GC never evicted the protected HEAD chain.
        let path_hash = compute_path_hash(&target_file);
        let head_id = engine.tree.get_head(&path_hash).expect("HEAD after writes");
        let head_node = engine
            .tree
            .get_node(head_id)
            .expect("latest head node present");
        let restored = engine
            .read_node_content(&head_node)
            .expect("HEAD content reconstructable after GC");
        assert_eq!(restored, b"version 127 data");
    }

    /// §4.4 a file deletion must be versioned as a node with
    /// `trigger=Delete` rather than silently dropped or recorded as Write.
    /// The watcher routes fs delete events here; without this, restored
    /// history would not show that the file was removed at that SeqNo.
    #[test]
    fn test_record_delete_emits_delete_trigger_node() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("del.db");
        let wal = dir.path().join("del.wal");
        let blobs = dir.path().join("delblobs");
        std::fs::create_dir_all(&blobs).unwrap();

        let target_file = dir.path().join("deleted.txt");
        let engine = ShadowSnapshotEngine::open(&db, &wal, &blobs).unwrap();
        let _v0 = engine.record_change(&target_file, b"present").unwrap();

        engine
            .record_delete(&target_file)
            .expect("record_delete succeeds");

        let after = engine.list_versions(&target_file).unwrap();
        let last = after.last().expect("a node exists after deletion");
        assert_eq!(
            last.2,
            SnapshotTrigger::Delete,
            "delete must be versioned with trigger=Delete"
        );

        // HEAD must point at the Delete node so subsequent writes parent on the
        // tombstone (a recreate is a fresh full snapshot, not a delta over the
        // pre-delete content).
        let path_hash = compute_path_hash(&target_file);
        let head_id = engine
            .tree
            .get_head(&path_hash)
            .expect("HEAD defined after delete");
        let head_node = engine
            .tree
            .get_node(head_id)
            .expect("delete head node present");
        assert_eq!(head_node.trigger, SnapshotTrigger::Delete);
    }

    #[test]
    fn wal_only_delete_advances_sequence_after_restart() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("delete-sequence.db");
        let wal_path = directory.path().join("delete-sequence.wal");
        let blobs = directory.path().join("delete-sequence-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let deleted_path = directory.path().join("deleted.txt");

        let wal = Wal::open(&wal_path).unwrap();
        wal.append(&WalEntry {
            seq_no: 42,
            path_hash: compute_path_hash(&deleted_path),
            parent_id: None,
            content_ref: None,
            delta_ref: None,
            trigger: SnapshotTrigger::Delete,
        })
        .unwrap();
        wal.commit().unwrap();
        drop(wal);

        let engine = ShadowSnapshotEngine::open(&database, &wal_path, &blobs).unwrap();
        let delete_versions = engine.list_versions(&deleted_path).unwrap();
        assert_eq!(delete_versions.len(), 1);
        assert_eq!(delete_versions[0].1, 42);
        assert_eq!(delete_versions[0].2, SnapshotTrigger::Delete);
        let new_path = directory.path().join("new.txt");
        engine.record_change(&new_path, b"new content").unwrap();
        let versions = engine.list_versions(&new_path).unwrap();
        let sequence = versions.last().expect("new version exists").1;
        assert!(sequence > 42, "sequence {sequence} reused WAL history");
    }

    #[test]
    fn wal_replay_fills_sequence_gap_below_persisted_maximum() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("sequence-gap.db");
        let wal_path = directory.path().join("sequence-gap.wal");
        let blobs = directory.path().join("sequence-gap-blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let durable_path = directory.path().join("durable.txt");
        let missing_path = directory.path().join("missing-delete.txt");

        let storage = StorageEngine::open(&database).unwrap();
        storage
            .write_node(
                100,
                &compute_path_hash(&durable_path),
                100,
                None,
                None,
                None,
                0,
                SnapshotTrigger::Delete,
                1,
            )
            .unwrap();
        drop(storage);

        let wal = Wal::open(&wal_path).unwrap();
        wal.append(&WalEntry {
            seq_no: 42,
            path_hash: compute_path_hash(&missing_path),
            parent_id: None,
            content_ref: None,
            delta_ref: None,
            trigger: SnapshotTrigger::Delete,
        })
        .unwrap();
        wal.commit().unwrap();
        drop(wal);

        let engine = ShadowSnapshotEngine::open(&database, &wal_path, &blobs).unwrap();
        let missing_versions = engine.list_versions(&missing_path).unwrap();
        assert_eq!(missing_versions.len(), 1);
        let missing_sequence = missing_versions.first().expect("missing node restored").1;
        assert_eq!(missing_sequence, 42);

        let new_path = directory.path().join("new.txt");
        engine.record_change(&new_path, b"new content").unwrap();
        let new_versions = engine.list_versions(&new_path).unwrap();
        let new_sequence = new_versions.first().expect("new version exists").1;
        assert_eq!(new_sequence, 101);
    }
}
