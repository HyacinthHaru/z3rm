//! Binary Lifting LCA：$O(\log D)$ 最近公共祖先查询
//!
//! 每个节点存储 $\lceil \log_2(\text{depth}) \rceil$ 祖先指针。
//! ancestors[k] = 向上跳 $2^k$ 步的祖先 ID。

use std::collections::HashMap;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::version_tree::{VersionId, VersionNode};

/// 计算节点深度（到根节点的边数）
pub fn compute_depth(nodes: &HashMap<VersionId, Arc<VersionNode>>, node_id: VersionId) -> usize {
    let mut depth = 0usize;
    let mut current = node_id;

    loop {
        let n = match nodes.get(&current) {
            Some(n) => n,
            None => return depth,
        };
        match n.parent_id {
            Some(parent) => {
                current = parent;
                depth += 1;
            }
            None => return depth,
        }
    }
}

/// 构建 binary lifting 祖先跳表
///
/// 对于深度为 d 的节点，ancestors[k] = $2^k$ 步祖先。
/// 跳表大小 = $\lceil \log_2(d) \rceil + 1$
pub fn build_ancestor_table(
    node: &VersionNode,
    nodes: &HashMap<VersionId, Arc<VersionNode>>,
) -> SmallVec<[VersionId; 16]> {
    let mut table: SmallVec<[VersionId; 16]> = SmallVec::new();

    let parent = match node.parent_id {
        Some(p) => p,
        None => return table,
    };

    // ancestors[0] = 直接父节点
    table.push(parent);

    // 计算父节点的跳表
    let parent_node = match nodes.get(&parent) {
        Some(n) => n,
        None => return table,
    };
    let parent_ancestors = build_ancestor_table(parent_node, nodes);

    // ancestors[k] = parent.ancestors[k-1]
    for k in 1..16 {
        if k - 1 < parent_ancestors.len() {
            table.push(parent_ancestors[k - 1]);
        } else {
            break;
        }
    }

    table
}

/// 向上跳 $2^k$ 步，返回到达的节点 ID
fn jump(
    nodes: &HashMap<VersionId, Arc<VersionNode>>,
    node_id: VersionId,
    k: usize,
) -> Option<VersionId> {
    let node = nodes.get(&node_id)?;
    let ancestors = build_ancestor_table(node, nodes);

    if k < ancestors.len() {
        Some(ancestors[k])
    } else {
        None
    }
}

/// 求两个节点的最近公共祖先（LCA）
///
/// 时间复杂度：$O(\log D)$
///
/// 算法：
/// 1. 统一深度：深节点跳到与浅节点同深度
/// 2. 同时向上跳：从最大 $2^k$ 开始，如果跳后不同则跳，最后各跳一步即 LCA
pub fn compute_lca(
    nodes: &HashMap<VersionId, Arc<VersionNode>>,
    a: VersionId,
    b: VersionId,
) -> Option<VersionId> {
    if a == b {
        return Some(a);
    }

    let depth_a = compute_depth(nodes, a);
    let depth_b = compute_depth(nodes, b);

    // 统一深度
    let (mut shallow, mut deep, deeper_depth) = if depth_a >= depth_b {
        (b, a, depth_a)
    } else {
        (a, b, depth_b)
    };

    let diff = deeper_depth - depth_a.min(depth_b);

    // 深节点向上跳到浅节点同深度
    for k in (0..16).rev() {
        if (diff >> k) & 1 != 0 {
            if let Some(ancestor) = jump(nodes, deep, k) {
                deep = ancestor;
            }
        }
    }

    // 现在深度相同
    if deep == shallow {
        return Some(deep);
    }

    // 同时向上跳
    for k in (0..16).rev() {
        let ancestor_a = jump(nodes, deep, k);
        let ancestor_b = jump(nodes, shallow, k);
        if ancestor_a != ancestor_b {
            if let (Some(a), Some(b)) = (ancestor_a, ancestor_b) {
                deep = a;
                shallow = b;
            }
        }
    }

    // 再跳一步即到 LCA
    jump(nodes, deep, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用节点工厂
    fn make_node(id: VersionId, parent: Option<VersionId>) -> Arc<VersionNode> {
        Arc::new(VersionNode {
            version_id: id,
            path_hash: [0u8; 32],
            seq_no: id,
            timestamp_ns: 0,
            parent_id: parent,
            ancestors: SmallVec::new(),
            full_content: None,
            delta: None,
            delta_depth: 0,
            trigger: crate::version_tree::SnapshotTrigger::Write,
        })
    }

    #[test]
    fn test_lca_siblings() {
        //       1
        //      / \
        //     2   3
        let nodes: HashMap<VersionId, Arc<VersionNode>> = [
            (1, make_node(1, None)),
            (2, make_node(2, Some(1))),
            (3, make_node(3, Some(1))),
        ]
        .into();

        let lca = compute_lca(&nodes, 2, 3);
        eprintln!("LCA(2,3) = {:?}", lca);
        assert_eq!(lca, Some(1));
    }

    #[test]
    fn test_lca_same_node() {
        let nodes: HashMap<VersionId, Arc<VersionNode>> = [(1, make_node(1, None))].into();

        assert_eq!(compute_lca(&nodes, 1, 1), Some(1));
    }

    #[test]
    fn test_lca_ancestor_descendant() {
        // 1 → 2 → 3 → 4
        let nodes: HashMap<VersionId, Arc<VersionNode>> = [
            (1, make_node(1, None)),
            (2, make_node(2, Some(1))),
            (3, make_node(3, Some(2))),
            (4, make_node(4, Some(3))),
        ]
        .into();

        // LCA(1, 4) = 1
        assert_eq!(compute_lca(&nodes, 1, 4), Some(1));
        // LCA(2, 4) = 2
        assert_eq!(compute_lca(&nodes, 2, 4), Some(2));
    }

    #[test]
    fn test_depth_computation() {
        let nodes: HashMap<VersionId, Arc<VersionNode>> = [
            (1, make_node(1, None)),
            (2, make_node(2, Some(1))),
            (3, make_node(3, Some(2))),
        ]
        .into();

        assert_eq!(compute_depth(&nodes, 1), 0);
        assert_eq!(compute_depth(&nodes, 2), 1);
        assert_eq!(compute_depth(&nodes, 3), 2);
    }
}
