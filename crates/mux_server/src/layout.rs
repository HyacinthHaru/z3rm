// §3.10 Layout 模块 — 从 workspace pane_group 迁移的 split tree。
// 管理 pane 分割、合并、尺寸比例。

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;

/// 布局树 (§3.10 LayoutTree)
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutTree {
    /// 根节点
    pub root: LayoutNode,
}

/// 布局节点 (§3.10 LayoutNode)
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutNode {
    /// 叶子节点: 单个 pane (§3.10 PaneLeaf)
    Pane {
        /// 节点 ID
        id: String,
        /// 关联的 pane ID
        pane_id: String,
    },
    /// 分割节点: 子节点 + 方向 + 比例 (§3.10 SplitNode)
    Split {
        /// 节点 ID
        id: String,
        /// 分割方向: 左右 / 上下
        direction: SplitDirection,
        /// 子节点列表
        children: Vec<LayoutNode>,
        /// 尺寸比例 (每个 child 一个 float, 总和为 1.0)
        ratios: Vec<f32>,
    },
}

/// 分割方向 (§3.10 SplitNode.SplitDirection)
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum SplitDirection {
    /// 左右分割 (水平分割, 子节点左右排列)
    LeftRight,
    /// 上下分割 (垂直分割, 子节点上下排列)
    TopBottom,
}

/// §3.7 类型化布局持久化编码: 前序节点表。
///
/// 持久化格式是扁平的节点数组, split 用数组索引引用子节点, 不嵌套 JSON。
/// 扁平编码让反序列化不受 serde 递归深度限制; 树深与结构不变量在
/// `LayoutTree::from_persisted` 重建时逐节点校验, 而不是在解析时隐式信任。
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct PersistedLayoutTree {
    /// 前序节点表; 索引 0 必须是根。
    pub nodes: Vec<PersistedLayoutNode>,
}

/// §3.7 持久化布局节点: 与 `LayoutNode` 同构, 但 split 的 children 是
/// `PersistedLayoutTree.nodes` 中的索引。前序不变量: 子索引严格大于父索引。
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistedLayoutNode {
    /// 叶子节点: 单个 pane
    Pane {
        /// 节点 ID
        id: String,
        /// 关联的 pane ID
        pane_id: String,
    },
    /// 分割节点
    Split {
        /// 节点 ID
        id: String,
        /// 分割方向
        direction: SplitDirection,
        /// 子节点索引 (前序, 严格递增)
        children: Vec<usize>,
        /// 尺寸比例 (每个 child 一个 float)
        ratios: Vec<f32>,
    },
}

/// Bound internal recursion for persistence and tree operations.
pub const MAX_INTERNAL_LAYOUT_DEPTH: usize = 128;
/// Each direction change contributes a `LayoutNode` + `SplitNode` pair on the wire.
pub use mux_protocol::MAX_LAYOUT_WIRE_DEPTH as MAX_WIRE_LAYOUT_DEPTH;
/// §3.7 持久化布局节点数上限: 防御损坏行诱导的超大分配。
pub const MAX_PERSISTED_LAYOUT_NODES: usize = 4096;

impl LayoutTree {
    /// 空布局树
    pub fn empty() -> Self {
        Self {
            root: LayoutNode::Pane {
                id: String::new(),
                pane_id: String::new(),
            },
        }
    }

    /// 从单个 pane 创建 (§3.10)
    pub fn with_pane(id: String, pane_id: String) -> Self {
        Self {
            root: LayoutNode::Pane { id, pane_id },
        }
    }

    /// 根节点是否是空 placeholder (session 创建时初始状态)
    pub fn is_empty_root(&self) -> bool {
        matches!(
            self.root,
            LayoutNode::Pane {
                id: ref i,
                pane_id: ref p,
            } if i.is_empty() && p.is_empty()
        )
    }

