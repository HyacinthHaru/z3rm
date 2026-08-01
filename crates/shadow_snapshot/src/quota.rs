//! Quota GC：age-based FIFO eviction + promote-to-full（§4.9）
//!
//! - 删除时按 seq_no 升序（FIFO）evict 最老的节点
//! - 用 blobstore 的真实保留字节数约束，不靠外部 `used_bytes` 喂入
//! - 保留所有 HEAD 的 reconstructability：HEAD 的整条 delta 链及其 full base
//!   一律不可删；删除 full base 前必须先把仍可达的 delta child materialize 成
//!   full snapshot（promote-to-full），避免悬空 parent/delta 引用
//! - Orphan 分支 grace period（默认 24h）后变为 GC 候选
//! - Git commit hook：commit 后标记 pre-commit deltas 为 gc-eligible

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::delta_chain::DeltaReplay;
use crate::storage::{BlobStore, StorageEngine};
use crate::version_tree::{ContentHash, SeqNo, VersionId, VersionTree};

/// 孤儿分支 grace period（默认 24h）
const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(24 * 3600);

/// 配额管理器
pub struct QuotaManager {
    /// 最大存储空间（字节）
    max_bytes: u64,
    /// 当前已用空间（由 `run_gc` 根据 blobstore 实际保留字节重算）
    used_bytes: parking_lot::Mutex<u64>,
    /// Grace period（interior mutability）
    grace_period: parking_lot::Mutex<Duration>,
    /// 孤儿节点标记时间
    orphan_since: parking_lot::Mutex<HashMap<VersionId, Instant>>,
    /// GC 候选集合（planning 用，记录已标记/计划删除的节点）
    gc_eligible: parking_lot::Mutex<HashSet<VersionId>>,
}

