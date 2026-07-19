//! VDOM Bridge — maps JSON VDOM returned by QuickJS extensions to GPUI elements.
//!
//! Spec §5.4: Extensions return a Virtual DOM (JSON) → native GPUI bridge maps
//! it to elements. Extensions never call GPUI directly.
//!
//! This module provides the VDOM parsing and a placeholder renderer.
//! Full GPUI element mapping requires per-element-type dispatch which is
//! non-trivial with GPUI's builder pattern. The placeholder returns a
//! text representation of the VDOM tree, to be replaced incrementally.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A VDOM node — the JSON structure extensions return from render() calls.
///
/// Spec §5.4 format:
/// ```json
/// {
///   "type": "div",
///   "props": { "id": "status-bar" },
///   "style": { "gap": "4px" },
///   "children": ["text", { "type": "button", ... }]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VDomNode {
    /// Element type: "div", "span", "text", "button", etc.
    #[serde(rename = "type")]
    pub element_type: String,
    /// Optional properties (id, class, onClick handlers, etc.)
    #[serde(default)]
    pub props: BTreeMap<String, serde_json::Value>,
    /// Optional inline styles
    #[serde(default)]
    pub style: BTreeMap<String, String>,
    /// Children: text strings or nested VDomNode
    #[serde(default)]
    pub children: Vec<VDomChild>,
}

/// A VDOM child — either a text string or a nested element node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VDomChild {
    /// Plain text content
    Text(String),
    /// Nested element
    Node(VDomNode),
}

/// Parse a VDOM JSON value into a VDomNode tree.
///
/// Extensions return `serde_json::Value` from QuickJS; this validates
/// and converts to typed VDomNode for the renderer.
pub fn parse_vdom(value: &serde_json::Value) -> Result<VDomNode> {
    serde_json::from_value(value.clone())
        .map_err(|e| anyhow::anyhow!("VDOM parse error: {}", e))
}

/// Flatten a VDOM tree into a text representation (for placeholder rendering).
///
/// This is used by the native chrome baseline (§5.1) to show VDOM content
/// before the full GPUI element mapping is implemented.
pub fn vdom_to_text(node: &VDomNode, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let mut out = format!("{}<{}", indent, node.element_type);
    if let Some(id) = node.props.get("id") {
        out.push_str(&format!(" id={}", id));
    }
    out.push('>');
    for child in &node.children {
        match child {
            VDomChild::Text(t) => {
                out.push_str(&format!("\n{}  {}", indent, t));
            }
            VDomChild::Node(n) => {
                out.push('\n');
                out.push_str(&vdom_to_text(n, depth + 1));
            }
        }
    }
    out.push_str(&format!("\n{}</{}>", indent, node.element_type));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_div() {
        let json = serde_json::json!({
            "type": "div",
            "props": { "id": "test" },
            "children": ["hello"]
        });
        let node = parse_vdom(&json).expect("parse");
        assert_eq!(node.element_type, "div");
        assert_eq!(node.props.get("id").unwrap(), "test");
        assert_eq!(node.children.len(), 1);
        match &node.children[0] {
            VDomChild::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("expected text child"),
        }
    }

    #[test]
    fn parse_nested() {
        let json = serde_json::json!({
            "type": "div",
            "children": [
                { "type": "span", "children": ["nested"] }
            ]
        });
        let node = parse_vdom(&json).expect("parse");
        match &node.children[0] {
            VDomChild::Node(n) => assert_eq!(n.element_type, "span"),
            _ => panic!("expected node child"),
        }
    }

    #[test]
    fn vdom_to_text_produces_readable_output() {
        let node = VDomNode {
            element_type: "div".into(),
            props: BTreeMap::new(),
            style: BTreeMap::new(),
            children: vec![VDomChild::Text("Hello".into())],
        };
        let text = vdom_to_text(&node, 0);
        assert!(text.contains("<div>"));
        assert!(text.contains("Hello"));
        assert!(text.contains("</div>"));
    }
}
