//! shadow_snapshot — Version tree engine per spec §4.
//!
//! 单写线程 (watcher)，WAL-first，content-addressed blob store，
//! age-based FIFO eviction。所有操作以单调 SeqNo 为键。

mod decline;
mod delta_chain;
mod engine;
mod git_hook;
mod lca;
mod memtable;
mod monitor;
mod quota;
mod storage;
mod version_tree;
mod wal;

pub use decline::DeclineProtocol;
pub use delta_chain::{D_MAX, DeltaOp, DeltaReplay, deserialize_delta_ops, serialize_delta_ops};
pub use engine::ShadowSnapshotEngine;
pub use engine::compute_path_hash;
pub use lca::{build_ancestor_table, compute_lca};
pub use memtable::{MemTable, PathChange};
pub use monitor::{EventKind, FileEvent, Monitor, WatchHandle};
pub use quota::QuotaManager;
pub use storage::{BlobStore, StorageEngine};
pub use version_tree::SnapshotTrigger;
pub use version_tree::{
    ContentHash, DeltaRef, PathHash, SeqNo, VersionId, VersionNode, VersionTree,
};
pub use wal::{Wal, WalEntry};
