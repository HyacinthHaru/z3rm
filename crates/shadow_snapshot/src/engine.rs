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

use crate::storage::{BlobStore, StorageEngine};
use crate::version_tree::{
    ContentHash, PathHash, SeqNo, SnapshotTrigger, VersionId, VersionTree,
};
use crate::wal::{Wal, WalEntry};

/// Compute a Blake3 hash of a file path for use as `PathHash`.
fn compute_path_hash(path: &Path) -> PathHash {
    let mut hasher = Blake3Hasher::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.finalize().into()
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
    pub fn open(db_path: &Path, wal_path: &Path, blob_dir: &Path) -> Result<Self> {
        let storage = Arc::new(StorageEngine::open(db_path)?);
        let wal = Wal::open(wal_path).map_err(|e| anyhow::anyhow!(e))?;
        let blob_store = BlobStore::new(Arc::clone(&storage), blob_dir.to_path_buf());

        Ok(Self {
            wal,
            storage,
            tree: VersionTree::new(),
            blob_store,
            seq_no: AtomicU64::new(1),
        })
    }

    /// Record a file change. Called by the file watcher.
    ///
    /// Stores content in the blob store, writes a WAL entry,
    /// and advances the version tree HEAD for the given path.
    pub fn record_change(&self, path: &Path, new_content: &[u8]) -> Result<VersionId> {
        let path_hash = compute_path_hash(path);
        let seq_no = self.seq_no.fetch_add(1, Ordering::AcqRel) as SeqNo;
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        // Store content in blob store (content-addressed, deduplicates automatically).
        let content_hash = self.blob_store.put(new_content)?;

        // Get current HEAD for this path (used as parent for chain).
        let parent_id = self.tree.get_head(&path_hash);

        // Advance version tree HEAD with a full snapshot node.
        let version_id = self.tree.advance_head(
            path_hash,
            seq_no,
            timestamp_ns,
            parent_id,
            Some(content_hash),
            None, // Full snapshot, no delta
            0,
            SnapshotTrigger::Write,
        );

        // Persist the node to storage.
        self.storage.write_node(
            version_id,
            &path_hash,
            seq_no,
            parent_id,
            Some(&content_hash),
            None,
            0,
            SnapshotTrigger::Write,
            timestamp_ns,
        )?;

        // Append WAL entry (group commit happens on checkpoint).
        let entry = WalEntry {
            seq_no,
            path_hash,
            parent_id,
            content_ref: Some(content_hash),
            delta_ref: None,
            trigger: SnapshotTrigger::Write,
        };
        self.wal.append(&entry)?;

        Ok(version_id)
    }

    /// Query content at a specific version.
    pub fn query_version(&self, version_id: VersionId) -> Result<Option<Vec<u8>>> {
        let node = match self.tree.get_node(version_id) {
            Some(node) => node,
            None => return Ok(None),
        };

        // Full snapshots store content_hash directly.
        let content_hash = if let Some(hash) = &node.full_content {
            hash.clone()
        } else {
            // Delta-only node — reconstruct from chain.
            // For now, return None (full reconstruction is §4.6 scope).
            return Ok(None);
        };

        self.blob_store.get(&content_hash).map(Some)
    }

    /// Decline (undo) to a specific version. Crash-safe per §4.8.
    ///
    /// Stub: logs the intention and records a WAL entry.
    /// Full crash-safe protocol (WAL-first, write file, watcher match)
    /// is deferred until worktree integration.
    pub fn decline(&self, path: &Path, target_version: VersionId) -> Result<()> {
        let node = self.tree.get_node(target_version).ok_or_else(|| {
            anyhow::anyhow!("version {} not found", target_version)
        })?;

        tracing::info!(
            version_id = target_version,
            path = ?path,
            seq_no = node.seq_no,
            "decline stub: would restore version to target"
        );

        // Record a WAL entry so crash recovery knows decline was intended.
        let seq_no = self.seq_no.fetch_add(1, Ordering::AcqRel) as SeqNo;
        let entry = WalEntry {
            seq_no,
            path_hash: node.path_hash,
            parent_id: Some(target_version),
            content_ref: node.full_content,
            delta_ref: None,
            trigger: SnapshotTrigger::Decline,
        };
        self.wal.append(&entry)?;
        self.wal.commit()?;

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
}
