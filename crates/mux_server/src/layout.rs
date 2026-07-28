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
        let old_root = std::mem::replace(
            &mut self.root,
            LayoutNode::Pane {
                id: String::new(),
                pane_id: String::new(),
            },
        );
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
                    if Self::split_node(child, old_node_id, new_pane_id.clone(), direction).is_ok()
                    {
                        return Ok(());
                    }
                }
                Err(anyhow::anyhow!("node not found: {}", old_node_id))
            }
            LayoutNode::Pane { .. } => Err(anyhow::anyhow!("node not found: {}", old_node_id)),
        }
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
        // 先拦截 sole-root: 根是叶子且正是要删的 pane。此时没有兄弟可塌缩,
        // 删除会留下空布局树 → 拒绝。原树未被触碰, 不变量保持。
        if let LayoutNode::Pane { pane_id: pid, .. } = &self.root {
            if pid == pane_id {
                anyhow::bail!("cannot remove sole root pane: layout would be empty");
            }
        }

        // 取出原树; 失败路径 (`?` 提前返回) 不会写回 self.root, 但我们已经在
        // 上层替换成了哨兵, 必须在错误路径恢复。用显式作用域 + 始终恢复。
        let mut old_root = std::mem::replace(
            &mut self.root,
            LayoutNode::Pane {
                id: String::new(),
                pane_id: String::new(),
            },
        );
        match Self::remove_from_node(&mut old_root, pane_id) {
            Ok(true) => {
                self.root = old_root;
                Ok(())
            }
            Ok(false) => {
                // 没有找到该 pane: 恢复原树, 返回错误而非虚假成功。
                self.root = old_root;
                anyhow::bail!("pane not found in layout: {}", pane_id);
            }
            Err(e) => {
                // 移除过程出错: 恢复原树, 向上传递错误。
                self.root = old_root;
                Err(e)
            }
        }
    }

    fn remove_from_node(node: &mut LayoutNode, pane_id: &str) -> anyhow::Result<bool> {
        let LayoutNode::Split {
            children, ratios, ..
        } = node
        else {
            return Ok(false);
        };

        let direct_child_index = children.iter().position(|child| {
            matches!(child, LayoutNode::Pane { pane_id: child_pane_id, .. } if child_pane_id == pane_id)
        });
        let removed = if let Some(index) = direct_child_index {
            children.remove(index);
            ratios.remove(index);
            true
        } else {
            let mut removed = false;
            for child in children.iter_mut() {
                if Self::remove_from_node(child, pane_id)? {
                    removed = true;
                    break;
                }
            }
            removed
        };

        if !removed {
            return Ok(false);
        }
        if children.len() == 1 {
            *node = children.remove(0);
        } else {
            Self::normalize_ratios(ratios);
        }
        Ok(true)
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
        let old_root = std::mem::replace(
            &mut self.root,
            LayoutNode::Pane {
                id: String::new(),
                pane_id: String::new(),
            },
        );
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

    /// §3.7 序列化布局树为 tmux 风格 (绝对 cell 计数 + 校验和)。
    ///
    /// 格式: `<checksum>,<body>`, checksum 覆盖 `<body>`。body 递归定义:
    /// - 叶子: `WxH,xoff,yoff,paneid`
    /// - 左右分割: `{child,child,...}`
    /// - 上下分割: `[child,child,...]`
    ///
    /// `cols`/`rows` 是容器总尺寸; 各 child 按 ratios 切分, 最后一个 child 吃掉
    /// 舍入余数, 保证铺满容器无缝隙。
    pub fn serialize(&self, cols: u32, rows: u32) -> anyhow::Result<String> {
        let body = self.serialize_node(&self.root, 0, 0, cols, rows)?;
        let checksum = Self::compute_checksum(&body);
        Ok(format!("{},{}", checksum, body))
    }

    fn serialize_node(
        &self,
        node: &LayoutNode,
        xoff: u32,
        yoff: u32,
        width: u32,
        height: u32,
    ) -> anyhow::Result<String> {
        match node {
            LayoutNode::Pane { pane_id, .. } => Ok(format!(
                "{}x{},{},{},{}",
                width, height, xoff, yoff, pane_id
            )),
            LayoutNode::Split {
                direction,
                children,
                ratios,
                ..
            } => {
                let (open, close) = match direction {
                    SplitDirection::LeftRight => ("{", "}"),
                    SplitDirection::TopBottom => ("[", "]"),
                };
                let n = children.len();
                let mut parts = Vec::with_capacity(n);
                let mut offset = 0u32;
                for (i, child) in children.iter().enumerate() {
                    let ratio = ratios.get(i).copied().unwrap_or(1.0 / n.max(1) as f32);
                    let (cw, ch, cx, cy) = match direction {
                        SplitDirection::LeftRight => {
                            let w = if i + 1 == n {
                                width - offset
                            } else {
                                (width as f32 * ratio) as u32
                            };
                            (w, height, xoff + offset, yoff)
                        }
                        SplitDirection::TopBottom => {
                            let h = if i + 1 == n {
                                height - offset
                            } else {
                                (height as f32 * ratio) as u32
                            };
                            (width, h, xoff, yoff + offset)
                        }
                    };
                    parts.push(self.serialize_node(child, cx, cy, cw, ch)?);
                    offset += match direction {
                        SplitDirection::LeftRight => cw,
                        SplitDirection::TopBottom => ch,
                    };
                }
                Ok(format!("{}{}{}", open, parts.join(","), close))
            }
        }
    }

    /// §3.7 反序列化布局树。
    ///
    /// 优先解析新的 tmux 风格格式 (`<checksum>,<body>`); 失败时回退到旧的
    /// float-ratio 行格式 (向后兼容已持久化的旧快照)。
    pub fn deserialize(s: &str) -> anyhow::Result<LayoutTree> {
        if let Ok(tree) = Self::deserialize_tmux(s) {
            return Ok(tree);
        }
        Self::deserialize_legacy(s)
    }

    /// §3.7 解析 tmux 风格格式: `<checksum>,<body>`。
    fn deserialize_tmux(s: &str) -> anyhow::Result<LayoutTree> {
        let (checksum_str, body) = s
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("layout: missing checksum prefix"))?;
        let expected: u32 = checksum_str
            .parse()
            .map_err(|_| anyhow::anyhow!("layout: invalid checksum"))?;
        anyhow::ensure!(
            Self::compute_checksum(body) == expected,
            "layout: checksum mismatch"
        );

        let mut next_id = 0usize;
        let mut node_ids = HashMap::new();
        let (root, _w, _h, rest) = Self::parse_node(body, &mut next_id, &mut node_ids)?;
        anyhow::ensure!(rest.is_empty(), "layout: trailing data after body");
        Ok(LayoutTree { root, node_ids })
    }

    /// 递归下降解析一个节点, 返回 (节点, 宽, 高, 剩余输入)。
    /// 叶子: `WxH,xoff,yoff,paneid`; 分割: `{...}` (左右) / `[...]` (上下)。
    fn parse_node<'a>(
        s: &'a str,
        next_id: &mut usize,
        node_ids: &mut HashMap<String, usize>,
    ) -> anyhow::Result<(LayoutNode, u32, u32, &'a str)> {
        let bytes = s.as_bytes();
        anyhow::ensure!(!bytes.is_empty(), "layout: unexpected end of input");
        match bytes[0] {
            b'{' | b'[' => Self::parse_split(s, next_id, node_ids),
            b'0'..=b'9' => Self::parse_leaf(s, next_id, node_ids),
            c => anyhow::bail!("layout: unexpected byte {:?} at node start", c as char),
        }
    }

    fn parse_leaf<'a>(
        s: &'a str,
        next_id: &mut usize,
        node_ids: &mut HashMap<String, usize>,
    ) -> anyhow::Result<(LayoutNode, u32, u32, &'a str)> {
        // 格式: WxH,xoff,yoff,paneid
        let bytes = s.as_bytes();
        let mut i = 0;
        let width = Self::scan_uint(bytes, &mut i)?;
        anyhow::ensure!(
            i < bytes.len() && bytes[i] == b'x',
            "layout: expected 'x' in WxH"
        );
        i += 1;
        let height = Self::scan_uint(bytes, &mut i)?;
        anyhow::ensure!(
            i < bytes.len() && bytes[i] == b',',
            "layout: expected ',' after WxH"
        );
        i += 1;
        let _xoff = Self::scan_uint(bytes, &mut i)?;
        anyhow::ensure!(
            i < bytes.len() && bytes[i] == b',',
            "layout: expected ',' after xoff"
        );
        i += 1;
        let _yoff = Self::scan_uint(bytes, &mut i)?;
        anyhow::ensure!(
            i < bytes.len() && bytes[i] == b',',
            "layout: expected ',' after yoff"
        );
        i += 1;
        // paneid 直到分隔符 (',' '}' ']') 或结尾; 分隔符均为 ASCII, 对 UTF-8 安全。
        let start = i;
        while i < bytes.len() && !matches!(bytes[i], b',' | b'}' | b']') {
            i += 1;
        }
        let pane_id = std::str::from_utf8(&bytes[start..i])?.to_string();
        let id = Self::alloc_id(next_id, node_ids);
        Ok((LayoutNode::Pane { id, pane_id }, width, height, &s[i..]))
    }

    fn parse_split<'a>(
        s: &'a str,
        next_id: &mut usize,
        node_ids: &mut HashMap<String, usize>,
    ) -> anyhow::Result<(LayoutNode, u32, u32, &'a str)> {
        let bytes = s.as_bytes();
        let (direction, close): (SplitDirection, u8) = match bytes[0] {
            b'{' => (SplitDirection::LeftRight, b'}'),
            b'[' => (SplitDirection::TopBottom, b']'),
            _ => anyhow::bail!("layout: expected split opener"),
        };
        let mut rest = &s[1..];
        let mut children = Vec::new();
        let mut widths = Vec::new();
        let mut heights = Vec::new();
        loop {
            let (child, w, h, after) = Self::parse_node(rest, next_id, node_ids)?;
            children.push(child);
            widths.push(w);
            heights.push(h);
            let ab = after.as_bytes();
            anyhow::ensure!(!ab.is_empty(), "layout: unterminated split");
            match ab[0] {
                b',' => rest = &after[1..],
                c if c == close => {
                    rest = &after[1..];
                    break;
                }
                c => anyhow::bail!("layout: unexpected byte {:?} in split", c as char),
            }
        }

        // 从绝对 cell 计数反推 ratios: child 主轴尺寸 / 容器主轴尺寸。
        let (total_w, total_h) = match direction {
            SplitDirection::LeftRight => (
                widths.iter().sum::<u32>(),
                heights.iter().copied().max().unwrap_or(0),
            ),
            SplitDirection::TopBottom => (
                widths.iter().copied().max().unwrap_or(0),
                heights.iter().sum::<u32>(),
            ),
        };
        let denom = match direction {
            SplitDirection::LeftRight => total_w,
            SplitDirection::TopBottom => total_h,
        };
        let ratios: Vec<f32> = if denom == 0 {
            vec![1.0 / children.len().max(1) as f32; children.len()]
        } else {
            match direction {
                SplitDirection::LeftRight => {
                    widths.iter().map(|w| *w as f32 / denom as f32).collect()
                }
                SplitDirection::TopBottom => {
                    heights.iter().map(|h| *h as f32 / denom as f32).collect()
                }
            }
        };
        let id = Self::alloc_id(next_id, node_ids);
        Ok((
            LayoutNode::Split {
                id,
                direction,
                children,
                ratios,
            },
            total_w,
            total_h,
            rest,
        ))
    }

    /// 扫描无符号整数并推进游标。
    fn scan_uint(bytes: &[u8], i: &mut usize) -> anyhow::Result<u32> {
        let start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        anyhow::ensure!(*i > start, "layout: expected a number");
        let s = std::str::from_utf8(&bytes[start..*i])?;
        s.parse::<u32>()
            .map_err(|_| anyhow::anyhow!("layout: number out of range"))
    }

    fn alloc_id(next_id: &mut usize, node_ids: &mut HashMap<String, usize>) -> String {
        let idx = *next_id;
        *next_id += 1;
        let id = format!("n{}", idx);
        node_ids.insert(id.clone(), idx);
        id
    }

    /// §3.7 解析旧的 float-ratio 行格式 (向后兼容): 前序遍历,
    /// `P:id:pane_id` / `S:id:dir:[ratios]`, 末行 checksum。
    fn deserialize_legacy(s: &str) -> anyhow::Result<LayoutTree> {
        let mut lines: Vec<&str> = s.lines().collect();
        anyhow::ensure!(lines.len() >= 2, "layout: legacy format too short");
        let checksum_line = lines.pop().unwrap();
        let expected: u32 = checksum_line
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("layout: invalid legacy checksum"))?;
        // 旧 serialize 的 buf = 每个节点行 + '\n'; checksum 覆盖该 buf。
        let mut buf = String::new();
        for line in &lines {
            buf.push_str(line);
            buf.push('\n');
        }
        anyhow::ensure!(
            Self::compute_checksum(&buf) == expected,
            "layout: legacy checksum mismatch"
        );

        let mut idx = 0usize;
        let mut node_ids = HashMap::new();
        let root = Self::parse_legacy_node(&lines, &mut idx, &mut node_ids)?;
        Ok(LayoutTree { root, node_ids })
    }

    fn parse_legacy_node(
        lines: &[&str],
        idx: &mut usize,
        node_ids: &mut HashMap<String, usize>,
    ) -> anyhow::Result<LayoutNode> {
        let line = lines
            .get(*idx)
            .ok_or_else(|| anyhow::anyhow!("layout: legacy truncated"))?;
        *idx += 1;
        if let Some(rest) = line.strip_prefix("P:") {
            let (id, pane_id) = rest
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("layout: bad legacy pane line"))?;
            node_ids.insert(id.to_string(), node_ids.len());
            Ok(LayoutNode::Pane {
                id: id.to_string(),
                pane_id: pane_id.to_string(),
            })
        } else if let Some(rest) = line.strip_prefix("S:") {
            // 格式: S:id:dir:[ratios]
            let (id, rest) = rest
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("layout: bad legacy split id"))?;
            let (dir, ratios_str) = rest
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("layout: bad legacy split dir"))?;
            let direction = match dir {
                "H" => SplitDirection::LeftRight,
                "V" => SplitDirection::TopBottom,
                _ => anyhow::bail!("layout: bad legacy direction"),
            };
            let ratios = Self::parse_legacy_ratios(ratios_str)?;
            let mut children = Vec::with_capacity(ratios.len());
            for _ in 0..ratios.len() {
                children.push(Self::parse_legacy_node(lines, idx, node_ids)?);
            }
            node_ids.insert(id.to_string(), node_ids.len());
            Ok(LayoutNode::Split {
                id: id.to_string(),
                direction,
                children,
                ratios,
            })
        } else {
            anyhow::bail!("layout: unrecognized legacy line: {}", line)
        }
    }

    /// 解析 `{:?}` 格式的 Vec<f32>, 如 `[0.5, 0.5]`。
    fn parse_legacy_ratios(s: &str) -> anyhow::Result<Vec<f32>> {
        let inner = s
            .trim()
            .strip_prefix('[')
            .and_then(|t| t.strip_suffix(']'))
            .ok_or_else(|| anyhow::anyhow!("layout: bad legacy ratios"))?;
        if inner.trim().is_empty() {
            return Ok(Vec::new());
        }
        inner
            .split(',')
            .map(|p| {
                p.trim()
                    .parse::<f32>()
                    .map_err(|_| anyhow::anyhow!("layout: bad ratio"))
            })
            .collect()
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
