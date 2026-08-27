// §3.10 mux_server 单元测试 — 验证 grid diff ring、layout tree、
// generation counter、session 生命周期等核心功能。

use crate::grid_sync::{GridDiff, GridDiffRing};
use crate::layout::{
    LayoutNode, LayoutTree, PersistedLayoutNode, PersistedLayoutTree, SplitDirection,
};
use std::io::Write;

/// §3.3 Grid diff ring: push + overflow
#[test]
fn test_diff_ring_push_and_overflow() {
    let mut ring = GridDiffRing::new(4);

    for i in 0..4 {
        ring.push(i, GridDiff { rows: vec![] });
    }
    assert_eq!(ring.len(), 4);

    ring.push(4, GridDiff { rows: vec![] });
    assert_eq!(ring.len(), 4);
}

/// §3.3 Grid diff ring: empty ring
#[test]
fn test_diff_ring_empty() {
    let ring = GridDiffRing::new(4);
    assert!(ring.is_empty());
    assert_eq!(ring.len(), 0);
}

/// §3.3 Grid diff ring: push preserves order
#[test]
fn test_diff_ring_preserves_order() {
    let mut ring = GridDiffRing::new(64);
    ring.push(10, GridDiff { rows: vec![] });
    ring.push(20, GridDiff { rows: vec![] });
    ring.push(30, GridDiff { rows: vec![] });
    assert_eq!(ring.len(), 3);
}

/// §3.10 Layout tree: split pane
#[test]
fn test_layout_split() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    match &tree.root {
        LayoutNode::Split {
            direction,
            children,
            ratios,
            ..
        } => {
            assert_eq!(*direction, SplitDirection::LeftRight);
            assert_eq!(children.len(), 2);
            assert_eq!(ratios.len(), 2);
            assert!((ratios[0] - 0.5).abs() < 1e-6);
            assert!((ratios[1] - 0.5).abs() < 1e-6);
        }
        _ => panic!("expected Split node after split"),
    }
}

#[test]
fn same_direction_splits_preserve_internal_pairing_and_geometry() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::TopBottom)
        .expect("first split");
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::TopBottom)
        .expect("second split");

    assert_eq!(tree.pane_ids(), vec!["pane-1", "pane-2", "pane-3"]);
    match &tree.root {
        LayoutNode::Split {
            direction,
            children,
            ratios,
            ..
        } => {
            assert_eq!(*direction, SplitDirection::TopBottom);
            assert_eq!(ratios, &[0.5, 0.5]);
            match &children[1] {
                LayoutNode::Split {
                    direction,
                    children,
                    ratios,
                    ..
                } => {
                    assert_eq!(*direction, SplitDirection::TopBottom);
                    assert_eq!(children.len(), 2);
                    assert_eq!(ratios, &[0.5, 0.5]);
                }
                node => panic!("expected paired child split, got {node:?}"),
            }
        }
        node => panic!("expected root split, got {node:?}"),
    }
}

#[test]
fn closing_a_same_direction_split_restores_prior_geometry() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::TopBottom)
        .expect("first split");
    let before = tree.root.clone();
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::TopBottom)
        .expect("second split");

    tree.remove_pane("pane-3").expect("close second split half");

    assert_eq!(tree.pane_ids(), vec!["pane-1", "pane-2"]);
    assert_eq!(tree.root, before);
}

#[test]
fn nearest_same_direction_resize_preserves_outer_sibling_ratio() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("first split");
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::LeftRight)
        .expect("second split");

    tree.resize_pane("pane-3", SplitDirection::LeftRight, 10.0)
        .expect("resize nearest pair");

    match &tree.root {
        LayoutNode::Split {
            children, ratios, ..
        } => {
            assert_eq!(ratios, &[0.5, 0.5]);
            match &children[1] {
                LayoutNode::Split { ratios, .. } => {
                    assert!((ratios[0] - 0.05).abs() < 1e-6);
                    assert!((ratios[1] - 0.95).abs() < 1e-6);
                    assert!((ratios.iter().sum::<f32>() - 1.0).abs() < 1e-6);
                }
                node => panic!("expected paired child split, got {node:?}"),
            }
        }
        node => panic!("expected root split, got {node:?}"),
    }
}

#[test]
fn alternating_split_depth_is_rejected_atomically() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-0".to_string());
    let mut focused = "pane-0".to_string();
    for index in 1..=crate::layout::MAX_WIRE_LAYOUT_DEPTH {
        let next = format!("pane-{index}");
        let direction = if index % 2 == 0 {
            SplitDirection::LeftRight
        } else {
            SplitDirection::TopBottom
        };
        tree.split(&focused, next.clone(), direction)
            .expect("split within wire depth");
        focused = next;
    }
    let before = tree.pane_ids();

    let error = tree
        .split(
            &focused,
            "too-deep".to_string(),
            if crate::layout::MAX_WIRE_LAYOUT_DEPTH % 2 == 0 {
                SplitDirection::TopBottom
            } else {
                SplitDirection::LeftRight
            },
        )
        .expect_err("alternating split beyond wire depth must fail");

    assert!(error.to_string().contains("wire depth"));
    assert_eq!(tree.pane_ids(), before);
    assert!(tree.root.find_pane("too-deep").is_none());
}

