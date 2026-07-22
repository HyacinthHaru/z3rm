# 0006 - Shadow Snapshot Version Tree

**Status:** Accepted

## Context

z3rm needs crash-safe, git-independent undo/redo for terminal buffer state, pane layouts, and workspace state. Git is too coarse (commits), too slow (per-keystroke), and requires repo. Traditional undo stacks are linear and lost on crash. We need persistent, queryable history with branching (try alternative, return to branch point) and crash recovery (WAL replay).

## Decision

Persistent version tree (not DAG) with these invariants:
- **Tree structure:** Each snapshot has exactly one parent (except root). Branching creates new child from any existing node. No merges, no DAG — linear history per branch, explicit branch points.
- **WAL-first:** Every mutation appends to write-ahead log (WAL) before applying to in-memory state. WAL entries: `SeqNo`, `BranchId`, `ParentSeqNo`, `Operation`, `PayloadHash`. `SeqNo` is monotonically increasing `u64` (single-writer thread).
- **D_MAX = 16:** Maximum tree depth from root to leaf. Exceeding D_MAX triggers automatic compaction (flatten branch, retain tip + branch points).
- **Age-based eviction:** Snapshots older than `retention_days` (default 30) and not a branch point or tip are eligible for GC. GC compacts WAL, rewrites `SeqNo` monotonically.
- **Single-writer thread:** All mutations go through `SnapshotWriter` actor (single-threaded). Readers access immutable `SnapshotTree` via `Arc<SnapshotTree>` (lock-free reads).

Snapshot payloads are `z3rm_shadow::Snapshot` (serialized workspace + terminal grid state). Storage: `z3rm_shadow_storage` crate with `sled` or `redb` backend.

## Consequences

- **Positive:** Crash-safe by construction (WAL replay on startup). Branching enables "what-if" exploration without git. Single-writer eliminates lock contention. D_MAX bounds memory. Age-based eviction bounds disk.
- **Negative:** Single-writer thread is throughput bottleneck for high-frequency mutations (keystrokes). D_MAX compaction loses intermediate history. No merge semantics (intentional). WAL replay on cold start adds latency.
- **Mitigation:** Batch keystrokes into single WAL entry per frame (16ms). Compaction preserves branch points and tips. Startup replay is incremental (background, UI shows last known state immediately). `sled`/`redb` provide fast sequential WAL writes.