impl QuotaManager {
    /// 创建配额管理器
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            used_bytes: parking_lot::Mutex::new(0),
            grace_period: parking_lot::Mutex::new(DEFAULT_GRACE_PERIOD),
            orphan_since: parking_lot::Mutex::new(HashMap::new()),
            gc_eligible: parking_lot::Mutex::new(HashSet::new()),
        }
    }

    /// 设置 grace period
    pub fn set_grace_period(&self, period: Duration) {
        *self.grace_period.lock() = period;
    }

    /// 检查是否超过配额。`used_bytes` 由上次 `run_gc` 根据 blobstore 实际保留
    /// 字节刷新；若从未运行过 GC 则始终返回 false（未超配额是诚实的）。
    pub fn is_over_quota(&self) -> bool {
        *self.used_bytes.lock() > self.max_bytes
    }

    /// 执行 GC：按真实保留字节 FIFO 删除最老的、非保护的节点，直到回到配额内。
    ///
    /// 保护集 = 每个 HEAD 的重建链（HEAD → parent → … 直到 full base 含），
    /// 保证回收后所有 HEAD 都仍可重建。删除一个 full base 前，先把其仍可达的
    /// delta child 在位提升为 full（materialize → 写新 full blob → 改写节点），
    /// 释放对旧 base 的引用，从而不留下悬空 parent / delta 引用。
    ///
    /// 返回本次回收的字节数（基于 blobstore 的实际大小）。
    pub fn run_gc(
        &self,
        tree: &VersionTree,
        blob_store: &BlobStore,
        storage: &StorageEngine,
    ) -> Result<u64> {
        let used = storage.total_blob_bytes().context("gc: total blob bytes")?;
        *self.used_bytes.lock() = used;

        let to_free = used.saturating_sub(self.max_bytes);
        if to_free == 0 {
            return Ok(0);
        }

        let protected = self.protected_set(tree);
        // FIFO：按 seq_no 升序选可删候选，跳过保护集。
        let mut candidates: Vec<(SeqNo, VersionId)> = tree
            .iter_nodes()
            .into_iter()
            .filter(|(id, _)| !protected.contains(id))
            .map(|(id, n)| (n.seq_no, id))
            .collect();
        candidates.sort_by_key(|(seq, _)| *seq);

        let mut freed: u64 = 0;
        for (seq_no, id) in candidates {
            if freed >= to_free {
                break;
            }
            let Some(node) = tree.get_node(id) else {
                continue;
            };

            // 删除其 full base 之前，先把仍可达（在保护集中）的 delta child
            // 提升为 full snapshot，避免删 base 后 child 重建链断裂。
            if node.full_content.is_some() {
                self.promote_protected_delta_children(tree, blob_store, storage, id)?;
            }

            let (content_size, delta_size) = storage
                .node_blob_sizes(
                    node.full_content.as_ref().unwrap_or(&[0u8; 32]),
                    node.delta.as_ref().map(|d| &d.hash),
                )
                .context("gc: node blob sizes")?;
            let node_bytes = content_size.max(delta_size);

            // 持久层与内存层一致地删除节点。
            storage
                .delete_node(id)
                .context("gc: delete node from storage")?;
            tree.remove_node(id);

            // unref 该节点引用的 blob（full 与/或 delta）。
            if let Some(full_hash) = &node.full_content {
                blob_store.unref(full_hash).context("gc: unref full blob")?;
            }
            if let Some(delta) = &node.delta {
                blob_store
                    .unref(&delta.hash)
                    .context("gc: unref delta blob")?;
            }

            {
                let mut eligible = self.gc_eligible.lock();
                eligible.insert(id);
            }
            freed += node_bytes;
            info!(
                version_id = id,
                seq_no,
                freed_bytes = node_bytes,
                "gc: evicted node"
            );
        }

        // 用实际的回收后大小刷新 used_bytes（保守按 freed 扣减）。
        let new_used = used.saturating_sub(freed);
        *self.used_bytes.lock() = new_used;
        Ok(freed)
    }

    /// 把指定 full base 节点上仍 *受保护* 的直接 delta child 在位提升为 full。
    ///
    /// “受保护”指 child 在某个 HEAD 的重建链上——删除 base 会破坏其重建路径，
    /// 因此必须先 materialize：reconstruct child 内容 → 存为新 full blob →
    /// 在 version tree / storage 中改写该 child 为 full → unref 旧 delta blob。
    fn promote_protected_delta_children(
        &self,
        tree: &VersionTree,
        blob_store: &BlobStore,
        storage: &StorageEngine,
        base_id: VersionId,
    ) -> Result<()> {
        let protected = self.protected_set(tree);
        let children: Vec<VersionId> = tree
            .iter_nodes()
            .into_iter()
            .filter(|(_, n)| n.parent_id == Some(base_id) && n.delta.is_some())
            .map(|(id, _)| id)
            .collect();

        for child_id in children {
            if !protected.contains(&child_id) {
                continue;
            }
            let Some(child) = tree.get_node(child_id) else {
                continue;
            };
            // reconstruct child 的当前内容（依赖 base + delta 链）。
            let rope = DeltaReplay::reconstruct(
                &child,
                |id| tree.get_node(id),
                |hash: &[u8; 32]| blob_store.get(hash).ok(),
            )
            .context("gc: promote: reconstruct delta child")?;
            let content = rope_to_bytes(&rope);
            let new_full_hash = blob_store
                .put(&content)
                .context("gc: promote: store full")?;
            // 在树中改写 child：full_content=新 hash, delta=None, depth=0；拿回旧 delta。
            let old_delta = tree.promote_to_full(child_id, new_full_hash);
            // 持久化改写后的 child 节点。
            storage
                .write_node(
                    child_id,
                    &child.path_hash,
                    child.seq_no,
                    child.parent_id,
                    Some(&new_full_hash),
                    None,
                    0,
                    child.trigger,
                    child.timestamp_ns,
                )
                .context("gc: promote: persist rewritten child")?;
            // 旧 delta blob 不再被该 child 引用 → unref（refcount--，归零则删）。
            if let Some(delta) = old_delta {
                blob_store
                    .unref(&delta.hash)
                    .context("gc: promote: unref old delta blob")?;
            }
            info!(
                version_id = child_id,
                parent = base_id,
                "gc: promoted delta child to full"
            );
        }
        Ok(())
    }

    /// 计算保护集：所有 HEAD 的可重建链（含 full base）。删除该集合中任何节点
    /// 都会破坏某个 HEAD 的 reconstructability，故 GC 一律跳过。
    fn protected_set(&self, tree: &VersionTree) -> HashSet<VersionId> {
        let mut protected = HashSet::new();
        for &head_id in tree.iter_heads().values() {
            let mut current = Some(head_id);
            // 沿 parent 链回溯，直到遇到 full snapshot 为止（含）。
            let mut steps = 0u32;
            while let Some(id) = current {
                if !protected.insert(id) {
                    break; // 已访问，避免环/重复
                }
                let Some(node) = tree.get_node(id) else { break };
                if node.full_content.is_some() {
                    break; // full base：保存就停止回溯
                }
                current = node.parent_id;
                steps += 1;
                // 保险：delta 链理论上限 D_MAX
                if steps > crate::delta_chain::D_MAX as u32 {
                    break;
                }
            }
        }
        protected
    }

    /// 收集所有 HEAD 链上的节点 ID（不可 GC）。旧的计划用 API（planning），
    /// 保留向后兼容；真实保护集语义由 `protected_set` 提供。
    pub fn collect_head_ids(&self, tree: &VersionTree) -> HashSet<VersionId> {
        let mut head_ids = HashSet::new();
        let orphans = tree.get_orphans();

        let heads = tree.iter_heads();
        for &head_id in heads.values() {
            let mut stack = vec![head_id];
            while let Some(id) = stack.pop() {
                if head_ids.insert(id) {
                    if let Some(node) = tree.get_node(id) {
                        if let Some(parent) = node.parent_id {
                            // 不跟随 orphan 节点的祖先链
                            if !orphans.contains(&parent) {
                                stack.push(parent);
                            }
                        }
                    }
                }
            }
        }

        head_ids
    }

    /// Promote-to-full：返回树中已没有任何存活 delta child 的 full snapshot 数。
    ///
    /// 旧的纯计划式 API，仅用于报告；真正的 child promotion 由 `run_gc` 在删除
    /// full base 之前通过 `promote_protected_delta_children` 完成。
    pub fn batch_promote(&self, tree: &VersionTree) -> usize {
        let full_snapshots: Vec<VersionId> = tree
            .iter_nodes()
            .into_iter()
            .filter(|(_, n)| n.full_content.is_some())
            .map(|(id, _)| id)
            .collect();

        let mut promoted = 0;
        for snapshot_id in &full_snapshots {
            let mut has_live_child = false;
            for (_, child) in tree.iter_nodes() {
                if child.parent_id == Some(*snapshot_id) && child.delta.is_some() {
                    has_live_child = true;
                    break;
                }
            }
            if !has_live_child {
                promoted += 1;
            }
        }
        promoted
    }

    /// 标记孤儿分支为 GC 候选
    ///
    /// 孤儿分支在 grace period 后变为 GC 候选。
    pub fn prune_orphan_branches(&self, tree: &VersionTree, now: Instant) {
        let orphans = tree.get_orphans();

        let mut orphan_since = self.orphan_since.lock();
        for id in &orphans {
            orphan_since.entry(*id).or_insert(now);
        }

        // 检查哪些孤儿已超过 grace period
        let grace = *self.grace_period.lock();
        let mut to_gc = Vec::new();
        for (id, since) in orphan_since.iter() {
            if now.duration_since(*since) >= grace {
                to_gc.push(*id);
            }
        }

        // 标记为 GC 候选
        if !to_gc.is_empty() {
            let mut eligible = self.gc_eligible.lock();
            for id in &to_gc {
                eligible.insert(*id);
            }

            // 从 orphan_since 中移除
            for id in &to_gc {
                orphan_since.remove(id);
            }

            tree.mark_gc_eligible(&to_gc);
            warn!(count = to_gc.len(), "gc: orphan branches pruned");
        }
    }

    /// Git commit hook：标记 pre-commit deltas 为 GC 候选
    ///
    /// git commit 后，commit 之前的所有 delta 变为可 GC（
    /// 因为 commit 已持久化到 git history，shadow snapshot 不再需要它们）。
    pub fn on_git_commit(&self, tree: &VersionTree, commit_seq: SeqNo) {
        let mut to_gc = Vec::new();
        for (id, node) in tree.iter_nodes() {
            if node.seq_no < commit_seq && node.delta.is_some() {
                to_gc.push(id);
            }
        }

        if !to_gc.is_empty() {
            let mut eligible = self.gc_eligible.lock();
            for id in &to_gc {
                eligible.insert(*id);
            }

            tree.mark_gc_eligible(&to_gc);
            info!(count = to_gc.len(), seq = commit_seq, "gc: git commit hook");
        }
    }

    /// 获取 GC 候选数量
    pub fn gc_eligible_count(&self) -> usize {
        self.gc_eligible.lock().len()
    }

    /// 清除 GC 候选（实际删除后调用）
    pub fn clear_gc_eligible(&self, ids: &[VersionId]) {
        let mut eligible = self.gc_eligible.lock();
        for id in ids {
            eligible.remove(id);
        }
    }
}

/// 把 Rope 的内容转成字节（保留 UTF-8；非 UTF-8 的内容用 lossy 转）。
fn rope_to_bytes(rope: &rope::Rope) -> Vec<u8> {
    // DeltaReplay 的 apply 依赖 UTF-8 文本语义，故用 to_string 路径。
    rope.to_string().into_bytes()
}