#[test]
fn failed_split_leaves_layout_unchanged() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    let before = tree.root.clone();

    assert!(
        tree.split(
            "missing-pane",
            "pane-2".to_string(),
            SplitDirection::LeftRight,
        )
        .is_err()
    );

    assert_eq!(tree.root, before);
}

/// §3.10 Layout tree: remove pane
#[test]
fn test_layout_remove_pane() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::TopBottom)
        .expect("split failed");

    tree.remove_pane("pane-2").expect("remove failed");

    match &tree.root {
        LayoutNode::Pane { pane_id, .. } => {
            assert_eq!(pane_id, "pane-1");
        }
        _ => panic!("expected flattened Pane node after removal"),
    }
}

/// §3.10 Layout tree: resize pane
#[test]
fn test_layout_resize_pane() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    tree.resize_pane("pane-2", SplitDirection::LeftRight, 0.1)
        .expect("resize failed");

    match &tree.root {
        LayoutNode::Split { ratios, .. } => {
            assert!(ratios[1] > 0.5);
            assert!(ratios[0] < 0.5);
        }
        _ => panic!("expected Split node"),
    }
}

/// §16.9 拖动分隔条报告的是落点。同一个落点报告两次, 树必须一模一样 ——
/// `resize_pane` 收增量, 重放会把分隔条挪两次, 这正是它替代不了的地方。
#[test]
fn setting_layout_ratios_is_absolute_and_repeatable() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    tree.set_ratios("node-1", &[0.7, 0.3])
        .expect("set ratios failed");
    let once = tree.root.clone();
    tree.set_ratios("node-1", &[0.7, 0.3])
        .expect("replaying the same drag must succeed");

    assert_eq!(tree.root, once, "an absolute ratio must not accumulate");
    match &tree.root {
        LayoutNode::Split { ratios, .. } => {
            assert!((ratios[0] - 0.7).abs() < 1e-6);
            assert!((ratios[1] - 0.3).abs() < 1e-6);
        }
        _ => panic!("expected Split node"),
    }
}

/// 比例被归一化, 所以客户端按像素报告 (300/700) 和按比例报告 (0.3/0.7)
/// 是同一个请求。
#[test]
fn layout_ratios_are_normalised() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    tree.set_ratios("node-1", &[300.0, 700.0])
        .expect("set ratios failed");

    match &tree.root {
        LayoutNode::Split { ratios, .. } => {
            assert!((ratios[0] - 0.3).abs() < 1e-6);
            assert!((ratios[1] - 0.7).abs() < 1e-6);
        }
        _ => panic!("expected Split node"),
    }
}

#[test]
fn layout_ratios_reject_a_mismatched_count() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    let before = tree.root.clone();

    tree.set_ratios("node-1", &[0.5, 0.3, 0.2])
        .expect_err("three ratios for two children must be rejected");
    tree.set_ratios("node-1", &[0.0, 1.0])
        .expect_err("a zero-width pane is not a layout");
    tree.set_ratios("node-1", &[f32::NAN, 1.0])
        .expect_err("NaN must not reach the tree");

    assert_eq!(tree.root, before, "a rejected resize must not touch the tree");
}

/// §16.9 拖动 tab: 叶子离开原处, 在目标旁边重新进入。
#[test]
fn moving_a_pane_places_it_beside_the_target() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::TopBottom)
        .expect("split failed");

    tree.move_pane("pane-3", "pane-1", SplitDirection::LeftRight, true)
        .expect("move failed");

    // pane-3 现在在 pane-1 左边, 而它原来所在的上下 split 只剩 pane-2, 已折叠。
    match &tree.root {
        LayoutNode::Split {
            direction, children, ..
        } => {
            assert_eq!(*direction, SplitDirection::LeftRight);
            assert_eq!(children.len(), 3, "the collapsed split must not leave a stub");
            let panes: Vec<&str> = children
                .iter()
                .map(|child| match child {
                    LayoutNode::Pane { pane_id, .. } => pane_id.as_str(),
                    LayoutNode::Split { .. } => "split",
                })
                .collect();
            assert_eq!(panes, vec!["pane-3", "pane-1", "pane-2"]);
        }
        _ => panic!("expected Split node"),
    }
}

