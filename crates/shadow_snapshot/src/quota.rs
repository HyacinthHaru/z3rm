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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::delta_chain::DeltaReplay;
use crate::storage::{BlobStore, StorageEngine};
use crate::version_tree::{ContentHash, SeqNo, VersionId, VersionTree};

/// 孤儿分支 grace period（默认 24h）
const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(24 * 3600);

/// `node_blob_sizes` 需要一个 full-content hash；tombstone / delta-only 节点没有
/// full blob，用全零哈希查询必然命中零行，等价于"没有 full blob 占用"。
const ABSENT_CONTENT_HASH: ContentHash = [0u8; 32];

/// §4.9 `QuotaMode::Global` 用的跨引擎用量账本。
///
/// 每个 session 一个引擎、一个 SQLite 库，因此没有任何单个引擎知道全局占用。
/// 账本让每个 `QuotaManager` 报告自己的实际占用并读回全局总量，GC 据此判断
/// 是否超出共享配额；删除仍然只发生在各自的树里（FIFO by seq_no 不变）。
pub struct GlobalQuotaLedger {
    usage: parking_lot::Mutex<HashMap<u64, u64>>,
    next_id: AtomicU64,
}

impl GlobalQuotaLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            usage: parking_lot::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    fn register(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::AcqRel)
    }

    /// 记录某个引擎的占用并返回全局总量。
    fn report(&self, id: u64, used: u64) -> u64 {
        let mut usage = self.usage.lock();
        usage.insert(id, used);
        usage.values().sum()
    }

    fn release(&self, id: u64) {
        self.usage.lock().remove(&id);
    }

    /// 当前全局总占用（诊断用）。
    pub fn total_bytes(&self) -> u64 {
        self.usage.lock().values().sum()
    }
}

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
    /// GC 候选集合：孤儿分支过 grace period、或 git commit 后的 pre-commit
    /// delta。`run_gc` 优先回收这些节点，真正删除后再移出集合。
    gc_eligible: parking_lot::Mutex<HashSet<VersionId>>,
    /// `QuotaMode::Global` 时的共享账本 + 本引擎在账本里的 id。
    shared_ledger: Option<(Arc<GlobalQuotaLedger>, u64)>,
}

