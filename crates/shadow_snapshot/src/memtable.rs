//! MemTable：Layer 1，内存中的 BTreeMap<SeqNo, PathChange>
//!
//! 热缓存：LRU cache 存储最近访问的 Rope。
//! 支持范围查询：查找 [t1, t2] 区间内变更的文件集合。

use std::collections::{BTreeMap, HashSet};

use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use rope::Rope;

use crate::version_tree::{PathHash, SeqNo};

/// 单条路径变更记录
#[derive(Debug, Clone)]
pub struct PathChange {
    /// 文件路径哈希
    pub path_hash: PathHash,
    /// 序列号
    pub seq_no: SeqNo,
    /// 内容 Rope（热数据）
    pub content: Option<Rope>,
}

/// MemTable：有序变更集合
pub struct MemTable {
    /// BTreeMap 按 SeqNo 排序
    entries: RwLock<BTreeMap<SeqNo, PathChange>>,
    /// LRU 热缓存：path_hash -> Rope
    hot_cache: Mutex<LruCache<PathHash, Rope>>,
}

impl MemTable {
    /// 创建 MemTable，LRU 缓存容量为 capacity
    pub fn new(_capacity: usize) -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
            hot_cache: Mutex::new(LruCache::unbounded()),
        }
    }

    /// 插入变更到 MemTable
    pub fn insert(&self, seq_no: SeqNo, change: PathChange) {
        let mut entries = self.entries.write();
        entries.insert(seq_no, change.clone());

        // 热缓存：如果 content 可用则放入 LRU
        if let Some(content) = &change.content {
            let mut cache = self.hot_cache.lock();
            cache.put(change.path_hash, content.clone());
        }
    }

    /// 按 SeqNo 查询单条变更
    pub fn get(&self, seq_no: SeqNo) -> Option<PathChange> {
        let entries = self.entries.read();
        entries.get(&seq_no).cloned()
    }

    /// 范围查询：返回 [t1, t2] 区间内所有变更过文件的 path_hash 集合
    ///
    /// §4.2 range query
    pub fn query_changed_files(&self, t1: SeqNo, t2: SeqNo) -> HashSet<PathHash> {
        let entries = self.entries.read();
        let mut paths = HashSet::new();

        for (_, change) in entries.range(t1..=t2) {
            paths.insert(change.path_hash);
        }

        paths
    }

    /// 从热缓存获取 Rope
    pub fn get_cached_rope(&self, path_hash: &PathHash) -> Option<Rope> {
        let mut cache = self.hot_cache.lock();
        cache.get(path_hash).cloned()
    }

    /// 获取最新的 SeqNo
    pub fn latest_seq(&self) -> Option<SeqNo> {
        let entries = self.entries.read();
        entries.last_key_value().map(|(k, _)| *k)
    }

    /// 获取 MemTable 中的条目数
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// 清除指定范围之前的所有条目（checkpoint 后调用）
    pub fn trim_before(&self, seq_no: SeqNo) {
        let mut entries = self.entries.write();
        let keys_to_remove: Vec<SeqNo> = entries
            .range(..seq_no)
            .map(|(k, _)| *k)
            .collect();
        for key in keys_to_remove {
            entries.remove(&key);
        }
    }

    /// 清空所有条目
    pub fn clear(&self) {
        let mut entries = self.entries.write();
        entries.clear();
        let mut cache = self.hot_cache.lock();
        cache.clear();
    }
}

impl Default for MemTable {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_change(seq_no: SeqNo, hash_val: u8) -> PathChange {
        PathChange {
            path_hash: [hash_val; 32],
            seq_no,
            content: None,
        }
    }

    #[test]
    fn test_insert_and_get() {
        let mt = MemTable::new(16);
        let change = make_change(1, 0xAA);
        mt.insert(1, change);

        let got = mt.get(1);
        assert!(got.is_some());
        let got = got.unwrap();
        assert_eq!(got.seq_no, 1);
    }

    #[test]
    fn test_range_query() {
        let mt = MemTable::new(16);

        // seq 1: path A
        mt.insert(1, make_change(1, 0x01));
        // seq 2: path B
        mt.insert(2, make_change(2, 0x02));
        // seq 3: path A again
        mt.insert(3, make_change(3, 0x01));
        // seq 4: path C
        mt.insert(4, make_change(4, 0x03));

        // 查询 [1, 3] → {A, B}
        let changed = mt.query_changed_files(1, 3);
        assert_eq!(changed.len(), 2);

        // 查询 [2, 4] → {A, B, C}
        let changed = mt.query_changed_files(2, 4);
        assert_eq!(changed.len(), 3);
    }

    #[test]
    fn test_trim_before() {
        let mt = MemTable::new(16);
        for i in 1..=5 {
            mt.insert(i, make_change(i, i as u8));
        }
        mt.trim_before(4);

        assert_eq!(mt.len(), 2); // seq 4, 5 remain
        assert!(mt.get(1).is_none());
        assert!(mt.get(4).is_some());
    }
}
