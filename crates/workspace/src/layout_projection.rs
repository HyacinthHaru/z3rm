// §15.1 / §16.9 布局投影模块 — client workspace 从 server 接收 LayoutTree 后渲染。
// Client 变为无状态 layout renderer，不再维护本地布局树。

use gpui::{Axis, Bounds, Pixels, point, size};
use std::collections::HashMap;

// ============================================================================
// §15.1 LayoutTree — 从 mux_protocol 导入的布局树，client 端投影使用
// ============================================================================

/// §15.1 布局节点枚举。与 mux_server 的 LayoutNode 对应。
#[derive(Clone, Debug)]
pub enum LayoutNode {
    /// 叶子节点: 单个 pane
    Pane {
        /// 节点 ID
        id: String,
        /// 关联的 pane ID
        pane_id: String,
    },
    /// 分割节点: 子节点 + 方向 + 比例
    Split {
        /// 节点 ID
        id: String,
        /// 分割方向
        direction: SplitDirection,
        /// 子节点列表
        children: Vec<LayoutNode>,
        /// 尺寸比例 (每个 child 一个 float, 总和为 1.0)
        ratios: Vec<f32>,
    },
}

/// §15.1 分割方向
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SplitDirection {
    /// 左右分割 (水平)
    LeftRight,
    /// 上下分割 (垂直)
    TopBottom,
}

impl SplitDirection {
    /// 转换为 GPUI Axis
    pub fn to_axis(&self) -> Axis {
        match self {
            SplitDirection::LeftRight => Axis::Horizontal,
            SplitDirection::TopBottom => Axis::Vertical,
        }
    }
}

/// §15.1 布局树容器
#[derive(Clone, Debug)]
pub struct LayoutTree {
    /// 根节点
    pub root: LayoutNode,
}

impl LayoutTree {
    /// 从 mux_protocol 的 proto LayoutTree 转换
    pub fn from_proto(tree: &mux_protocol::LayoutTree) -> Self {
        let root_node = tree.root.as_ref().expect("LayoutTree root must be present");
        Self {
            root: Self::node_from_proto(root_node),
        }
    }

    fn node_from_proto(node: &mux_protocol::LayoutNode) -> LayoutNode {
        match &node.node {
            Some(mux_protocol::layout_node::Node::Pane(leaf)) => LayoutNode::Pane {
                id: node.id.clone(),
                pane_id: leaf.pane_id.clone(),
            },
            Some(mux_protocol::layout_node::Node::Split(split)) => LayoutNode::Split {
                id: node.id.clone(),
                direction: match split.direction {
                    1 => SplitDirection::LeftRight,
                    2 => SplitDirection::TopBottom,
                    _ => SplitDirection::LeftRight,
                },
                children: split
                    .children
                    .iter()
                    .map(|c| Self::node_from_proto(c))
                    .collect(),
                ratios: split.ratios.clone(),
            },
            None => LayoutNode::Pane {
                id: node.id.clone(),
                pane_id: String::new(),
            },
        }
    }

    /// 收集所有 pane IDs
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
}

// ============================================================================
// §15.1 布局投影 — 将 LayoutTree 映射为 GPUI 元素位置
// ============================================================================

/// §15.1 布局投影结果: 包含每个 pane 的 Bounds
#[derive(Debug)]
pub struct LayoutProjection {
    /// pane_id → Bounds
    pub pane_bounds: HashMap<String, Bounds<Pixels>>,
    /// 根节点 bounds
    pub root_bounds: Bounds<Pixels>,
}

/// §15.1 投影配置
#[derive(Clone, Copy, Debug)]
pub struct ProjectionConfig {
    /// 可用区域
    pub available_bounds: Bounds<Pixels>,
    /// 分割条宽度
    pub splitter_width: Pixels,
}

impl LayoutTree {
    /// §15.1 将布局树投影到可用区域, 返回每个 pane 的 Bounds
    pub fn project(&self, config: ProjectionConfig) -> LayoutProjection {
        let mut pane_bounds = HashMap::new();
        self.project_node(
            &self.root,
            config.available_bounds,
            config.splitter_width,
            &mut pane_bounds,
        );
        LayoutProjection {
            pane_bounds,
            root_bounds: config.available_bounds,
        }
    }