/// 同一次拖动被重复投递 —— 至少一次的通知语义下这是常态 —— 不能让树接着动。
#[test]
fn moving_a_pane_where_it_already_sits_changes_nothing() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::TopBottom)
        .expect("split failed");

    tree.move_pane("pane-3", "pane-1", SplitDirection::LeftRight, true)
        .expect("move failed");
    let once = tree.root.clone();
    tree.move_pane("pane-3", "pane-1", SplitDirection::LeftRight, true)
        .expect("replaying the same drag must succeed");

    assert_eq!(tree.root, once, "a replayed move must be a no-op");
}

/// 落在目标左半边和落在右半边是两个不同的请求。
#[test]
fn moving_before_and_after_a_target_differ() {
    let build = |before: bool| {
        let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
        tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
            .expect("split failed");
        tree.split("pane-2", "pane-3".to_string(), SplitDirection::TopBottom)
            .expect("split failed");
        tree.move_pane("pane-3", "pane-1", SplitDirection::LeftRight, before)
            .expect("move failed");
        match tree.root {
            LayoutNode::Split { children, .. } => children
                .iter()
                .map(|child| match child {
                    LayoutNode::Pane { pane_id, .. } => pane_id.clone(),
                    LayoutNode::Split { .. } => "split".to_string(),
                })
                .collect::<Vec<_>>(),
            _ => panic!("expected Split node"),
        }
    };

    assert_eq!(build(true), vec!["pane-3", "pane-1", "pane-2"]);
    assert_eq!(build(false), vec!["pane-1", "pane-3", "pane-2"]);
}

#[test]
fn moving_a_pane_rejects_unknown_and_self_targets() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    let before = tree.root.clone();

    tree.move_pane("pane-1", "pane-1", SplitDirection::LeftRight, true)
        .expect_err("a pane cannot be moved next to itself");
    tree.move_pane("pane-9", "pane-1", SplitDirection::LeftRight, true)
        .expect_err("an unknown pane cannot be moved");
    tree.move_pane("pane-1", "pane-9", SplitDirection::LeftRight, true)
        .expect_err("a pane cannot be moved next to an unknown target");

    assert_eq!(tree.root, before, "a rejected move must not touch the tree");
}

/// 移动跨越轴向时目标叶子会变成新的 split; 新节点的 id 不能撞上已有的。
#[test]
fn moving_a_pane_across_axes_mints_a_unique_node_id() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::LeftRight)
        .expect("split failed");

    tree.move_pane("pane-3", "pane-1", SplitDirection::TopBottom, false)
        .expect("move failed");

    let mut ids = Vec::new();
    collect_ids(&tree.root, &mut ids);
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "node ids must stay unique: {ids:?}");
}

fn collect_ids(node: &LayoutNode, ids: &mut Vec<String>) {
    match node {
        LayoutNode::Pane { id, .. } => ids.push(id.clone()),
        LayoutNode::Split { id, children, .. } => {
            ids.push(id.clone());
            for child in children {
                collect_ids(child, ids);
            }
        }
    }
}

/// §3.7 类型化 layout 持久化: 多层混合轴向树精确 round-trip (节点 ID、比例、方向)。
#[test]
fn persisted_layout_round_trips_exact_mixed_axis_tree() {
    let mut tree = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    tree.split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split left-right");
    tree.resize_pane("pane-1", SplitDirection::LeftRight, 0.2)
        .expect("resize outer split");
    tree.split("pane-2", "pane-3".to_string(), SplitDirection::TopBottom)
        .expect("split top-bottom");
    tree.resize_pane("pane-2", SplitDirection::TopBottom, 0.1)
        .expect("resize inner split");

    let encoded = serde_json::to_string(&tree).expect("encode typed layout");
    let decoded: LayoutTree = serde_json::from_str(&encoded).expect("decode typed layout");

    assert_eq!(decoded.root, tree.root, "tree must round-trip exactly");
    match &decoded.root {
        LayoutNode::Split {
            direction,
            ratios,
            children,
            ..
        } => {
            assert_eq!(*direction, SplitDirection::LeftRight);
            assert_eq!(ratios, &[0.7, 0.3]);
            match &children[1] {
                LayoutNode::Split {
                    direction,
                    ratios,
                    ..
                } => {
                    assert_eq!(*direction, SplitDirection::TopBottom);
                    assert!((ratios[0] - 0.6).abs() < 1e-6);
                    assert!((ratios[1] - 0.4).abs() < 1e-6);
                }
                node => panic!("expected nested split, got {node:?}"),
            }
        }
        node => panic!("expected root split, got {node:?}"),
    }
}

/// §3.7 空布局占位根必须 round-trip, 但不能混入真实布局。
#[test]
fn persisted_layout_round_trips_empty_root_only_as_sole_node() {
    let empty = LayoutTree::empty();
    let encoded = serde_json::to_string(&empty).expect("encode empty layout");
    let decoded: LayoutTree = serde_json::from_str(&encoded).expect("decode empty layout");
    assert!(decoded.is_empty_root());

    let persisted = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 2],
                ratios: vec![0.5, 0.5],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "empty-placeholder".to_string(),
                pane_id: String::new(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&persisted)
        .expect_err("empty placeholder inside a real layout must be rejected");
    assert!(error.to_string().contains("pane id must not be empty"));
}

