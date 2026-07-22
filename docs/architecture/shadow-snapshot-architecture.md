# Shadow Snapshot Architecture

z3rm_s.s.: persistent, crash-safe, git-independent undo/redo for terminal + workspace state. See ADR-0006 for decision rationale.

## Overview

```
┌──────────────────────────────────────────────────────────┐
│                     Client (GPUI)                        │
│   SnapshotView (branch selector, timeline, diff)         │
└────────────────────────┬─────────────────────────────────┘
                         │ gRPC
                         ▼
┌──────────────────────────────────────────────────────────┐
│                    z3rm_mux Server                       │
│  ┌────────────────────────────────────────────────────┐  │
│  │              ShadowManager (actor)                  │  │
│  │  Single-writer thread, mpsc channel inbox         │  │
│  │                                                    │  │
│  │  ┌─────────────┐  ┌─────────────┐  ┌───────────┐  │  │
│  │  │ WalWriter   │  │ SnapshotTree│  │ GarbageCol│  │  │
│  │  │ (append-only)│  │ (Arc, RO)  │  │ (periodic)│  │  │
│  │  └──────┬──────┘  └──────┬──────┘  └─────┬─────┘  │  │
│  │         │                │                │        │  │
│  │         ▼                ▼                ▼        │  │
│  │  ┌──────────────────────────────────────────────┐ │  │
│  │  │           ShadowStorage (sled)               │ │  │
│  │  │  tree: snapshots table, wal: WAL segments    │  │  │
│  │  └──────────────────────────────────────────────┘ │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

## Data Model

### SnapshotTree
```rust
pub struct SnapshotTree {
    nodes: HashMap<SeqNo, SnapshotNode>,  // immutable after insert
    branches: HashMap<BranchId, BranchInfo>,
    root: SeqNo,
    tips: HashMap<BranchId, SeqNo>,       // current tip per branch
}

pub struct SnapshotNode {
    pub seq_no: SeqNo,            // u64, globally monotonic
    pub branch_id: BranchId,
    pub parent_seq_no: Option<SeqNo>,  // None = root
    pub created_at: Instant,
    pub payload_hash: [u8; 32],   // SHA-256
    pub payload_ref: PayloadRef,   // pointer to sled blob
    pub children: Vec<SeqNo>,     // for navigation
    pub is_tip: bool,
}

pub struct BranchInfo {
    pub id: BranchId,
    pub name: String,
    pub root_seq_no: SeqNo,
    pub tip_seq_no: SeqNo,
    pub depth: u32,               // node count from root
}
```

### WAL_Entry
```rust
pub struct WalEntry {
    pub seq_no: SeqNo,
    pub branch_id: BranchId,
    pub parent_seq_no: Option<SeqNo>,
    pub op: WalOp,
    pub payload_hash: [u8; 32],
    pub timestamp_ns: u64,
}

pub enum WalOp {
    SnapshotCreated { payload_ref: PayloadRef, description: String },
    SnapshotRestored { target_seq_no: SeqNo, new_branch_id: Option<BranchId> },
    SnapshotBranched { new_branch_id: BranchId },
    SnapshotDeleted { seq_no: SeqNo },
    Compaction { flattened: Vec<SeqNo>, retained_tip: SeqNo },
}
```

## Sequencing

`SeqNo` is a `u64` assigned by `ShadowManager` actor (single-writer). Monotonically increasing, gap-free. On restart, WAL replay reconstructs `SeqNo` counter from last entry.

```rust
impl ShadowManager {
    fn next_seq_no(&mut self) -> SeqNo {
        self.counter += 1;
        self.counter
    }
}
```

## WAL Protocol

1. **Append:** Client calls `SnapshotManager::create_snapshot()`. Manager serializes payload, writes to storage blob, computes SHA-256, appends `WalEntry` to WAL segment, `fsync`s WAL. Only then updates in-memory tree.
2. **Replay:** On startup, `ShadowManager` reads WAL segments forward, reconstructs `SnapshotTree`, verifies `payload_hash` against stored blobs, discards entries with missing/corrupt payloads (logs warning).
3. **Compaction:** When WAL segment exceeds `max_wal_segment_size` (default 64 MiB), background compaction: read tree from WAL, serialize compacted tree (branches + tips + root), write new WAL segment, atomically swap, delete old segments.

## Branching

```
root (1) ─── (2) ─── (3) ─── (4) [tip: main]
                 │
                 └── (5) ─── (6) [tip: experiment]
```

- `create_snapshot` on `main` → new node with `parent = 4`
- `branch_snapshot(seq_no=3, branch_id="experiment")` → new branch rooted at node 3
- `restore_snapshot(seq_no=3, create_branch=false)` → resets `main` tip to node 3, children 4+ orphaned (eligible for GC)
- `restore_snapshot(seq_no=3, create_branch=true)` → new branch from node 3, `main` unchanged

## Garbage Collection

- **Age-based:** Nodes older than `retention_days` (default 30) and not a tip or branch root are eligible.
- **Depth-based:** Branches exceeding `D_MAX` (16) trigger compaction: flatten path from root to tip, retain branch points + tip, delete intermediates.
- **Orphaned:** Children of a restored-to node (when `create_branch=false`) become eligible.
- **Frequency:** Background task runs every `gc_interval` (default 1 hour).

GC writes `WalEntry::Compaction` before deleting payloads, so crash during GC is recoverable.

## Storage Backend

`sled` embedded database (`z3rm_shadow_storage`):
- `snapshots` tree: `SeqNo -> SnapshotNode` (serialized with `postcard`)
- `payloads` tree: `PayloadRef -> blob` (raw bytes, content-addressed)
- `wal` tree: `segment_id -> WalEntry` (append-only, log-structured)

Alternatives considered: `redb` (pure Rust, no unsafe), LMDB (C, battle-tested). sled chosen for active maintenance, embedded simplicity, log-structured design (fits WAL pattern).

## Concurrent Access

- **Writers:** Single `ShadowManager` actor. All mutations via mpsc channel. No write locks.
- **Readers:** `Arc<SnapshotTree>` clone is cheap (immutable). Readers traverse tree lock-free. Payload reads via storage backend's internal concurrency.

## Integration with Mux

- `GridManager` calls `ShadowManager::create_snapshot()` on:
  - Pane split (state checkpoint)
  - Pane close (save before destroy)
  - Manual trigger (`Ctrl+Shift+S` in chrome)
  - Timer (default every 60s if dirty)
- `TerminalView` subscribes to `SubscribeSnapshots` stream for UI timeline.
- `z3rm_chrome` exposes snapshot timeline / branch selector as command palette entries.