# Shadow Snapshot Engine (§4)

Versioned file snapshot engine with **WAL-first write path**, **content-addressed blob store**, **bounded delta chains**, and **crash-safe decline protocol**.

## Architecture

```
               ShadowSnapshotEngine
┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐
│   WAL    │→ │VersionTr.│→ │ Storage  │→ │BlobStore │
│ (Layer 0)│  │ (Layer 1)│  │ (SQLite) │  │(Layer 2) │
└────┬─────┘  └──────────┘  └──────────┘  └────┬─────┘
     └── DeltaChain (Rope-level, D_MAX=16) ────┘
```

## Version Tree (§4.1-4.2)
- **Not a DAG** — single-parent chain per file path.
- Keyed by **monotonic `SeqNo`** (global `AtomicU64`).
- `PathHash` = Blake3(path); `ContentHash` = SHA-256(content).
- `VersionNode`: `{ id, path_hash, parent_id, seq_no, content_ref?, delta_ref?, trigger, ancestors[16] }`.
- **Binary lifting LCA**: precomputed `ancestors[k] = 2^k` ancestor → `O(log D)` LCA queries.
- HEAD per path: `HashMap<PathHash, VersionId>`.

## WAL — Layer 0 (§4.3)
- Append-only binary records: `[magic][seq_no][path_hash][parent_id?][content_hash?][delta_hash?][compressed_size?][trigger][checksum]`.
- Group commit (debounce 50ms) + `fsync`. Replay on startup reconstructs version tree.

## Storage Engine — Layer 1 (`storage.rs`)
- SQLite `version_nodes` table: indexed on `(path_hash, seq_no)`.
- `write_node()`, `read_node()`, `list_versions(path_hash)`.

## Blob Store — Layer 2 (`storage.rs:BlobStore`)
- Content-addressed by SHA-256; **Zstd level-1** compressed.
- Tiered: `< 4 KB` inline SQLite; `≥ 4 KB` sharded `hash[0..2]` on filesystem.
- Refcounted GC: `gc()` deletes unreferenced blobs.

## Delta Chain (§4.6, `delta_chain.rs`)
- **Bounded depth**: `D_MAX = 16`. Rope-level ops: `Delete`, `Insert`, `Replace`.
- At `depth == D_MAX` → force full snapshot materialization.
- Reconstruction: walk back ≤ 16 steps to latest full snapshot, replay deltas forward.
- Serialization: custom binary format (no serde dependency).

## Write Path (record_change)

1. SHA-256 content hash; `blob_store.put(hash, content)`.
2. Fetch `seq_no.fetch_add(1)`; get parent HEAD.
3. Compute rope delta; if `delta_depth ≥ D_MAX`, force full snapshot.
4. `WAL.append(entry).fsync()` (durability point).
5. `VersionTree.advance_head()`; `MemTable.record()`.

**Single-writer**: all mutations through `&self` with interior mutability.

## Read Path (query_version)

1. Read `VersionNode` from SQLite.
2. If `content_ref` exists → return blob directly.
3. Otherwise: walk back delta chain collecting `DeltaOp`s until a full snapshot is found; replay in reverse.

## Decline Protocol (§4.8, `decline.rs`)

1. WAL entry (`trigger=Decline`, `content_ref=hash(target)`) + `fsync`.
2. Write target content to actual file path.
3. Watcher calls `check_pending()`: matches pending Decline → **skips** normal record_change.
4. MemTable advances HEAD.

**Crash recovery**: WAL scan finds incomplete Decline entries → re-execute write + memtable step.

## Eviction & Quota (`quota.rs`)
- **Age-based FIFO**: oldest versions evicted until `total_size < max_bytes`.
- Preserves: HEAD per path, full snapshots (deltas evicted first).

## Monitor (`monitor.rs`)
- `notify` crate watches workspace root. Debounce 100ms. Binary-file detection skips non-text.

## Integration

| Consumer | Usage |
|----------|-------|
| `mux_server::Session` | `snapshot_watch` — cwd file changes → `record_change()` |
| `connection::handle_read_file` | `query_version()` for historical file view |

## Constants

| Symbol | Value | File |
|--------|-------|------|
| `D_MAX` | 16 | `delta_chain.rs` |
| `INLINE_THRESHOLD` | 4096 B | `storage.rs` |
| WAL commit window | 50 ms | `wal.rs` |
| Watch debounce | 100 ms | `monitor.rs` |
| Zstd level | 1 | `storage.rs` |