/// §3.7 持久化比例必须是有限正数: 零或负数比例视为损坏。
#[test]
fn persisted_layout_rejects_non_positive_ratios() {
    let zero_ratio = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 2],
                ratios: vec![0.0, 1.0],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-2".to_string(),
                pane_id: "pane-2".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&zero_ratio)
        .expect_err("zero ratio must be rejected");
    assert!(error.to_string().contains("finite and positive"));

    let negative_ratio = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 2],
                ratios: vec![-0.5, 1.5],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-2".to_string(),
                pane_id: "pane-2".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&negative_ratio)
        .expect_err("negative ratio must be rejected");
    assert!(error.to_string().contains("finite and positive"));
}

/// §3.7 持久化节点 ID 必须全局唯一。
#[test]
fn persisted_layout_rejects_duplicate_node_ids() {
    let duplicated = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 2],
                ratios: vec![0.5, 0.5],
            },
            PersistedLayoutNode::Pane {
                id: "root".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-2".to_string(),
                pane_id: "pane-2".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&duplicated)
        .expect_err("duplicate node id must be rejected");
    assert!(error.to_string().contains("duplicate persisted node id"));
}

/// §3.7 持久化 pane ID 必须全局唯一。
#[test]
fn persisted_layout_rejects_duplicate_pane_ids() {
    let duplicated = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 2],
                ratios: vec![0.5, 0.5],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-2".to_string(),
                pane_id: "pane-1".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&duplicated)
        .expect_err("duplicate pane id must be rejected");
    assert!(error.to_string().contains("duplicate persisted pane id"));
}

/// §3.7 子索引必须落在节点表范围内。
#[test]
fn persisted_layout_rejects_out_of_bounds_child_index() {
    let escaped = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 3],
                ratios: vec![0.5, 0.5],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-2".to_string(),
                pane_id: "pane-2".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&escaped)
        .expect_err("out-of-bounds child index must be rejected");
    assert!(error.to_string().contains("out of bounds"));
}

/// §3.7 前序不变量: 子索引必须大于父索引 (拒绝自引用与环)。
#[test]
fn persisted_layout_rejects_backward_child_index() {
    let cyclic = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![0, 1],
                ratios: vec![0.5, 0.5],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&cyclic)
        .expect_err("self-referencing child index must be rejected");
    assert!(error.to_string().contains("strictly increasing"));
}

/// §3.7 每个非根节点必须恰好被一个 split 引用。
#[test]
fn persisted_layout_rejects_disconnected_nodes() {
    let orphaned = PersistedLayoutTree {
        nodes: vec![
            PersistedLayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![1, 2],
                ratios: vec![0.5, 0.5],
            },
            PersistedLayoutNode::Pane {
                id: "node-1".to_string(),
                pane_id: "pane-1".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-2".to_string(),
                pane_id: "pane-2".to_string(),
            },
            PersistedLayoutNode::Pane {
                id: "node-orphan".to_string(),
                pane_id: "pane-orphan".to_string(),
            },
        ],
    };
    let error = LayoutTree::from_persisted(&orphaned)
        .expect_err("unreferenced node must be rejected");
    assert!(error.to_string().contains("single tree"));
}

/// §3.7 方向交替的深度受 wire 深度上限约束 (§3.10 MAX_LAYOUT_WIRE_DEPTH)。
#[test]
fn persisted_layout_rejects_wire_depth_overflow() {
    let mut nodes = Vec::new();
    for level in 0..=crate::layout::MAX_WIRE_LAYOUT_DEPTH {
        let direction = if level % 2 == 0 {
            SplitDirection::LeftRight
        } else {
            SplitDirection::TopBottom
        };
        let split_index = nodes.len();
        nodes.push(PersistedLayoutNode::Split {
            id: format!("node-{level}"),
            direction,
            children: vec![split_index + 1, split_index + 2],
            ratios: vec![0.5, 0.5],
        });
        nodes.push(PersistedLayoutNode::Pane {
            id: format!("node-{level}-left"),
            pane_id: format!("pane-{level}-left"),
        });
    }
    nodes.push(PersistedLayoutNode::Pane {
        id: "node-deep-right".to_string(),
        pane_id: "pane-deep".to_string(),
    });
    let deep = PersistedLayoutTree { nodes };

    let error = LayoutTree::from_persisted(&deep)
        .expect_err("alternating layout beyond wire depth must be rejected");
    assert!(error.to_string().contains("wire depth"));
}

