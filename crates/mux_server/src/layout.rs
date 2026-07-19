// §3.10 Layout 模块 — 从 workspace pane_group 迁移的 split tree。
// 管理 pane 分割、合并、尺寸比例。

use std::collections::HashMap;

/// 布局树 (§3.10 LayoutTree)
#[derive(Clone, Debug)]
pub struct LayoutTree {
    /// 根节点
    pub root: LayoutNode,
    /// 节点 ID 映射
    pub node_ids: HashMap<String, usize>,
}

/// 布局节点 (§3.10 LayoutNode)
#[derive(Clone, Debug)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitDirection {
    /// 左右分割 (水平分割, 子节点左右排列)
    LeftRight,
    /// 上下分割 (垂直分割, 子节点上下排列)
    TopBottom,
}

impl LayoutTree {
    /// 空布局树
    pub fn empty() -> Self {
        Self {
            root: LayoutNode::Pane {
                id: String::new(),
                pane_id: String::new(),
            },
            node_ids: HashMap::new(),
        }
    }

    /// 从单个 pane 创建 (§3.10)
    pub fn with_pane(id: String, pane_id: String) -> Self {
        Self {
            root: LayoutNode::Pane { id, pane_id },
            node_ids: HashMap::new(),
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
        let old_node_id = Self::find_pane_node_id(&self.root, pane_id)?;
        let old_root = std::mem::replace(&mut self.root, LayoutNode::Pane { id: String::new(), pane_id: String::new() });
        let mut root = old_root;
        Self::split_node(&mut root, &old_node_id, new_pane_id, direction)?;
        self.root = root;
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
                for child in children.iter_mut() {
                    if Self::split_node(child, old_node_id, new_pane_id.clone(), direction).is_ok() {
                        return Ok(());
                    }
                }
                Err(anyhow::anyhow!("node not found: {}", old_node_id))
            }
            LayoutNode::Pane { .. } => {
                Err(anyhow::anyhow!("node not found: {}", old_node_id))
            }
        }
    }
    pub fn remove_pane(&mut self, pane_id: &str) -> anyhow::Result<()> {
        let mut old_root = std::mem::replace(&mut self.root, LayoutNode::Pane { id: String::new(), pane_id: String::new() });
        Self::remove_from_node(&mut old_root, pane_id)?;
        self.root = old_root;
        Ok(())
    }

    fn remove_from_node(node: &mut LayoutNode, pane_id: &str) -> anyhow::Result<bool> {
        match node {
            LayoutNode::Pane { pane_id: pid, .. } if pid == pane_id => Ok(true),
            LayoutNode::Pane { .. } => Ok(false),
            LayoutNode::Split {
                children,
                ratios,
                ..
            } => {
                let mut removed = false;
                for (i, child) in children.iter_mut().enumerate() {
                    if Self::remove_from_node(child, pane_id)? {
                        removed = true;
                        children.remove(i);
                        ratios.remove(i);
                        break;
                    }
                }

                if removed {
                    // 如果只剩一个子节点, 扁平化
                    if children.len() == 1 {
                        let child = children.remove(0);
                        *node = child;
                        return Ok(true);
                    }
                    Self::normalize_ratios(ratios);
                }
                Ok(removed)
            }
        }
    }

    /// 归一化比例
    fn normalize_ratios(ratios: &mut Vec<f32>) {
        if ratios.is_empty() {
            return;
        }
        let sum: f32 = ratios.iter().sum();
        if (sum - 0.0f32).abs() < 1e-6 {
            return;
        }
        for r in ratios.iter_mut() {
            *r = *r / sum;
        }
    }

    pub fn resize_pane(
        &mut self,
        pane_id: &str,
        direction: SplitDirection,
        delta: f32,
    ) -> anyhow::Result<()> {
        let old_root = std::mem::replace(&mut self.root, LayoutNode::Pane { id: String::new(), pane_id: String::new() });
        let mut root = old_root;
        Self::resize_in_node(&mut root, pane_id, direction, delta)?;
        self.root = root;
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
                direction: dir,
                children,
                ratios,
                ..
            } => {
                if *dir != direction {
                    for child in children.iter_mut() {
                        if Self::resize_in_node(child, pane_id, direction, delta)? {
                            return Ok(true);
                        }
                    }
                    return Ok(false);
                }

                for (i, child) in children.iter().enumerate() {
                    if Self::contains_pane(child, pane_id) {
                        ratios[i] = (ratios[i] + delta).max(0.05);
                        if i > 0 {
                            ratios[i - 1] = (ratios[i - 1] - delta).max(0.05);
                        } else if i + 1 < ratios.len() {
                            ratios[i + 1] = (ratios[i + 1] - delta).max(0.05);
                        }
                        Self::normalize_ratios(ratios);
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// 检查节点是否包含指定 pane
    fn contains_pane(node: &LayoutNode, pane_id: &str) -> bool {
        match node {
            LayoutNode::Pane { pane_id: pid, .. } => pid == pane_id,
            LayoutNode::Split { children, .. } => children
                .iter()
                .any(|c| Self::contains_pane(c, pane_id)),
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

    /// §3.7 序列化布局树为 tmux 风格的校验和格式
    pub fn serialize(&self) -> anyhow::Result<String> {
        let mut buf = String::new();
        self.serialize_node(&self.root, &mut buf)?;
        // 添加校验和
        let checksum = Self::compute_checksum(&buf);
        Ok(format!("{}\n{}", buf, checksum))
    }

    fn serialize_node(&self, node: &LayoutNode, buf: &mut String) -> anyhow::Result<()> {
        match node {
            LayoutNode::Pane { id, pane_id } => {
                buf.push_str(&format!("P:{}:{}\n", id, pane_id));
            }
            LayoutNode::Split {
                id,
                direction,
                children,
                ratios,
            } => {
                let dir_str = match direction {
                    SplitDirection::LeftRight => "H",
                    SplitDirection::TopBottom => "V",
                };
                buf.push_str(&format!(
                    "S:{}:{}:{:?}\n",
                    id,
                    dir_str,
                    ratios
                ));
                for child in children {
                    self.serialize_node(child, buf)?;
                }
            }
        }
        Ok(())
    }

    fn compute_checksum(data: &str) -> u32 {
        // 简单校验和 (tmux 风格)
        let mut sum: u32 = 0;
        for byte in data.bytes() {
            sum = sum.wrapping_mul(16777619).wrapping_add(byte as u32);
        }
        sum
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