impl QuotaManager {
    /// 创建配额管理器（per-project 作用域）
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            used_bytes: parking_lot::Mutex::new(0),
            grace_period: parking_lot::Mutex::new(DEFAULT_GRACE_PERIOD),
            orphan_since: parking_lot::Mutex::new(HashMap::new()),
            gc_eligible: parking_lot::Mutex::new(HashSet::new()),
            shared_ledger: None,
        }
    }

    /// §4.9 创建共享配额的管理器（`quota_mode = "global"`）。
    pub fn with_shared_ledger(max_bytes: u64, ledger: Arc<GlobalQuotaLedger>) -> Self {
        let id = ledger.register();
        let mut manager = Self::new(max_bytes);
        manager.shared_ledger = Some((ledger, id));
        manager
    }

    /// 设置孤儿分支 grace period（§4.4 "configurable, default 24h"）。
    pub fn set_grace_period(&self, period: Duration) {
        *self.grace_period.lock() = period;
    }

    /// 本次 GC 用来和 `max_bytes` 比较的占用量。
    ///
    /// per-project：就是本引擎的占用。global：把本引擎占用报进共享账本并取回
    /// 全局总量，于是任一 session 超出共享配额时所有 session 都会开始回收。
    pub fn budget_usage(&self, own_used: u64) -> u64 {
        match &self.shared_ledger {
            Some((ledger, id)) => ledger.report(*id, own_used),
            None => own_used,
        }
    }

    /// 检查是否超过配额。`used_bytes` 由上次 `run_gc` 根据 blobstore 实际保留
    /// 字节刷新；若从未运行过 GC 则始终返回 false（未超配额是诚实的）。
    pub fn is_over_quota(&self) -> bool {
        *self.used_bytes.lock() > self.max_bytes
    }

    /// 执行 GC：按真实保留字节 FIFO 删除最老的、非保护的节点，直到回到配额内。
    ///
    /// 顺序（§4.9）：
    /// 1. `prune_orphan_branches`：过了 grace period 的孤儿分支进入候选集。
    /// 2. 计算预算占用（global 模式下是跨引擎总量）与需释放字节数。
    /// 3. 选受害者：gc-eligible 优先，同类内按 seq_no 升序（FIFO，不是 LRU）。
    /// 4. `batch_promote`：一次性把所有受害 full base 上仍受保护的 delta child
    ///    提升为 full（spec 要求 promotion 在单次 GC pass 内批量摊销 I/O）。
    /// 5. 删除受害者、unref blob、把它们移出候选集。
    ///
    /// 保护集 = 每个 HEAD 的重建链（HEAD → parent → … 直到 full base 含），
    /// 保证回收后所有 HEAD 都仍可重建。
    ///
    /// 返回本次回收的字节数（基于 blobstore 的实际大小）。
    pub fn run_gc(
        &self,
        tree: &VersionTree,
        blob_store: &BlobStore,
        storage: &StorageEngine,
    ) -> Result<u64> {
        // 孤儿分支的 grace period 以单调时钟计量，不受 NTP 回拨影响。
        self.prune_orphan_branches(tree, Instant::now());

        // 每一轮都重新实测占用量。一轮内的规划是按"每个节点各自的 blob 大小"
        // 累加的，而 blob 是内容寻址、可被多个节点共享的，所以规划值会高估实际
        // 能释放的字节数，一轮下来往往还在配额之上。重测后继续，直到真的降到
        // 配额以内或没有可回收的节点为止。
        let mut freed_total: u64 = 0;
        loop {
            let freed = self.evict_pass(tree, blob_store, storage)?;
            freed_total += freed;
            if freed == 0 {
                break;
            }
        }
        Ok(freed_total)
    }

    /// 一轮回收：实测占用 → 规划受害者 → promote → 删除。返回本轮释放的字节数，
    /// 0 表示已在配额内或没有可回收的节点。
    fn evict_pass(
        &self,
        tree: &VersionTree,
        blob_store: &BlobStore,
        storage: &StorageEngine,
    ) -> Result<u64> {
        let used = storage.total_blob_bytes().context("gc: total blob bytes")?;
        *self.used_bytes.lock() = used;

        // 共享配额下按全局总量判断是否超额，但本引擎最多只能释放自己那部分。
        let budget_used = self.budget_usage(used);
        let to_free = budget_used.saturating_sub(self.max_bytes).min(used);
        if to_free == 0 {
            return Ok(0);
        }

        let protected = self.protected_set(tree);
        let eligible = self.gc_eligible.lock().clone();
        // 排序键 (not_eligible, seq_no)：gc-eligible 先被回收（spec §4.9
        // "Next GC cycle prioritizes gc-eligible nodes"），同类内 FIFO by seq_no。
        let mut candidates: Vec<(bool, SeqNo, VersionId)> = tree
            .iter_nodes()
            .into_iter()
            .filter(|(id, _)| !protected.contains(id))
            .map(|(id, node)| (!eligible.contains(&id), node.seq_no, id))
            .collect();
        candidates.sort_unstable();

        let mut victims = Vec::new();
        let mut planned: u64 = 0;
        for (_, seq_no, id) in candidates {
            if planned >= to_free {
                break;
            }
            let Some(node) = tree.get_node(id) else {
                continue;
            };
            let (content_size, delta_size) = storage
                .node_blob_sizes(
                    node.full_content.as_ref().unwrap_or(&ABSENT_CONTENT_HASH),
                    node.delta.as_ref().map(|d| &d.hash),
                )
                .context("gc: node blob sizes")?;
            planned += content_size.max(delta_size);
            victims.push((id, seq_no, node, content_size.max(delta_size)));
        }

        // 删除 full base 之前，先把仍可达（在保护集中）的 delta child 提升为
        // full snapshot，避免删 base 后 child 重建链断裂。整批一次做完。
        let bases: Vec<VersionId> = victims
            .iter()
            .filter(|(_, _, node, _)| node.full_content.is_some())
            .map(|(id, _, _, _)| *id)
            .collect();
        let promoted = self.batch_promote(tree, blob_store, storage, &bases)?;

        let mut freed: u64 = 0;
        let mut evicted = Vec::with_capacity(victims.len());
        for (id, seq_no, node, node_bytes) in victims {
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

            evicted.push(id);
            freed += node_bytes;
            info!(
                version_id = id,
                seq_no,
                freed_bytes = node_bytes,
                "gc: evicted node"
            );
        }
        // 已经真正删掉的节点不再是"候选"，否则候选集会无限增长。
        self.clear_gc_eligible(&evicted);

        // 用实际的回收后大小刷新 used_bytes（保守按 freed 扣减）。
        let new_used = used.saturating_sub(freed);
        *self.used_bytes.lock() = new_used;
        self.budget_usage(new_used);
        info!(
            evicted = evicted.len(),
            promoted,
            freed_bytes = freed,
            remaining_eligible = self.gc_eligible_count(),
            "gc: pass complete"
        );
        // 没有删掉任何节点就说明剩下的全在保护集里，再循环也不会有进展。
        if evicted.is_empty() {
            return Ok(0);
        }
        Ok(freed)
    }

    /// §4.9 批量 promote-to-full：把这批 full base 上仍 *受保护* 的直接 delta
    /// child 在位提升为 full，返回提升的节点数。
    ///
    /// “受保护”指 child 在某个 HEAD 的重建链上——删除 base 会破坏其重建路径，
    /// 因此必须先 materialize：reconstruct child 内容 → 存为新 full blob →
    /// 在 version tree / storage 中改写该 child 为 full → unref 旧 delta blob。
    ///
    /// 一次 GC pass 只算一次保护集、只扫一次节点表，spec 要求 promotion 批量
    /// 进行以摊销 I/O。
    pub fn batch_promote(
        &self,
        tree: &VersionTree,
        blob_store: &BlobStore,
        storage: &StorageEngine,
        base_ids: &[VersionId],
    ) -> Result<usize> {
        if base_ids.is_empty() {
            return Ok(0);
        }
        let bases: HashSet<VersionId> = base_ids.iter().copied().collect();
        let protected = self.protected_set(tree);
        let children: Vec<VersionId> = tree
            .iter_nodes()
            .into_iter()
            .filter(|(id, node)| {
                node.delta.is_some()
                    && node.parent_id.is_some_and(|parent| bases.contains(&parent))
                    && protected.contains(id)
            })
            .map(|(id, _)| id)
            .collect();

        let mut promoted = 0;
        for child_id in children {
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
            promoted += 1;
            info!(
                version_id = child_id,
                parent = ?child.parent_id,
                "gc: promoted delta child to full"
            );
        }
        Ok(promoted)
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

    /// 标记孤儿分支为 GC 候选（§4.4 orphan branch policy）。
    ///
    /// 由 `run_gc` 在每次 pass 开头调用：孤儿被首次看到时记下时间，超过
    /// grace period 后进入候选集，于是下一轮选受害者时排在最前面。
    /// 仍在某个 HEAD 重建链上的节点即使被标记也不会被删（保护集优先）。
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

    /// Git commit hook：标记 pre-commit deltas 为 GC 候选，返回标记数量。
    ///
    /// git commit 后，commit 之前的所有 delta 变为可 GC（
    /// 因为 commit 已持久化到 git history，shadow snapshot 不再需要它们）。
    /// 边界用引擎的单调 SeqNo，不用墙钟时间或 commit 时间戳。
    pub fn on_git_commit(&self, tree: &VersionTree, commit_seq: SeqNo) -> usize {
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
            drop(eligible);

            tree.mark_gc_eligible(&to_gc);
            info!(count = to_gc.len(), seq = commit_seq, "gc: git commit hook");
        }
        to_gc.len()
    }

    /// 获取 GC 候选数量
    pub fn gc_eligible_count(&self) -> usize {
        self.gc_eligible.lock().len()
    }

    /// 某个版本是否已进入 GC 候选集。
    pub fn is_gc_eligible(&self, version_id: VersionId) -> bool {
        self.gc_eligible.lock().contains(&version_id)
    }

    /// 清除 GC 候选（实际删除后调用）
    pub fn clear_gc_eligible(&self, ids: &[VersionId]) {
        let mut eligible = self.gc_eligible.lock();
        for id in ids {
            eligible.remove(id);
        }
    }
}