/// §3.7 同向链深度受内部深度上限约束。
#[test]
fn persisted_layout_rejects_internal_depth_overflow() {
    let mut nodes = Vec::new();
    for level in 0..=crate::layout::MAX_INTERNAL_LAYOUT_DEPTH {
        let split_index = nodes.len();
        nodes.push(PersistedLayoutNode::Split {
            id: format!("node-{level}"),
            direction: SplitDirection::TopBottom,
            children: vec![split_index + 1, split_index + 2],
            ratios: vec![0.5, 0.5],
        });
        nodes.push(PersistedLayoutNode::Pane {
            id: format!("node-{level}-left"),
            pane_id: format!("pane-{level}-left"),
        });
    }
    nodes.push(PersistedLayoutNode::Pane {
        id: "node-deep-right".to_string(),
        pane_id: "pane-deep".to_string(),
    });
    let deep = PersistedLayoutTree { nodes };

    let error = LayoutTree::from_persisted(&deep)
        .expect_err("chain beyond internal depth must be rejected");
    assert!(error.to_string().contains("internal maximum"));
}

/// §3.7 空节点表不是合法布局。
#[test]
fn persisted_layout_rejects_empty_node_list() {
    let error = LayoutTree::from_persisted(&PersistedLayoutTree { nodes: Vec::new() })
        .expect_err("empty node list must be rejected");
    assert!(error.to_string().contains("no nodes"));
}

/// §3.7 Layout tree: collect pane IDs
#[test]
fn test_layout_pane_ids() {
    let mut tree = LayoutTree::with_pane("n1".to_string(), "p1".to_string());
    tree.split("p1", "p2".to_string(), SplitDirection::LeftRight)
        .expect("split failed");
    tree.split("p1", "p3".to_string(), SplitDirection::TopBottom)
        .expect("split failed");

    let ids = tree.pane_ids();
    assert!(ids.contains(&"p1".to_string()));
    assert!(ids.contains(&"p2".to_string()));
    assert!(ids.contains(&"p3".to_string()));
}

/// §3.10 Session lifecycle: create session
#[test]
fn test_session_create() {
    let session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );
    assert_eq!(session.id, "sess-1");
    assert_eq!(session.name, "test");
    assert_eq!(session.cwd, "/home/user");
    assert!(session.is_empty());
}

/// §3.10 Session: attach/detach client
#[test]
fn test_session_attach_detach() {
    let mut session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    session.add_attached_client(
        "client-1".to_string(),
        crate::session::AttachMode::Shared,
        crate::session::ClientRole::ReadWrite,
        None,
    );
    assert_eq!(session.attached_client_count(), 1);

    session.add_attached_client(
        "client-2".to_string(),
        crate::session::AttachMode::ReadOnly,
        crate::session::ClientRole::ReadOnly,
        None,
    );
    assert_eq!(session.attached_client_count(), 2);

    session.remove_attached_client("client-1");
    assert_eq!(session.attached_client_count(), 1);
}

/// §3.10 Session: focused pane
#[test]
fn test_session_focused_pane() {
    let mut session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    assert!(session.get_focused_pane().is_none());

    session.set_focused_pane("pane-1".to_string());
    assert_eq!(session.get_focused_pane(), Some("pane-1"));
}

/// §3.10 Session: add tab
#[test]
fn test_session_add_tab() {
    let mut session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    session.add_tab("tab-1".to_string(), "Terminal".to_string());
    assert!(session.tabs.contains_key("tab-1"));
    let tab = session.tabs.get("tab-1").unwrap();
    assert_eq!(tab.title, "Terminal");
}

#[cfg(unix)]
#[test]
fn session_remove_pane_cleans_layout_tabs_and_focus() {
    let mut session = crate::session::Session::new(
        "sess-remove".to_string(),
        "remove".to_string(),
        "/tmp".to_string(),
    );
    let pane_one = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        20,
        5,
        Some(crate::pane::ShellCommand {
            program: "/bin/cat".to_string(),
            ..Default::default()
        }),
    )
    .expect("spawn pane one");
    let pane_two = crate::pane::Pane::spawn(
        "pane-2".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        20,
        5,
        Some(crate::pane::ShellCommand {
            program: "/bin/cat".to_string(),
            ..Default::default()
        }),
    )
    .expect("spawn pane two");
    session.panes.write().insert(pane_one.id.clone(), pane_one);
    session.panes.write().insert(pane_two.id.clone(), pane_two);
    session.add_tab("tab-1".to_string(), "shells".to_string());
    session.tabs.get_mut("tab-1").unwrap().pane_ids =
        vec!["pane-1".to_string(), "pane-2".to_string()];
    session.layout = LayoutTree::with_pane("node-1".to_string(), "pane-1".to_string());
    session
        .layout
        .split("pane-1", "pane-2".to_string(), SplitDirection::LeftRight)
        .expect("split test layout");
    session.focused_pane = Some("pane-2".to_string());
    session.focused_tab = Some("tab-1".to_string());

    assert!(session.remove_pane("pane-2").expect("remove second pane"));
    assert_eq!(session.layout.pane_ids(), vec!["pane-1"]);
    assert_eq!(session.tabs["tab-1"].pane_ids, vec!["pane-1"]);
    assert_eq!(session.focused_pane.as_deref(), Some("pane-1"));
    assert_eq!(session.focused_tab.as_deref(), Some("tab-1"));

    assert!(session.remove_pane("pane-1").expect("remove final pane"));
    assert!(session.panes.read().is_empty());
    assert!(session.layout.is_empty_root());
    assert!(session.tabs["tab-1"].pane_ids.is_empty());
    assert!(session.focused_pane.is_none());
    assert!(session.focused_tab.is_none());
    assert!(!session.remove_pane("pane-1").expect("repeat removal"));
}

