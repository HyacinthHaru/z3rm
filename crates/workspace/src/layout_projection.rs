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
        // §15.4 the server serializes an empty session's root as None; treat
        // that as an empty single-pane tree rather than panicking (AGENTS.md).
        let Some(root_node) = tree.root.as_ref() else {
            return Self {
                root: LayoutNode::Pane {
                    id: String::new(),
                    pane_id: String::new(),
                },
            };
        };
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
    /// §16.9 The direction of the split a pane sits directly inside.
    ///
    /// A tab dropped into another pane's tab strip carries no direction of its
    /// own — the server has no stacking, so the nearest thing it can hold is
    /// "beside the target, along the axis already there".
    pub fn parent_direction(&self, pane_id: &str) -> Option<SplitDirection> {
        fn visit(node: &LayoutNode, pane_id: &str) -> Option<SplitDirection> {
            let LayoutNode::Split {
                direction,
                children,
                ..
            } = node
            else {
                return None;
            };
            if children.iter().any(
                |child| matches!(child, LayoutNode::Pane { pane_id: id, .. } if id == pane_id),
            ) {
                return Some(*direction);
            }
            children.iter().find_map(|child| visit(child, pane_id))
        }
        visit(&self.root, pane_id)
    }

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
                // §15.4 guard against mismatched/empty ratios from a malformed
                // or forward-compatible layout payload: fall back to equal
                // weights instead of indexing out of bounds or dividing by 0.
                let equal_ratios = vec![1.0_f32; children.len()];
                let safe_ratios = if ratios.len() == children.len() && !ratios.is_empty() {
                    ratios.as_slice()
                } else {
                    equal_ratios.as_slice()
                };
                let total_ratio: f32 = safe_ratios.iter().sum();
                let total_ratio = if total_ratio <= 0.0 {
                    children.len() as f32
                } else {
                    total_ratio
                };

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
                    let child_size = (safe_ratios[i] / total_ratio) * usable_size;
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

                    self.project_node(child, child_bounds, splitter_width, pane_bounds);

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
pub fn render_layout(tree: &LayoutTree, bounds: Bounds<Pixels>) -> Vec<(String, Bounds<Pixels>)> {
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

// ============================================================================
// §15.4 / §15.12 Client-side projection of one authoritative attach snapshot
// ============================================================================

/// §15.4 / §15.12 Client-side projection of one authoritative attach snapshot.
#[derive(Clone, Default)]
pub struct MuxSnapshot {
    /// The layout tree the server handed back, if this session has one.
    pub layout: Option<LayoutTree>,
    /// Which pane the server considers focused.
    pub focused_pane: Option<String>,
    /// Per-pane zoom state, seeded without a second round trip.
    pub zoomed: HashMap<String, bool>,
    /// Every pane in the snapshot, in layout order when there is a layout.
    pub pane_ids: Vec<String>,
    /// Kept verbatim because the sidebar needs the tab dimension, which the
    /// layout tree does not model.
    pub session: Option<mux_protocol::SessionSnapshot>,
}

impl MuxSnapshot {
    /// Project the snapshot carried by an attach response.
    pub fn from_attach(response: &mux_protocol::AttachResponse) -> Self {
        let Some(snapshot) = response.snapshot.as_ref() else {
            return Self::default();
        };
        let layout = snapshot.layout.as_ref().map(LayoutTree::from_proto);
        let pane_ids = match &layout {
            Some(layout) => layout.pane_ids(),
            None => snapshot
                .tabs
                .iter()
                .flat_map(|tab| tab.panes.iter().map(|pane| pane.id.clone()))
                .collect(),
        };
        Self {
            layout,
            focused_pane: (!snapshot.focused_pane_id.is_empty())
                .then(|| snapshot.focused_pane_id.clone()),
            zoomed: snapshot
                .tabs
                .iter()
                .flat_map(|tab| tab.panes.iter().map(|pane| (pane.id.clone(), pane.zoomed)))
                .collect(),
            pane_ids,
            session: Some(snapshot.clone()),
        }
    }
}

/// §15.4 / §15.12 Project the authoritative server layout into a workspace:
/// one GPUI pane per server pane.
///
/// Both clients project the same snapshot the same way; they differ only in
/// what a pane view is, so that is the one thing they pass in. `build_pane_item`
/// is handed the pane id the server minted and returns the item to add.
///
/// Must run inside the `cx.new(|cx| Workspace::new(..))` closure — items added
/// after the workspace is constructed never reach the render tree.
pub fn install_snapshot_panes(
    workspace: &mut crate::Workspace,
    snapshot: &MuxSnapshot,
    mut build_pane_item: impl FnMut(
        &mut crate::Workspace,
        String,
        &mut gpui::Window,
        &mut gpui::Context<crate::Workspace>,
    ) -> Box<dyn crate::ItemHandle>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::Workspace>,
) {
    match &snapshot.layout {
        Some(layout) => {
            // The zoom pass below needs to know which pane holds which id, and
            // the layout walk is the only place that pairing exists.
            let mut panes_by_id: Vec<(String, gpui::Entity<crate::Pane>)> = Vec::new();
            workspace.apply_initial_layout(
                layout,
                snapshot.focused_pane.as_deref(),
                |workspace, window, cx| workspace.add_pane_for_layout(window, cx),
                |workspace, pane, pane_id, window, cx| {
                    panes_by_id.push((pane_id.clone(), pane.clone()));
                    pane.update(cx, |pane, _| {
                        pane.set_should_display_welcome_page(false);
                    });
                    let item = build_pane_item(workspace, pane_id, window, cx);
                    workspace.add_item(pane.clone(), item, None, true, true, window, cx);
                },
                window,
                cx,
            );

            // §15.4 seed zoom from PaneInfo without re-RPC.
            for (pane_id, pane) in panes_by_id {
                if snapshot.zoomed.get(&pane_id) == Some(&true) {
                    workspace.set_pane_zoomed(pane, true, window, cx);
                }
            }
        }
        None => {
            // No layout tree: single default pane with all views as tabs.
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, _| {
                pane.set_should_display_welcome_page(false);
            });
            let pane_ids = if snapshot.pane_ids.is_empty() {
                vec!["default".to_string()]
            } else {
                snapshot.pane_ids.clone()
            };
            for (index, pane_id) in pane_ids.into_iter().enumerate() {
                let item = build_pane_item(workspace, pane_id, window, cx);
                workspace.add_item(pane.clone(), item, None, index == 0, true, window, cx);
            }
        }
    }
}