impl Drop for QuotaManager {
    fn drop(&mut self) {
        // 引擎结束后必须从共享账本里摘掉自己的那份，否则 global 配额会一直
        // 把已关闭 session 的字节算进总量。
        if let Some((ledger, id)) = &self.shared_ledger {
            ledger.release(*id);
        }
    }
}

/// 把 Rope 的内容转成字节（保留 UTF-8；非 UTF-8 的内容用 lossy 转）。
fn rope_to_bytes(rope: &rope::Rope) -> Vec<u8> {
    // DeltaReplay 的 apply 依赖 UTF-8 文本语义，故用 to_string 路径。
    rope.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version_tree::SnapshotTrigger;

    #[test]
    fn shared_ledger_sums_engine_usage_and_releases_on_drop() {
        let ledger = GlobalQuotaLedger::new();
        let first = QuotaManager::with_shared_ledger(1024, Arc::clone(&ledger));
        {
            let second = QuotaManager::with_shared_ledger(1024, Arc::clone(&ledger));
            assert_eq!(first.budget_usage(400), 400);
            assert_eq!(second.budget_usage(600), 1000);
            assert_eq!(ledger.total_bytes(), 1000);
        }
        // 第二个引擎析构后它的占用必须从全局总量里消失。
        assert_eq!(ledger.total_bytes(), 400);
    }

    #[test]
    fn orphan_branches_become_eligible_after_grace_period() {
        let tree = VersionTree::new();
        let path = [7u8; 32];
        let root = tree.advance_head(
            path,
            1,
            1,
            None,
            Some([1u8; 32]),
            None,
            0,
            SnapshotTrigger::Write,
        );
        // parent 不是当前 HEAD → 旧 HEAD 变成 orphan（§4.4 分叉）。
        tree.advance_head(
            path,
            2,
            2,
            None,
            Some([2u8; 32]),
            None,
            0,
            SnapshotTrigger::Write,
        );
        assert!(tree.get_orphans().contains(&root));

        let quota = QuotaManager::new(u64::MAX);
        // 默认 24h grace period 内不该标记任何东西。
        quota.prune_orphan_branches(&tree, Instant::now());
        assert_eq!(quota.gc_eligible_count(), 0);

        quota.set_grace_period(Duration::ZERO);
        quota.prune_orphan_branches(&tree, Instant::now());
        assert!(
            quota.is_gc_eligible(root),
            "orphan must become a GC candidate"
        );
    }

    #[test]
    fn git_commit_marks_only_pre_commit_deltas() {
        let tree = VersionTree::new();
        let path = [9u8; 32];
        let base = tree.advance_head(
            path,
            1,
            1,
            None,
            Some([1u8; 32]),
            None,
            0,
            SnapshotTrigger::Write,
        );
        let delta = crate::version_tree::DeltaRef {
            hash: [2u8; 32],
            compressed_size: 4,
        };
        let pre_commit = tree.advance_head(
            path,
            2,
            2,
            Some(base),
            None,
            Some(delta.clone()),
            1,
            SnapshotTrigger::Write,
        );
        let post_commit = tree.advance_head(
            path,
            9,
            9,
            Some(pre_commit),
            None,
            Some(delta),
            2,
            SnapshotTrigger::Write,
        );

        let quota = QuotaManager::new(u64::MAX);
        let marked = quota.on_git_commit(&tree, 5);

        assert_eq!(marked, 1);
        assert!(quota.is_gc_eligible(pre_commit));
        assert!(
            !quota.is_gc_eligible(post_commit),
            "post-commit delta must be kept"
        );
        assert!(
            !quota.is_gc_eligible(base),
            "full snapshots are not delta garbage"
        );
    }
}