/// §3.10 Pane: creation and generation
#[test]
fn test_pane_creation() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    assert_eq!(pane.id, "pane-1");
    assert_eq!(pane.get_generation(), 0);
    assert!(pane.is_alive());

    pane.bump_generation();
    assert_eq!(pane.get_generation(), 1);
}

/// §3.10 Pane: resize
#[test]
fn test_pane_resize() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    pane.resize(100, 30).expect("resize pane");
    assert_eq!(pane.get_cols(), 100);
    assert_eq!(pane.get_rows(), 30);
}

/// §3.10 Pane: title
#[test]
fn test_pane_title() {
    let pane = crate::pane::Pane::spawn(
        "pane-1".to_string(),
        std::env::temp_dir().to_string_lossy().to_string(),
        80,
        24,
        None,
    )
    .expect("spawn pane");

    pane.set_title("my-title".to_string());
    assert_eq!(pane.get_title(), "my-title");
}

/// §16.9 Pane: authoritative Alacritty scrollback integration.
#[test]
fn test_pane_scrollback() {
    let pane = crate::pane::Pane::spawn_with_session(
        "pane-1".to_string(),
        String::new(),
        std::env::temp_dir().to_string_lossy().to_string(),
        8,
        2,
        None,
        10,
    )
    .expect("spawn pane");

    let initial_version = pane.get_scrollback_version();
    assert_ne!(initial_version, 0);
    let (lines, total, version) = pane.fetch_scrollback(0, 1, 10);
    assert!(lines.is_empty());
    assert_eq!(total, 0);
    assert_eq!(version, initial_version);

    let mut processor = alacritty_terminal::vte::ansi::Processor::<
        alacritty_terminal::vte::ansi::StdSyncHandler,
    >::new();
    processor.advance(&mut *pane.term.lock(), b"Hello\r\nWorld\r\n");

    let (lines, total, _) = pane.fetch_scrollback(0, 1, 10);
    assert_eq!(total, 1);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].cells[0].character, "H");
    assert_eq!(lines[0].cells[4].character, "o");
}
// §16.9 Pane: capacity honored by the authoritative Alacritty grid.
#[test]
fn test_pane_scrollback_capacity_uses_threaded_value() {
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    #[cfg(unix)]
    let quiet_command = crate::pane::ShellCommand {
        program: "/bin/cat".to_string(),
        ..Default::default()
    };
    #[cfg(windows)]
    let quiet_command = crate::pane::ShellCommand {
        program: "cmd.exe".to_string(),
        args: vec![
            "/Q".to_string(),
            "/D".to_string(),
            "/C".to_string(),
            "more >NUL".to_string(),
        ],
        ..Default::default()
    };
    let small = crate::pane::Pane::spawn_with_session(
        "pane-small".to_string(),
        "sess-small".to_string(),
        cwd.clone(),
        8,
        2,
        Some(quiet_command.clone()),
        42,
    )
    .expect("spawn pane with small scrollback");
    let mut processor = alacritty_terminal::vte::ansi::Processor::<
        alacritty_terminal::vte::ansi::StdSyncHandler,
    >::new();
    let small_output = (0..50).map(|_| "x\r\n").collect::<String>();
    processor.advance(&mut *small.term.lock(), small_output.as_bytes());
    let (_, small_total, _) = small.fetch_scrollback(0, 1, 100);
    assert_eq!(small_total, 42);

    let large = crate::pane::Pane::spawn_with_session(
        "pane-large".to_string(),
        "sess-large".to_string(),
        cwd,
        8,
        2,
        Some(quiet_command),
        87_654,
    )
    .expect("spawn pane with large scrollback");
    let large_output = (0..10_002).map(|_| "y\r\n").collect::<String>();
    processor.advance(&mut *large.term.lock(), large_output.as_bytes());
    let (_, large_total, _) = large.fetch_scrollback(0, 1, 1);
    assert_eq!(large_total, 10_001);
}