    /// 分割已有 pane (§3.10 SplitPaneRequest)
    pub fn split(
        &mut self,
        pane_id: &str,
        new_pane_id: String,
        direction: SplitDirection,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!new_pane_id.is_empty(), "new pane id must not be empty");
        anyhow::ensure!(
            self.root.find_pane(&new_pane_id).is_none(),
            "pane already exists in layout: {new_pane_id}"
        );

        let mut candidate = self.root.clone();
        let old_node_id = Self::find_pane_node_id(&candidate, pane_id)?;
        Self::split_node(&mut candidate, &old_node_id, new_pane_id, direction)?;
        Self::validate_structure(&candidate)?;
        self.root = candidate;
        Ok(())
    }

    fn find_pane_node_id(node: &LayoutNode, pane_id: &str) -> anyhow::Result<String> {
        match node {
            LayoutNode::Pane { id, pane_id: pid } if pid == pane_id => Ok(id.clone()),
            LayoutNode::Pane { .. } => Err(anyhow::anyhow!("pane not found: {}", pane_id)),
            LayoutNode::Split { children, .. } => {
                for child in children {
                    if let Ok(node_id) = Self::find_pane_node_id(child, pane_id) {
                        return Ok(node_id);
                    }
                }
                Err(anyhow::anyhow!("pane not found: {}", pane_id))
            }
        }
    }

    fn split_node(
        node: &mut LayoutNode,
        old_node_id: &str,
        new_pane_id: String,
        direction: SplitDirection,
    ) -> anyhow::Result<()> {
        match node {
            LayoutNode::Pane { id, pane_id, .. } if id == old_node_id => {
                *node = LayoutNode::Split {
                    id: id.clone(),
                    direction,
                    children: vec![
                        LayoutNode::Pane {
                            id: format!("{}-left", id),
                            pane_id: pane_id.clone(),
                        },
                        LayoutNode::Pane {
                            id: format!("{}-right", id),
                            pane_id: new_pane_id,
                        },
                    ],
                    ratios: vec![0.5, 0.5],
                };
                Ok(())
            }
            LayoutNode::Split { children, .. } => {
                let child = children
                    .iter_mut()
                    .find(|child| Self::contains_node_id(child, old_node_id))
                    .ok_or_else(|| anyhow::anyhow!("node not found: {}", old_node_id))?;
                Self::split_node(child, old_node_id, new_pane_id, direction)
            }
            LayoutNode::Pane { .. } => Err(anyhow::anyhow!("node not found: {}", old_node_id)),
        }
    }

    fn contains_node_id(node: &LayoutNode, node_id: &str) -> bool {
        match node {
            LayoutNode::Pane { id, .. } => id == node_id,
            LayoutNode::Split { id, children, .. } => {
                id == node_id
                    || children
                        .iter()
                        .any(|child| Self::contains_node_id(child, node_id))
            }
        }
    }

    fn validate_structure(root: &LayoutNode) -> anyhow::Result<()> {
        fn visit(
            node: &LayoutNode,
            internal_depth: usize,
            wire_depth: usize,
            parent_direction: Option<SplitDirection>,
            node_ids: &mut HashSet<String>,
            pane_ids: &mut HashSet<String>,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(
                internal_depth <= MAX_INTERNAL_LAYOUT_DEPTH,
                "layout depth exceeds internal maximum of {MAX_INTERNAL_LAYOUT_DEPTH}"
            );
            match node {
                LayoutNode::Pane { id, pane_id } => {
                    anyhow::ensure!(!id.is_empty(), "layout node id must not be empty");
                    anyhow::ensure!(!pane_id.is_empty(), "layout pane id must not be empty");
                    anyhow::ensure!(
                        node_ids.insert(id.clone()),
                        "duplicate layout node id: {id}"
                    );
                    anyhow::ensure!(
                        pane_ids.insert(pane_id.clone()),
                        "duplicate pane id in layout: {pane_id}"
                    );
                }
                LayoutNode::Split {
                    id,
                    direction,
                    children,
                    ratios,
                } => {
                    let wire_depth = if parent_direction == Some(*direction) {
                        wire_depth
                    } else {
                        wire_depth + 1
                    };
                    anyhow::ensure!(
                        wire_depth <= MAX_WIRE_LAYOUT_DEPTH,
                        "layout wire depth exceeds maximum of {MAX_WIRE_LAYOUT_DEPTH}"
                    );
                    anyhow::ensure!(!id.is_empty(), "layout node id must not be empty");
                    anyhow::ensure!(
                        node_ids.insert(id.clone()),
                        "duplicate layout node id: {id}"
                    );
                    anyhow::ensure!(
                        children.len() >= 2,
                        "layout split needs at least two children"
                    );
                    anyhow::ensure!(
                        children.len() == ratios.len(),
                        "layout split has mismatched children and ratios"
                    );
                    anyhow::ensure!(
                        ratios
                            .iter()
                            .all(|ratio| ratio.is_finite() && *ratio > 0.0),
                        "layout ratios must be finite and positive"
                    );
                    let ratio_sum: f32 = ratios.iter().sum();
                    anyhow::ensure!(
                        ratio_sum.is_finite() && ratio_sum > 0.0,
                        "layout split ratio sum must be finite and positive"
                    );
                    for child in children {
                        visit(
                            child,
                            internal_depth + 1,
                            wire_depth,
                            Some(*direction),
                            node_ids,
                            pane_ids,
                        )?;
                    }
                }
            }
            Ok(())
        }

        visit(root, 0, 0, None, &mut HashSet::new(), &mut HashSet::new())
    }

    /// §3.10 从布局树移除一个 pane。
    ///
    /// 不变量: 移除后布局树必须仍然持有至少一个 pane, 绝不留下空根
    /// (`Pane { id:"", pane_id:"" }` 哨兵)。因此:
    /// - 根节点为 `Pane` 且匹配 `pane_id` → 这是唯一的 pane, 移除会清空
    ///   布局 → 返回 `Err` (不修改原树)。
    /// - 找不到 `pane_id` → 返回 `Err` (不修改原树)。
    /// - 其余情形正常移除并扁平化, 失败路径原树完整恢复。
    pub fn remove_pane(&mut self, pane_id: &str) -> anyhow::Result<()> {
        if let LayoutNode::Pane { pane_id: pid, .. } = &self.root
            && pid == pane_id
        {
            anyhow::bail!("cannot remove sole root pane: layout would be empty");
        }

        let mut candidate = self.root.clone();
        anyhow::ensure!(
            Self::remove_from_node(&mut candidate, pane_id)?,
            "pane not found in layout: {pane_id}"
        );
        Self::validate_structure(&candidate)?;
        self.root = candidate;
        Ok(())
    }

    fn remove_from_node(node: &mut LayoutNode, pane_id: &str) -> anyhow::Result<bool> {
        let LayoutNode::Split {
            id,
            children,
            ratios,
            ..
        } = node
        else {
            return Ok(false);
        };
        let collapsed_id = id.clone();

        let direct_child_index = children.iter().position(|child| {
            matches!(child, LayoutNode::Pane { pane_id: child_pane_id, .. } if child_pane_id == pane_id)
        });
        let removed = if let Some(index) = direct_child_index {
            anyhow::ensure!(
                children.len() == ratios.len(),
                "layout split has mismatched children and ratios"
            );
            let removed_ratio = ratios[index];
            children.remove(index);
            ratios.remove(index);
            if children.len() > 1 {
                let recipient = index.checked_sub(1).unwrap_or(0);
                ratios[recipient] += removed_ratio;
            }
            true
        } else {
            let child = children
                .iter_mut()
                .find(|child| Self::contains_pane(child, pane_id));
            match child {
                Some(child) => Self::remove_from_node(child, pane_id)?,
                None => false,
            }
        };

        if !removed {
            return Ok(false);
        }
        if children.len() == 1 {
            let mut survivor = children.remove(0);
            match &mut survivor {
                LayoutNode::Pane { id, .. } | LayoutNode::Split { id, .. } => {
                    *id = collapsed_id;
                }
            }
            *node = survivor;
        }
        Ok(true)
    }

    pub fn resize_pane(
        &mut self,
        pane_id: &str,
        direction: SplitDirection,
        delta: f32,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(delta.is_finite(), "layout resize delta must be finite");
        let mut candidate = self.root.clone();
        anyhow::ensure!(
            Self::resize_in_node(&mut candidate, pane_id, direction, delta)?,
            "pane has no matching layout split: {pane_id}"
        );
        Self::validate_structure(&candidate)?;
        self.root = candidate;
        Ok(())
    }

    fn resize_in_node(
        node: &mut LayoutNode,
        pane_id: &str,
        direction: SplitDirection,
        delta: f32,
    ) -> anyhow::Result<bool> {
        match node {
            LayoutNode::Pane { .. } => Ok(false),
            LayoutNode::Split {
                direction: split_direction,
                children,
                ratios,
                ..
            } => {
                let Some(index) = children
                    .iter()
                    .position(|child| Self::contains_pane(child, pane_id))
                else {
                    return Ok(false);
                };

                if Self::resize_in_node(&mut children[index], pane_id, direction, delta)? {
                    return Ok(true);
                }
                if *split_direction != direction {
                    return Ok(false);
                }
                anyhow::ensure!(
                    children.len() == ratios.len(),
                    "layout split has mismatched children and ratios"
                );
                let neighbor = if index > 0 {
                    index - 1
                } else if index + 1 < ratios.len() {
                    index + 1
                } else {
                    anyhow::bail!("layout split has no resize neighbor");
                };
                let pair_total = ratios[index] + ratios[neighbor];
                anyhow::ensure!(
                    pair_total.is_finite() && pair_total > 0.0,
                    "layout resize pair has invalid ratios"
                );
                let minimum = (pair_total / 2.0).min(0.05);
                let resized = (ratios[index] + delta).clamp(minimum, pair_total - minimum);
                ratios[index] = resized;
                ratios[neighbor] = pair_total - resized;
                Ok(true)
            }
        }
    }

    /// 检查节点是否包含指定 pane
    fn contains_pane(node: &LayoutNode, pane_id: &str) -> bool {
        match node {
            LayoutNode::Pane { pane_id: pid, .. } => pid == pane_id,
            LayoutNode::Split { children, .. } => {
                children.iter().any(|c| Self::contains_pane(c, pane_id))
            }
        }
    }

    /// §3.10 获取所有 pane IDs
    pub fn pane_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        self.collect_pane_ids(&self.root, &mut ids);
        ids
    }

    fn collect_pane_ids(&self, node: &LayoutNode, ids: &mut Vec<String>) {
        match node {
            LayoutNode::Pane { pane_id, .. } => ids.push(pane_id.clone()),
            LayoutNode::Split { children, .. } => {
                for child in children {
                    self.collect_pane_ids(child, ids);
                }
            }
        }
    }

    /// §3.7 编码为类型化持久化格式 (前序节点表)。
    ///
    /// 与旧的 tmux 风格字符串不同, 该编码保留节点 ID 与精确的 f32 比例,
    /// 使恢复能重建出与保存时完全一致的树。
    pub fn to_persisted(&self) -> anyhow::Result<PersistedLayoutTree> {
        let mut nodes = Vec::new();
        Self::append_persisted(&self.root, &mut nodes)?;
        Ok(PersistedLayoutTree { nodes })
    }

    fn append_persisted(
        node: &LayoutNode,
        nodes: &mut Vec<PersistedLayoutNode>,
    ) -> anyhow::Result<usize> {
        let index = nodes.len();
        match node {
            LayoutNode::Pane { id, pane_id } => {
                nodes.push(PersistedLayoutNode::Pane {
                    id: id.clone(),
                    pane_id: pane_id.clone(),
                });
            }
            LayoutNode::Split {
                id,
                direction,
                children,
                ratios,
            } => {
                // 先占位, 访问完子树后再回填子索引: 前序编码要求子索引大于父索引。
                nodes.push(PersistedLayoutNode::Split {
                    id: id.clone(),
                    direction: *direction,
                    children: Vec::new(),
                    ratios: ratios.clone(),
                });
                let mut child_indices = Vec::with_capacity(children.len());
                for child in children {
                    child_indices.push(Self::append_persisted(child, nodes)?);
                }
                if let PersistedLayoutNode::Split {
                    children: slots, ..
                } = &mut nodes[index]
                {
                    *slots = child_indices;
                }
            }
        }
        Ok(index)
    }

    /// §3.7 从类型化持久化编码重建布局树。
    ///
    /// 重建期间执行完整结构校验 (唯一节点/pane ID、有限正比例、内部/wire
    /// 深度、前序子索引边界与单树性); 任一不变量失败即返回 Err, 不产生
    /// 部分状态。空占位根 (`Pane { id: "", pane_id: "" }`) 仅作为唯一节点
    /// 时允许 round-trip。
    pub fn from_persisted(persisted: &PersistedLayoutTree) -> anyhow::Result<LayoutTree> {
        let nodes = &persisted.nodes;
        anyhow::ensure!(!nodes.is_empty(), "persisted layout has no nodes");
        anyhow::ensure!(
            nodes.len() <= MAX_PERSISTED_LAYOUT_NODES,
            "persisted layout exceeds the maximum of {MAX_PERSISTED_LAYOUT_NODES} nodes"
        );

        // 空占位根 (尚无 pane 的 session): 仅当它是唯一节点时允许 round-trip。
        if nodes.len() == 1
            && let PersistedLayoutNode::Pane { id, pane_id } = &nodes[0]
            && id.is_empty()
            && pane_id.is_empty()
        {
            return Ok(LayoutTree::empty());
        }

        let mut seen_node_ids = HashSet::with_capacity(nodes.len());
        let mut seen_pane_ids = HashSet::with_capacity(nodes.len());
        let mut referenced = vec![false; nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            match node {
                PersistedLayoutNode::Pane { id, pane_id } => {
                    anyhow::ensure!(!id.is_empty(), "persisted pane node id must not be empty");
                    anyhow::ensure!(
                        seen_node_ids.insert(id.clone()),
                        "duplicate persisted node id: {id}"
                    );
                    anyhow::ensure!(
                        !pane_id.is_empty(),
                        "persisted pane id must not be empty"
                    );
                    anyhow::ensure!(
                        seen_pane_ids.insert(pane_id.clone()),
                        "duplicate persisted pane id: {pane_id}"
                    );
                }
                PersistedLayoutNode::Split {
                    id,
                    children,
                    ratios,
                    ..
                } => {
                    anyhow::ensure!(!id.is_empty(), "persisted split node id must not be empty");
                    anyhow::ensure!(
                        seen_node_ids.insert(id.clone()),
                        "duplicate persisted node id: {id}"
                    );
                    anyhow::ensure!(
                        children.len() >= 2,
                        "persisted split needs at least two children"
                    );
                    anyhow::ensure!(
                        children.len() == ratios.len(),
                        "persisted split has mismatched children and ratios"
                    );
                    // 前序不变量: 子索引严格递增且大于父索引 —— 同时排除越界、
                    // 自引用与环。
                    let mut previous = index;
                    for child in children {
                        anyhow::ensure!(
                            *child > previous,
                            "persisted split children must be strictly increasing pre-order indices"
                        );
                        anyhow::ensure!(
                            *child < nodes.len(),
                            "persisted split child index out of bounds: {child}"
                        );
                        previous = *child;
                        referenced[*child] = true;
                    }
                    for ratio in ratios {
                        anyhow::ensure!(
                            ratio.is_finite() && *ratio > 0.0,
                            "persisted layout ratios must be finite and positive"
                        );
                    }
                    let ratio_sum: f32 = ratios.iter().sum();
                    anyhow::ensure!(
                        ratio_sum.is_finite() && ratio_sum > 0.0,
                        "persisted layout ratio sum must be finite and positive"
                    );
                }
            }
        }
        // 除根外每个节点必须恰好被一个 split 引用 (前序索引 ⇒ 单树)。
        anyhow::ensure!(
            referenced.iter().skip(1).all(|is_referenced| *is_referenced)
                && !referenced[0],
            "persisted layout nodes must form a single tree rooted at node 0"
        );

        // 深度校验 (迭代遍历, 避免把未校验的深度直接递归进调用栈)。
        {
            let mut stack = vec![(0usize, 0usize, 0usize, None)];
            let mut visited = 0usize;
            while let Some((index, internal_depth, wire_depth, parent_direction)) = stack.pop() {
                visited += 1;
                anyhow::ensure!(
                    internal_depth <= MAX_INTERNAL_LAYOUT_DEPTH,
                    "layout depth exceeds internal maximum of {MAX_INTERNAL_LAYOUT_DEPTH}"
                );
                anyhow::ensure!(
                    wire_depth <= MAX_WIRE_LAYOUT_DEPTH,
                    "layout wire depth exceeds maximum of {MAX_WIRE_LAYOUT_DEPTH}"
                );
                let PersistedLayoutNode::Split {
                    direction,
                    children,
                    ..
                } = &nodes[index]
                else {
                    continue;
                };
                let child_wire_depth = if parent_direction == Some(*direction) {
                    wire_depth
                } else {
                    wire_depth + 1
                };
                for child in children {
                    stack.push((
                        *child,
                        internal_depth + 1,
                        child_wire_depth,
                        Some(*direction),
                    ));
                }
            }
            anyhow::ensure!(
                visited == nodes.len(),
                "persisted layout contains unreachable nodes"
            );
        }

        // 重建: 子索引大于父索引, 所以倒序构造时子树必然已经就绪。
        let mut built: Vec<LayoutNode> = Vec::with_capacity(nodes.len());
        for index in (0..nodes.len()).rev() {
            let node = match &nodes[index] {
                PersistedLayoutNode::Pane { id, pane_id } => LayoutNode::Pane {
                    id: id.clone(),
                    pane_id: pane_id.clone(),
                },
                PersistedLayoutNode::Split {
                    id,
                    direction,
                    children,
                    ratios,
                } => LayoutNode::Split {
                    id: id.clone(),
                    direction: *direction,
                    children: children
                        .iter()
                        .map(|child| built[nodes.len() - 1 - *child].clone())
                        .collect(),
                    ratios: ratios.clone(),
                },
            };
            built.push(node);
        }
        Ok(LayoutTree {
            root: built
                .pop()
                .ok_or_else(|| anyhow::anyhow!("persisted layout has no root"))?,
        })
    }
}

impl Serialize for LayoutTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let persisted = self
            .to_persisted()
            .map_err(|error| serde::ser::Error::custom(error.to_string()))?;
        PersistedLayoutTree::serialize(&persisted, serializer)
    }
}

impl<'de> Deserialize<'de> for LayoutTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let persisted = PersistedLayoutTree::deserialize(deserializer)?;
        LayoutTree::from_persisted(&persisted).map_err(DeError::custom)
    }
}

impl LayoutNode {
    /// 查找 pane
    pub fn find_pane(&self, pane_id: &str) -> Option<&str> {
        match self {
            LayoutNode::Pane { pane_id: pid, .. } if pid == pane_id => Some(pid),
            LayoutNode::Pane { .. } => None,
            LayoutNode::Split { children, .. } => {
                children.iter().find_map(|c| c.find_pane(pane_id))
            }
        }
    }
}
