//! Delta Chain：bounded D_MAX=16，Rope-level replay
//!
//! Delta 是 Rope 级别的增量操作列表。
//! 当 delta_depth 达到 D_MAX 时，强制 materialize full snapshot。
//! 内容重建：从当前版本回退到最近的 full snapshot（≤ D_MAX 步），
//! 然后向前应用 deltas。

use std::sync::Arc;

use rope::Rope;

use crate::version_tree::VersionNode;

/// Delta 链最大深度
pub const D_MAX: u8 = 16;

/// 单个 delta 操作
#[derive(Debug, Clone)]
pub enum DeltaOp {
    /// 在 offset 位置删除 delete_len 字节
    Delete { offset: usize, delete_len: usize },
    /// 在 offset 位置插入内容
    Insert { offset: usize, text: Arc<Rope> },
    /// 在 offset 位置替换（先删后插）
    Replace {
        offset: usize,
        delete_len: usize,
        text: Arc<Rope>,
    },
}

/// Delta replay 引擎
pub struct DeltaReplay;

impl DeltaReplay {
    /// 将 delta 操作应用到 Rope 上
    ///
    /// 时间复杂度：O(log N + ||insert||) 每操作
    pub fn apply_delta(base: &mut Rope, ops: &[DeltaOp]) {
        for op in ops {
            match op {
                DeltaOp::Delete { offset, delete_len } => {
                    let end = offset.saturating_add(*delete_len).min(base.len());
                    base.replace(*offset..end, "");
                }
                DeltaOp::Insert { offset, text } => {
                    let pos = *offset.min(&base.len());
                    base.replace(pos..pos, &text.to_string());
                }
                DeltaOp::Replace {
                    offset,
                    delete_len,
                    text,
                } => {
                    let end = offset.saturating_add(*delete_len).min(base.len());
                    base.replace(*offset..end, &text.to_string());
                }
            }
        }
    }

    /// 重建内容:从版本 V 回溯到最近的 full snapshot,向前应用 deltas。
    ///
    /// 参数:
    /// - target: 目标版本节点
    /// - get_node: 获取祖先节点的闭包,返回 Arc<VersionNode>
    /// - get_blob: 通过 ContentHash 获取 blob 内容的闭包 (full snapshot 内容
    ///   和 delta 序列化字节)
    ///
    /// 返回重建后的 Rope;None 表示无法重建 (缺少 full snapshot 或 blob)。
    pub fn reconstruct(
        target: &VersionNode,
        get_node: impl Fn(u64) -> Option<Arc<VersionNode>>,
        get_blob: impl Fn(&[u8; 32]) -> Option<Vec<u8>>,
    ) -> Option<Rope> {
        // 收集从 target 回溯到最近 full snapshot 的路径 (不含 base)。
        // 最多走 D_MAX+1 步:按契约 delta 深度 ≤ D_MAX,因此至多 D_MAX 个 delta
        // 跳后再取到 full base(共 D_MAX+1 次 get_node)。多余则视为链断裂。
        let mut path: Vec<Arc<VersionNode>> = Vec::new();
        let mut current_id = target.version_id;
        let mut base_content: Option<Vec<u8>> = None;
        for _ in 0..=D_MAX {
            let node = get_node(current_id)?;
            if let Some(full_hash) = &node.full_content {
                base_content = get_blob(full_hash);
                break;
            }
            path.push(node.clone());
            match node.parent_id {
                Some(parent) => current_id = parent,
                None => return None,
            }
        }
        let mut rope = Rope::from(
            std::str::from_utf8(&base_content?).ok()?.to_string(),
        );

        // deltas 按从 base 到 target 的顺序应用 (path 是反向收集的)。
        path.reverse();
        for node in path {
            let delta_ref = node.delta.as_ref()?;
            let delta_bytes = get_blob(&delta_ref.hash)?;
            let delta_ops = deserialize_delta_ops(&delta_bytes)?;
            Self::apply_delta(&mut rope, &delta_ops);
        }
        Some(rope)
    }