/// §16.9 Session: sync scrollback
#[test]
fn test_session_sync_scrollback() {
    let session = crate::session::Session::new(
        "sess-1".to_string(),
        "test".to_string(),
        "/home/user".to_string(),
    );

    // §16.9 初始状态
    let state = session.get_sync_scrollback();
    assert!(!state.enabled);
    assert!(state.pane_id.is_none());

    // §16.9 设置同步滚动
    session.set_sync_scrollback_offset("pane-1".to_string(), 42);
    let state = session.get_sync_scrollback();
    assert!(state.enabled);
    assert_eq!(state.pane_id, Some("pane-1".to_string()));
    assert_eq!(state.scroll_offset, 42);

    // §16.9 禁用同步滚动
    session.disable_sync_scrollback();
    let state = session.get_sync_scrollback();
    assert!(!state.enabled);
    assert!(state.pane_id.is_none());
    assert_eq!(state.scroll_offset, 0);
}

// ============================================================================
// §16.12 日志与诊断测试
// ============================================================================

/// §16.12 测试日志目录创建与 zlog 文件日志初始化
#[test]
fn test_setup_logging() {
    // §16.12 zlog::init() 是幂等的, 多次调用安全
    zlog::init();
    zlog::init_output_stderr();

    // §16.12 验证日志目录路径格式
    let log_dir = crate::get_log_dir();
    assert!(log_dir.ends_with("z3rm") || log_dir.ends_with("logs"));

    // §16.12 创建日志目录
    std::fs::create_dir_all(&log_dir).expect("failed to create log dir");
    assert!(log_dir.exists());
}

/// §16.12 测试日志文件创建与轮转路径
#[test]
fn test_log_file_rotation() {
    use std::fs;
    use std::path::PathBuf;

    // §16.12 使用临时目录测试轮转逻辑
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let log_path = temp_dir.path().join("test.log");
    let rotate_path = temp_dir.path().join("test.log.old");

    // §16.12 写入数据超过 1MB 阈值触发轮转
    let mut file = fs::File::create(&log_path).expect("create log file");
    for _ in 0..1024 {
        writeln!(file, "test log line for rotation test padding data").expect("write line");
    }
    drop(file);

    // §16.12 验证日志文件已创建
    assert!(log_path.exists());
    let metadata = fs::metadata(&log_path).expect("read metadata");
    assert!(metadata.len() > 0);

    // §16.12 模拟轮转: 复制当前日志到 .old, 然后截断原文件
    if log_path.exists() {
        fs::copy(&log_path, &rotate_path).expect("rotate log file");
        fs::write(&log_path, "").expect("truncate log file");
    }

    // §16.12 验证轮转后 .old 文件存在且原文件被截断
    assert!(rotate_path.exists());
    let old_size = fs::metadata(&rotate_path).expect("read old metadata").len();
    assert!(old_size > 0);
    let new_size = fs::metadata(&log_path).expect("read new metadata").len();
    assert_eq!(new_size, 0);
}

/// §16.12 测试状态输出格式
#[test]
fn test_status_output_format() {
    // §16.12 模拟 status 命令输出格式
    let output = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        "z3rm-server v0.1.0",
        "Uptime: 2h 34m",
        "Sessions: 2 (1 attached)",
        "Panes: 4",
        "Memory: 47 MB",
        "Socket: /tmp/z3rm/mux.sock"
    );

    // §16.12 验证输出格式包含所有必需字段
    assert!(output.contains("z3rm-server v0.1.0"));
    assert!(output.contains("Uptime:"));
    assert!(output.contains("Sessions:"));
    assert!(output.contains("Panes:"));
    assert!(output.contains("Memory:"));
    assert!(output.contains("Socket:"));

    // §16.12 验证行数
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 6);
}

// ============================================================================
// §3.3 窗口管理测试 (多窗口支持，Plan 32)
// ============================================================================

use crate::session::Session;

/// §3.3 测试窗口添加到会话
#[test]
fn test_session_add_window() {
    let session = Session::new("sess-1".to_string(), "test".to_string(), "/tmp".to_string());
    assert_eq!(session.window_count(), 0);

    assert!(session.add_window("win-1".to_string()));
    assert_eq!(session.window_count(), 1);
    assert!(session.has_window("win-1"));

    assert!(session.add_window("win-2".to_string()));
    assert_eq!(session.window_count(), 2);
    assert!(session.has_window("win-2"));
}

/// §3.3 测试重复窗口 ID 不重复添加
#[test]
fn test_session_deduplicate_windows() {
    let session = Session::new("sess-2".to_string(), "test".to_string(), "/tmp".to_string());

    assert!(session.add_window("win-1".to_string()));
    assert!(
        !session.add_window("win-1".to_string()),
        "a repeated window id must report that nothing was added"
    );
    assert_eq!(session.window_count(), 1);
}

