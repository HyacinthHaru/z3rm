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


/// §5.4 Convert a VDOM tree into a GPUI element tree.
///
/// Maps VDOM node types to GPUI elements:
/// - "div" → flex container div
/// - "span" → inline text div
/// - "button" → clickable div
/// - Text children → SharedString labels
///
/// Style properties are mapped to GPUI style methods where possible.
pub fn vdom_to_element(node: &VDomNode) -> gpui::AnyElement {
    use gpui::{div, px, IntoElement, ParentElement, SharedString, Styled};

    let mut element = div();

    // Apply flex layout styles from VDOM
    if let Some(direction) = node.style.get("flexDirection") {
        match direction.as_str() {
            "row" => { element = element.flex_row(); }
            "column" => { element = element.flex_col(); }
            _ => {}
        }
    } else {
        // Default: div is flex container
        element = element.flex();
    }

    if let Some(gap) = node.style.get("gap") {
        if let Some(px_val) = gap.strip_suffix("px") {
            if let Ok(val) = px_val.trim().parse::<f32>() {
                element = element.gap(px(val));
            }
        }
    }

    if let Some(justify) = node.style.get("justifyContent") {
        match justify.as_str() {
            "space-between" => { element = element.justify_between(); }
            "center" => { element = element.justify_center(); }
            _ => {}
        }
    }

    // §5.4 core styles: color (text), background, fontSize, padding. Each is
    // best-effort: an unparseable value is skipped (parse helpers return None),
    // so a malformed style never breaks the whole chrome render.
    if let Some(color) = node.style.get("color").and_then(|v| parse_color(v)) {
        element = element.text_color(color);
    }
    if let Some(bg) = node.style.get("background").and_then(|v| parse_color(v)) {
        element = element.bg(bg);
    }
    if let Some(size) = node.style.get("fontSize").and_then(|v| parse_px(v)) {
        element = element.text_size(px(size));
    }
    if let Some(pad) = node.style.get("padding").and_then(|v| parse_px(v)) {
        element = element.px(px(pad));
    }

    // Add children
    for child in &node.children {
        match child {
            VDomChild::Text(text) => {
                let label: SharedString = text.clone().into();
                element = element.child(label);
            }
            VDomChild::Node(child_node) => {
                element = element.child(vdom_to_element(child_node));
            }
        }
    }

    element.into_any_element()
}

/// §5.4 parse a CSS hex color (`#rrggbb`) into an `Hsla`. Returns None for
/// unparseable values so the bridge can skip the style rather than panic.
pub fn parse_color(value: &str) -> Option<gpui::Hsla> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let parsed = u32::from_str_radix(hex, 16).ok()?;
    Some(gpui::rgb(parsed).into())
}

/// §5.4 parse a CSS pixel length (`Npx`) into an f32. Returns None for
/// non-numeric or non-px values.
pub fn parse_px(value: &str) -> Option<f32> {
    let num = value.strip_suffix("px")?.trim();
    num.parse::<f32>().ok()
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
    fn parse_color_and_px_helpers_round_trip_core_styles() {
        // §5.4 the bridge must turn CSS-like style strings into GPUI values.
        // These helpers are the pure primitives vdom_to_element uses; without
        // them, color/background/fontSize/padding are silently ignored.
        assert_eq!(parse_color("#ff0000"), Some(gpui::rgb(0xff0000).into()));
        assert_eq!(parse_color("#000000"), Some(gpui::rgb(0x000000).into()));
        assert_eq!(parse_color("not-a-color"), None);
        assert_eq!(parse_px("14px"), Some(14.0));
        assert_eq!(parse_px("4px"), Some(4.0));
        assert_eq!(parse_px("nope"), None);
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
