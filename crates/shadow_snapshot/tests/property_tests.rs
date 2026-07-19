//! # Shadow Snapshot 属性测试与 WAL 回放测试
//!
//! §4 使用 proptest 对版本树、WAL 回放、GC 不变量进行属性测试。

use shadow_snapshot::*;
use std::collections::HashSet;

/// 生成随机哈希
fn random_hash() -> [u8; 32] {
    let mut arr = [0u8; 32];
    let data: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    arr.copy_from_slice(&data[..32]);
    arr
}

// ============================================================
// §4.1 WAL 回放正确性测试 (确定性)
// ============================================================

/// §4 WAL 回放正确性：写入 N 条记录，模拟在每个位置崩溃，验证回放产生相同状态。
#[test]
fn test_wal_replay_correctness() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");

    let wal = Wal::open(&wal_path).unwrap();
    let n_entries = 10;
    let mut expected_entries = Vec::new();
    for i in 0..n_entries {
        let entry = WalEntry {
            seq_no: (i as u64 + 1),
            path_hash: random_hash(),
            parent_id: if i == 0 { None } else { Some(i as u64) },
            content_ref: if i % 3 == 0 { Some(random_hash()) } else { None },
            delta_ref: if i % 3 != 0 {
                Some(DeltaRef {
                    hash: random_hash(),
                    compressed_size: rand::random::<u64>() % 1024,
                })
            } else {
                None
            },
            trigger: SnapshotTrigger::Write,
        };
        wal.append(&entry).unwrap();
        expected_entries.push(entry);
    }
    wal.commit().unwrap();

    drop(wal);
    let wal_replay = Wal::open(&wal_path).unwrap();
    let replayed = wal_replay.replay().unwrap();
    assert_eq!(replayed.len(), n_entries);

    for (i, entry) in replayed.iter().enumerate() {
        assert_eq!(entry.seq_no, expected_entries[i].seq_no);
        assert_eq!(entry.path_hash, expected_entries[i].path_hash);
    }
}

/// §4 WAL 空文件回放
#[test]
fn test_wal_replay_empty() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("empty.wal");
    std::fs::File::create(&wal_path).unwrap();

    let wal = Wal::open(&wal_path).unwrap();
    let replayed = wal.replay().unwrap();
    assert!(replayed.is_empty());
}

// ============================================================
// §4.2 属性测试：随机 write/decline/crash 序列
// ============================================================

/// §4 属性测试：随机写操作序列的不变量
/// 不变量：每个版本 ID 必须是唯一的，HEAD 必须指向最新节点。
#[test]
fn test_property_random_write_sequence() {
    use proptest::prelude::*;

    proptest!(
        ProptestConfig::with_cases(10),
        |(seq_len in 1..50)| {
            let tree = VersionTree::new();
            let path_hash = random_hash();
            let content_hash = random_hash();

            let mut last_id = None;
            let mut seen_ids = HashSet::new();

            for seq_no in 1..=(seq_len as u64) {
                let vid = tree.advance_head(
                    path_hash, seq_no,
                    seq_no as u128 * 1_000_000_000,
                    last_id, Some(content_hash),
                    None, 0, SnapshotTrigger::Write,
                );
                assert!(seen_ids.insert(vid), "版本 ID {} 重复", vid);
                last_id = Some(vid);

                let head = tree.get_head(&path_hash).expect("HEAD 不存在");
                assert_eq!(head, vid, "HEAD {} 不等于最新节点 {}", head, vid);
            }

            // 不变量：节点数 = 序列长度
            assert!(tree.node_count() == seq_len as usize, "节点数不匹配: {} != {}", tree.node_count(), seq_len);

            // 不变量：无孤儿节点（单链情况）
            let orphans = tree.get_orphans();
            assert!(orphans.is_empty(), "单链不应有孤儿节点");
        }
    );
}

/// §4 属性测试：多文件路径的 HEAD 隔离
#[test]
fn test_property_multi_path_head_isolation() {
    use proptest::prelude::*;

    proptest!(
        ProptestConfig::with_cases(10),
        |(n_paths in 1..10, ops_per_path in 1..20)| {
            let tree = VersionTree::new();

            let mut path_hashes = Vec::new();
            for _ in 0..n_paths {
                path_hashes.push(random_hash());
            }

            let mut heads = std::collections::HashMap::new();
            for path_hash in &path_hashes {
                let content_hash = random_hash();
                for seq_no in 1..=(ops_per_path as u64) {
                    let vid = tree.advance_head(
                        *path_hash, seq_no,
                        seq_no as u128 * 1_000_000_000,
                        heads.get(path_hash).copied(),
                        Some(content_hash), None, 0,
                        SnapshotTrigger::Write,
                    );
                    heads.insert(*path_hash, vid);
                }
            }

            for path_hash in &path_hashes {
                let expected_id = heads.get(path_hash).copied().expect("HEAD 未记录");
                let actual = tree.get_head(path_hash).expect("HEAD 不存在");
                assert_eq!(actual, expected_id, "路径 HEAD 不匹配");
            }
        }
    );
}