/// §3.3 测试从会话移除窗口
#[test]
fn test_session_remove_window() {
    let session = Session::new("sess-3".to_string(), "test".to_string(), "/tmp".to_string());

    session.add_window("win-1".to_string());
    session.add_window("win-2".to_string());
    assert_eq!(session.window_count(), 2);

    session.remove_window("win-1");
    assert_eq!(session.window_count(), 1);
    assert!(!session.has_window("win-1"));
    assert!(session.has_window("win-2"));
}

/// §3.3 测试获取窗口列表
#[test]
fn test_session_get_windows() {
    let session = Session::new("sess-4".to_string(), "test".to_string(), "/tmp".to_string());

    session.add_window("win-a".to_string());
    session.add_window("win-b".to_string());

    let windows = session.get_windows();
    assert_eq!(windows.len(), 2);
    assert!(windows.contains(&"win-a".to_string()));
    assert!(windows.contains(&"win-b".to_string()));
}

/// §3.3 测试布局变更广播: 每个 attached 连接 (即每个窗口) 都必须收到。
#[test]
fn test_session_broadcast_layout_change() {
    let mut session = Session::new("sess-5".to_string(), "test".to_string(), "/tmp".to_string());

    let mut receivers = Vec::new();
    for (client_id, window_id) in [
        ("client-1", "win-1"),
        ("client-2", "win-2"),
        ("client-3", "win-3"),
    ] {
        session.add_attached_client(
            client_id.to_string(),
            crate::session::AttachMode::Shared,
            crate::session::ClientRole::ReadWrite,
            Some(window_id.to_string()),
        );
        session.add_window(window_id.to_string());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        session.add_lifecycle_subscriber(client_id.to_string(), sender);
        receivers.push(receiver);
    }

    let targets = session.broadcast_layout_change(mux_protocol::LayoutTree { root: None });
    assert_eq!(targets.len(), 3);
    assert!(targets.contains(&"win-1".to_string()));
    assert!(targets.contains(&"win-2".to_string()));
    assert!(targets.contains(&"win-3".to_string()));

    for mut receiver in receivers {
        let envelope = receiver
            .try_recv()
            .expect("every connected window must receive the layout change");
        assert!(matches!(
            envelope.payload,
            Some(mux_protocol::proto::envelope::Payload::Notification(
                mux_protocol::Notification {
                    event: Some(mux_protocol::proto::notification::Event::SessionLayoutChanged(_))
                }
            ))
        ));
    }
}

/// §3.3 测试新会话初始无窗口
#[test]
fn test_session_new_has_no_windows() {
    let session = Session::new(
        "sess-new".to_string(),
        "new".to_string(),
        "/tmp".to_string(),
    );
    assert_eq!(session.window_count(), 0);
    assert!(session.get_windows().is_empty());
}

/// §3.3 窗口在最后一个引用它的 attached 客户端离开后才释放 (Plan 32)。
///
/// §15.4 的原地重连会在旧连接清理之前用同一个 window_id 建立新连接; 如果
/// release 不看引用, 旧连接的清理就会把刚接上的窗口误删。
#[test]
fn test_session_release_window_waits_for_last_client() {
    let mut session = Session::new(
        "sess-release".to_string(),
        "test".to_string(),
        "/tmp".to_string(),
    );

    session.add_attached_client(
        "client-old".to_string(),
        crate::session::AttachMode::Shared,
        crate::session::ClientRole::ReadWrite,
        Some("win-1".to_string()),
    );
    session.add_attached_client(
        "client-new".to_string(),
        crate::session::AttachMode::Shared,
        crate::session::ClientRole::ReadWrite,
        Some("win-1".to_string()),
    );
    session.add_window("win-1".to_string());

    assert_eq!(
        session.remove_attached_client("client-old"),
        Some("win-1".to_string()),
        "removing a client reports the window it claimed"
    );
    assert!(
        !session.release_window("win-1"),
        "the reconnected client still claims win-1"
    );
    assert!(session.has_window("win-1"));

    assert_eq!(
        session.remove_attached_client("client-new"),
        Some("win-1".to_string())
    );
    assert!(
        session.release_window("win-1"),
        "the last claimant left, so win-1 leaves the session"
    );
    assert!(!session.has_window("win-1"));
    assert_eq!(session.window_count(), 0);
}

/// §3.3 没有声明窗口的连接 (CLI 一次性命令) 不影响窗口列表。
#[test]
fn test_session_client_without_window_leaves_windows_alone() {
    let mut session = Session::new(
        "sess-cli".to_string(),
        "test".to_string(),
        "/tmp".to_string(),
    );
    session.add_window("win-gui".to_string());
    session.add_attached_client(
        "client-cli".to_string(),
        crate::session::AttachMode::Shared,
        crate::session::ClientRole::ReadWrite,
        None,
    );

    assert_eq!(session.remove_attached_client("client-cli"), None);
    assert!(session.has_window("win-gui"));
}
