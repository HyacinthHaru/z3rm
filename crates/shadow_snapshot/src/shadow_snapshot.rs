//! shadow_snapshot — Version tree engine per spec §4.
//!
//! 单写线程 (watcher)，WAL-first，content-addressed blob store，
//! age-based FIFO eviction。所有操作以单调 SeqNo 为键。

mod config;
mod delta_chain;
mod decline;
mod lca;
mod memtable;
mod monitor;
mod git_hook;
mod quota;
mod storage;
mod version_tree;
mod wal;
mod engine;

pub use config::{
    DEFAULT_CIRCUIT_BREAKER_K, DEFAULT_DEBOUNCE, DEFAULT_QUOTA_BYTES, GitCommitHookMode, QuotaMode,
    SnapshotConfig,
};
pub use delta_chain::{serialize_delta_ops, deserialize_delta_ops, DeltaOp, DeltaReplay, D_MAX};
pub use decline::DeclineProtocol;
pub use git_hook::{GitCommitTracker, GitCommitWatcher, resolve_git_dir, watch_git_commits};
pub use lca::{compute_lca, build_ancestor_table};
pub use memtable::{MemTable, PathChange};
pub use version_tree::SnapshotTrigger;
pub use quota::{GlobalQuotaLedger, QuotaManager};
pub use storage::{BlobStore, StorageEngine};
pub use version_tree::{
    ContentHash, DeltaRef, PathHash, SeqNo, VersionId, VersionNode, VersionTree,
};
pub use wal::{Wal, WalEntry};
pub use engine::ShadowSnapshotEngine;
pub use engine::compute_path_hash;
pub use monitor::{DebounceQueue, EventKind, FileEvent, Monitor, WatchHandle};