    fn project_node(
        &self,
        node: &LayoutNode,
        bounds: Bounds<Pixels>,
        splitter_width: Pixels,
        pane_bounds: &mut HashMap<String, Bounds<Pixels>>,
    ) {
        match node {
            LayoutNode::Pane { pane_id, .. } => {
                pane_bounds.insert(pane_id.clone(), bounds);
            }
            LayoutNode::Split {
                direction,
                children,
                ratios,
                ..
            } => {
                let axis = direction.to_axis();
                let total_ratio: f32 = ratios.iter().sum();

                // §15.1 计算每个子节点的 bounds (考虑分割条宽度)
                let num_children = children.len() as f32;
                let splitter_total = splitter_width * (num_children - 1.0).max(0.0);
                let usable_size = if axis == Axis::Horizontal {
                    bounds.size.width - splitter_total
                } else {
                    bounds.size.height - splitter_total
                };

                let mut current_offset = if axis == Axis::Horizontal {
                    bounds.origin.x
                } else {
                    bounds.origin.y
                };

                for (i, child) in children.iter().enumerate() {
                    let child_size = (ratios[i] / total_ratio) * usable_size;
                    let child_bounds = if axis == Axis::Horizontal {
                        Bounds {
                            origin: point(current_offset, bounds.origin.y),
                            size: size(child_size, bounds.size.height),
                        }
                    } else {
                        Bounds {
                            origin: point(bounds.origin.x, current_offset),
                            size: size(bounds.size.width, child_size),
                        }
                    };

                    self.project_node(
                        child,
                        child_bounds,
                        splitter_width,
                        pane_bounds,
                    );

                    // §15.1 加上分割条宽度偏移
                    current_offset += child_size + splitter_width;
                }
            }
        }
    }
}

// ============================================================================
// §15.1 布局渲染 — 将 LayoutTree 渲染为 GPUI 元素
// ============================================================================

/// §15.1 布局渲染器:将 LayoutTree 与 pane 实体映射,计算 GPUI 元素位置。
/// `render_layout` 自由函数是 §16.9 的实际入口。
pub struct LayoutRenderer;

impl LayoutRenderer {
    /// §16.9 投影布局树到可用区域, 返回每个 pane 的 Bounds
    pub fn project_layout(
        layout: &LayoutTree,
        available: Bounds<Pixels>,
        splitter_width: Pixels,
    ) -> LayoutProjection {
        layout.project(ProjectionConfig {
            available_bounds: available,
            splitter_width,
        })
    }
}

/// §16.9 布局渲染入口: 将 LayoutTree 投影为 pane bounds 列表。
///
/// 接收服务端布局树和 pane 实体映射, 根据树中 ratio 计算每个 pane
/// 在给定区域内的 Bounds。返回扁平的 (pane_id, Bounds) 列表供 GPUI 渲染。
///
/// 替代 PaneGroup 的 flexbox 布局计算 (spec §16.9)。
pub fn render_layout(
    tree: &LayoutTree,
    bounds: Bounds<Pixels>,
) -> Vec<(String, Bounds<Pixels>)> {
    let splitter_width = gpui::px(2.0);
    let projection = tree.project(ProjectionConfig {
        available_bounds: bounds,
        splitter_width,
    });
    projection.pane_bounds.into_iter().collect()
}

// ============================================================================
// §15.1 交互转发 — 将用户操作作为 RPC 转发到 server
// ============================================================================

/// §16.9 布局调整请求 — 由 client 发起, 转发到 server
#[derive(Debug, Clone)]
pub enum AdjustLayoutRequest {
    /// 分割 pane
    Split {
        /// 被分割的 pane ID
        pane_id: String,
        /// 分割方向
        direction: SplitDirection,
    },
    /// 关闭 pane
    Close {
        /// 被关闭的 pane ID
        pane_id: String,
    },
    /// 调整 pane 大小
    Resize {
        /// 被调整的 pane ID
        pane_id: String,
        /// 调整方向
        direction: SplitDirection,
        /// 调整量 (-1.0 ~ 1.0)
        delta: f32,
    },
    /// 聚焦 pane
    Focus {
        /// 被聚焦的 pane ID
        pane_id: String,
    },
}

// ============================================================================
// §15.1 Tabbar 样式枚举
// ============================================================================

/// §16.9 Tabbar 样式: top (顶部横排) 或 stacked (左侧堆叠)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabBarStyle {
    /// 顶部横排 (默认)
    #[default]
    Top,
    /// 左侧堆叠
    Stacked,
}

impl TabBarStyle {
    /// 是否为顶部横排
    pub fn is_top(&self) -> bool {
        *self == TabBarStyle::Top
    }

    /// 是否为左侧堆叠
    pub fn is_stacked(&self) -> bool {
        *self == TabBarStyle::Stacked
    }
}

impl std::fmt::Display for TabBarStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TabBarStyle::Top => write!(f, "top"),
            TabBarStyle::Stacked => write!(f, "stacked"),
        }
    }
}

impl std::str::FromStr for TabBarStyle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "top" => Ok(TabBarStyle::Top),
            "stacked" => Ok(TabBarStyle::Stacked),
            _ => Err(format!("unknown tabbar style: {}", s)),
        }
    }
}

impl serde::Serialize for TabBarStyle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for TabBarStyle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// §15.1 从 mux_protocol 类型转换
// ============================================================================

impl TryFrom<mux_protocol::LayoutTree> for LayoutTree {
    type Error = anyhow::Error;

    fn try_from(proto: mux_protocol::LayoutTree) -> Result<Self, Self::Error> {
        Ok(Self::from_proto(&proto))
    }
}

impl TryFrom<mux_protocol::LayoutNode> for LayoutNode {
    type Error = anyhow::Error;

    fn try_from(node: mux_protocol::LayoutNode) -> Result<Self, Self::Error> {
        Ok(LayoutTree::node_from_proto(&node))
    }
}

// #[cfg(test)]
// mod tests;