    /// 判断是否需要 materialize full snapshot
    ///
    /// 当 delta_depth == D_MAX 时返回 true
    pub fn should_materialize(delta_depth: u8) -> bool {
        delta_depth >= D_MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delta_apply_delete() {
        let mut rope = Rope::from("Hello, World!");
        DeltaReplay::apply_delta(&mut rope, &[DeltaOp::Delete {
            offset: 7,
            delete_len: 5, // "World"
        }]);
        assert_eq!(rope.to_string(), "Hello, !");
    }

    #[test]
    fn test_delta_apply_insert() {
        let mut rope = Rope::from("Hello!");
        let insert_text = Arc::new(Rope::from(" Beautiful"));
        DeltaReplay::apply_delta(
            &mut rope,
            &[DeltaOp::Insert {
                offset: 5,
                text: insert_text,
            }],
        );
        assert_eq!(rope.to_string(), "Hello Beautiful!");
    }

    #[test]
    fn test_delta_apply_replace() {
        let mut rope = Rope::from("Hello, World!");
        let new_text = Arc::new(Rope::from("Z3rm"));
        DeltaReplay::apply_delta(
            &mut rope,
            &[DeltaOp::Replace {
                offset: 7,
                delete_len: 5,
                text: new_text,
            }],
        );
        assert_eq!(rope.to_string(), "Hello, Z3rm!");
    }

    #[test]
    fn test_materialize_threshold() {
        assert!(!DeltaReplay::should_materialize(15));
        assert!(DeltaReplay::should_materialize(16));
        assert!(DeltaReplay::should_materialize(20));
    }

    #[test]
    fn test_delta_chain_depth_tracking() {
        // 验证 D_MAX 常量
        assert_eq!(D_MAX, 16);
    }
}

// ============================================================================
// §4.6 DeltaOp 序列化 (用于持久化 delta 到 BlobStore)
// ============================================================================
//
// 手写的二进制格式,避免引入 serde/bincode 依赖。格式:
//   [u32 ops_count_BE]
//   对每个 op:
//     [u8 tag]  0=Delete, 1=Insert, 2=Replace
//     [u64 offset_BE]
//     Delete:  [u64 delete_len_BE]
//     Insert:  [u32 text_len_BE][text_bytes UTF-8]
//     Replace: [u64 delete_len_BE][u32 text_len_BE][text_bytes]
//
// 这个格式只用于内部持久化;version 兼容性由 delta_ref.hash (内容寻址) 保证。

/// §4.6 把 DeltaOp 列表序列化为字节。
pub fn serialize_delta_ops(ops: &[DeltaOp]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(ops.len() as u32).to_be_bytes());
    for op in ops {
        match op {
            DeltaOp::Delete { offset, delete_len } => {
                buf.push(0);
                buf.extend_from_slice(&(*offset as u64).to_be_bytes());
                buf.extend_from_slice(&(*delete_len as u64).to_be_bytes());
            }
            DeltaOp::Insert { offset, text } => {
                buf.push(1);
                buf.extend_from_slice(&(*offset as u64).to_be_bytes());
                let s = text.to_string();
                let bytes = s.as_bytes();
                buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                buf.extend_from_slice(bytes);
            }
            DeltaOp::Replace {
                offset,
                delete_len,
                text,
            } => {
                buf.push(2);
                buf.extend_from_slice(&(*offset as u64).to_be_bytes());
                buf.extend_from_slice(&(*delete_len as u64).to_be_bytes());
                let s = text.to_string();
                let bytes = s.as_bytes();
                buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                buf.extend_from_slice(bytes);
            }
        }
    }
    buf
}

/// §4.6 反序列化 DeltaOp 列表。失败返回 None (调用方按缺 blob 处理)。
pub fn deserialize_delta_ops(bytes: &[u8]) -> Option<Vec<DeltaOp>> {
    let mut cur = 0;
    if bytes.len() < 4 {
        return None;
    }
    let count = u32::from_be_bytes(bytes[cur..cur + 4].try_into().ok()?);
    cur += 4;
    let mut ops = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let tag = *bytes.get(cur)?;
        cur += 1;
        let offset = u64::from_be_bytes(bytes.get(cur..cur + 8)?.try_into().ok()?) as usize;
        cur += 8;
        let op = match tag {
            0 => {
                let delete_len =
                    u64::from_be_bytes(bytes.get(cur..cur + 8)?.try_into().ok()?) as usize;
                cur += 8;
                DeltaOp::Delete { offset, delete_len }
            }
            1 => {
                let text_len = u32::from_be_bytes(bytes.get(cur..cur + 4)?.try_into().ok()?) as usize;
                cur += 4;
                let text = std::str::from_utf8(bytes.get(cur..cur + text_len)?).ok()?.to_string();
                cur += text_len;
                DeltaOp::Insert {
                    offset,
                    text: Arc::new(Rope::from(text)),
                }
            }
            2 => {
                let delete_len =
                    u64::from_be_bytes(bytes.get(cur..cur + 8)?.try_into().ok()?) as usize;
                cur += 8;
                let text_len = u32::from_be_bytes(bytes.get(cur..cur + 4)?.try_into().ok()?) as usize;
                cur += 4;
                let text = std::str::from_utf8(bytes.get(cur..cur + text_len)?).ok()?.to_string();
                cur += text_len;
                DeltaOp::Replace {
                    offset,
                    delete_len,
                    text: Arc::new(Rope::from(text)),
                }
            }
            _ => return None,
        };
        ops.push(op);
    }
    Some(ops)
}