#[cfg(test)]
mod layout_tree_tests {
    use super::{LayoutNode, LayoutTree, SplitDirection};

    fn leaf(id: &str, pane_id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: id.to_string(),
            pane_id: pane_id.to_string(),
        }
    }

    /// A tab dropped into another pane's strip carries no direction, so the
    /// axis already around the target is the only honest answer.
    #[test]
    fn parent_direction_reports_the_split_a_pane_sits_in() {
        let tree = LayoutTree {
            root: LayoutNode::Split {
                id: "root".to_string(),
                direction: SplitDirection::LeftRight,
                children: vec![
                    leaf("a", "pane-1"),
                    LayoutNode::Split {
                        id: "inner".to_string(),
                        direction: SplitDirection::TopBottom,
                        children: vec![leaf("b", "pane-2"), leaf("c", "pane-3")],
                        ratios: vec![0.5, 0.5],
                    },
                ],
                ratios: vec![0.5, 0.5],
            },
        };

        assert_eq!(
            tree.parent_direction("pane-1"),
            Some(SplitDirection::LeftRight)
        );
        assert_eq!(
            tree.parent_direction("pane-3"),
            Some(SplitDirection::TopBottom),
            "the nearest split wins, not the root"
        );
        assert_eq!(tree.parent_direction("pane-9"), None);
    }

    /// A lone pane has no split around it, so there is no axis to drop along.
    #[test]
    fn a_root_pane_has_no_parent_direction() {
        let tree = LayoutTree {
            root: leaf("root", "pane-1"),
        };
        assert_eq!(tree.parent_direction("pane-1"), None);
    }
}

#[cfg(test)]
mod mux_snapshot_tests {
    use super::MuxSnapshot;

    fn pane(id: &str, zoomed: bool) -> mux_protocol::PaneInfo {
        mux_protocol::PaneInfo {
            id: id.to_string(),
            zoomed,
            ..Default::default()
        }
    }

    fn snapshot(tabs: Vec<mux_protocol::TabInfo>) -> mux_protocol::AttachResponse {
        mux_protocol::AttachResponse {
            snapshot: Some(mux_protocol::SessionSnapshot {
                tabs,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_response_without_a_snapshot_projects_to_nothing() {
        let projected = MuxSnapshot::from_attach(&mux_protocol::AttachResponse::default());
        assert!(projected.pane_ids.is_empty());
        assert!(projected.layout.is_none());
        assert!(projected.session.is_none());
    }

    #[test]
    fn without_a_layout_the_panes_come_from_the_tabs_in_order() {
        // The tab dimension is the only place pane ids appear when the server
        // has not built a layout tree yet.
        let response = snapshot(vec![
            mux_protocol::TabInfo {
                id: "tab-0".into(),
                panes: vec![pane("a", false), pane("b", true)],
                ..Default::default()
            },
            mux_protocol::TabInfo {
                id: "tab-1".into(),
                panes: vec![pane("c", false)],
                ..Default::default()
            },
        ]);

        let projected = MuxSnapshot::from_attach(&response);

        assert_eq!(projected.pane_ids, vec!["a", "b", "c"]);
        assert_eq!(projected.zoomed.get("b"), Some(&true));
        assert_eq!(projected.zoomed.get("a"), Some(&false));
    }

    #[test]
    fn with_a_layout_the_panes_come_from_the_tree() {
        // Panes the layout does not place are not rendered, so a tab-only pane
        // must not reach `pane_ids`.
        let mut response = snapshot(vec![mux_protocol::TabInfo {
            id: "tab-0".into(),
            panes: vec![pane("placed", false), pane("unplaced", false)],
            ..Default::default()
        }]);
        response.snapshot.as_mut().unwrap().layout = Some(mux_protocol::LayoutTree {
            root: Some(mux_protocol::LayoutNode {
                id: "node-0".into(),
                node: Some(mux_protocol::layout_node::Node::Pane(
                    mux_protocol::PaneLeaf {
                        pane_id: "placed".into(),
                    },
                )),
            }),
            ..Default::default()
        });

        let projected = MuxSnapshot::from_attach(&response);

        assert_eq!(projected.pane_ids, vec!["placed"]);
        assert!(projected.layout.is_some());
    }

    #[test]
    fn an_empty_focused_pane_id_is_no_focus() {
        let mut response = snapshot(vec![]);
        response.snapshot.as_mut().unwrap().focused_pane_id = String::new();
        assert!(MuxSnapshot::from_attach(&response).focused_pane.is_none());

        response.snapshot.as_mut().unwrap().focused_pane_id = "a".into();
        assert_eq!(
            MuxSnapshot::from_attach(&response).focused_pane.as_deref(),
            Some("a")
        );
    }
}