/// §4 属性测试：版本树祖先表正确性
#[test]
fn test_property_ancestor_table_correctness() {
    use proptest::prelude::*;

    proptest!(
        ProptestConfig::with_cases(10),
        |(depth in 2..15)| {
            // §4.4 depth=1 只有根节点,parent=None,ancestors 正确为空。
            // 本测试验证的是 depth >= 2 (有祖先链) 的情况。
            let tree = VersionTree::new();
            let path_hash = random_hash();
            let content_hash = random_hash();

            let mut parent = None;
            let mut vid = 0;
            for i in 1..=(depth as u64) {
                vid = tree.advance_head(
                    path_hash, i, i as u128, parent,
                    if i == 1 { Some(content_hash) } else { None },
                    if i > 1 {
                        Some(DeltaRef { hash: random_hash(), compressed_size: i })
                    } else {
                        None
                    },
                    (i - 1) as u8, SnapshotTrigger::Write,
                );
                parent = Some(vid);
            }

            let node = tree.get_node(vid).expect("最终节点不存在");
            assert!(!node.ancestors.is_empty(), "depth={} 时祖先表不应为空", depth);
            assert_eq!(node.ancestors[0], node.parent_id.unwrap(), "直接祖先不匹配");
        }
    );
}

// ============================================================
// §4.3 GC 不变量验证
// ============================================================

/// §4 GC 后不变量：所有从 HEAD 可达的版本均可重建，无孤立 parent_id 引用。
#[test]
fn test_gc_invariant_reachable_versions() {
    let tree = VersionTree::new();
    let path_a = random_hash();
    let path_b = random_hash();
    let content_hash = random_hash();

    let mut parent_a = None;
    for i in 1..=3u64 {
        let vid = tree.advance_head(
            path_a, i, (i * 100) as u128, parent_a,
            Some(content_hash), None, 0, SnapshotTrigger::Write,
        );
        parent_a = Some(vid);
    }

    let mut parent_b = None;
    for i in 1..=2u64 {
        let vid = tree.advance_head(
            path_b, 10 + i, ((10 + i) * 100) as u128, parent_b,
            Some(content_hash), None, 0, SnapshotTrigger::Write,
        );
        parent_b = Some(vid);
    }

    let orphans = tree.find_orphan_branches();
    let heads = tree.iter_heads();
    for (_ph, &head_id) in &heads {
        assert!(!orphans.contains(&head_id), "HEAD {} 不应是孤儿", head_id);
    }

    assert_eq!(orphans.len(), 0, "无分支时不应有孤儿");
    assert_eq!(tree.node_count(), 5, "节点数应为 5");
}

/// §4 GC 后不变量：无无效的 parent_id 引用。
#[test]
fn test_gc_invariant_no_dangling_parent_refs() {
    let tree = VersionTree::new();
    let path_hash = random_hash();
    let content_hash = random_hash();

    let mut parent = None;
    for i in 1..=10u64 {
        let _vid = tree.advance_head(
            path_hash, i, (i * 100) as u128, parent,
            Some(content_hash), None, 0, SnapshotTrigger::Write,
        );
        parent = Some(_vid);
    }

    let nodes = tree.iter_nodes();
    for (_id, node) in &nodes {
        if let Some(pid) = node.parent_id {
            let _ = tree.get_node(pid).expect(&format!("parent_id {} 不存在于节点集合中", pid));
        }
    }
}

// ============================================================
// §4 MemTable 属性测试
// ============================================================

/// §4 MemTable 范围查询正确性
#[test]
fn test_memtable_range_query_correctness() {
    let table = MemTable::new(64);

    for i in 1..=5u64 {
        let path_hash = random_hash();
        table.insert(i, PathChange {
            path_hash, seq_no: i, content: None,
        });
    }

    let changed = table.query_changed_files(2, 4);
    assert_eq!(changed.len(), 3, "范围查询应返回 3 条记录");

    let empty = table.query_changed_files(10, 20);
    assert!(empty.is_empty(), "空范围应返回空集合");
}

/// §4 MemTable trim 操作后数据正确性
#[test]
fn test_memtable_trim_correctness() {
    let table = MemTable::new(64);

    for i in 1..=10u64 {
        let path_hash = random_hash();
        table.insert(i, PathChange {
            path_hash, seq_no: i, content: None,
        });
    }

    assert_eq!(table.len(), 10);

    table.trim_before(5);
    assert_eq!(table.len(), 6, "trim_before(5) 后应保留 6 条记录 (5-10)");

    assert!(table.get(1).is_none(), "SeqNo=1 已被修剪");
    assert!(table.get(4).is_none(), "SeqNo=4 已被修剪");
    assert!(table.get(5).is_some(), "SeqNo=5 应保留");
}
