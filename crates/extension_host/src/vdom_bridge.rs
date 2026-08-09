//! VDOM Bridge — maps JSON VDOM returned by QuickJS extensions to GPUI elements.
//!
//! Spec §5.4: Extensions return a Virtual DOM (JSON) → native GPUI bridge maps
//! it to elements. Extensions never call GPUI directly.
//!
//! High-frequency widgets use the display-list pattern instead: the node
//! carries `props.renderer` naming a view method, the host calls it each tick,
//! and the resulting draw ops are painted without walking the VDOM diff path.

use anyhow::Result;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, FocusHandle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

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

/// A VDOM child — either text, a scalar rendered as text, or a nested element.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VDomChild {
    /// Plain text content
    Text(String),
    /// Nested element
    Node(VDomNode),
}

fn normalize_vdom_node(value: &mut serde_json::Value) {
    let serde_json::Value::Object(properties) = value else {
        return;
    };
    let Some(serde_json::Value::Array(children)) = properties.get_mut("children") else {
        return;
    };

    let mut normalized = Vec::with_capacity(children.len());
    for mut child in std::mem::take(children) {
        match child {
            serde_json::Value::Null => {}
            serde_json::Value::Number(number) => {
                normalized.push(serde_json::Value::String(number.to_string()));
            }
            serde_json::Value::Bool(boolean) => {
                normalized.push(serde_json::Value::String(boolean.to_string()));
            }
            _ => {
                normalize_vdom_node(&mut child);
                normalized.push(child);
            }
        }
    }
    *children = normalized;
}

/// Parse a VDOM JSON value into a VDomNode tree.
///
/// QuickJS extensions commonly put numbers, booleans, or null in `children`.
/// Normalize those JavaScript scalar semantics before deserializing the typed
/// tree; null children are ignored and other scalars render as text.
pub fn parse_vdom(value: &serde_json::Value) -> Result<VDomNode> {
    let mut normalized = value.clone();
    normalize_vdom_node(&mut normalized);
    serde_json::from_value(normalized).map_err(|e| anyhow::anyhow!("VDOM parse error: {}", e))
}

/// Flatten a VDOM tree into a text representation.
///
/// Used by tests and by headless callers that need to assert on chrome
/// content without driving a GPUI frame.
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

/// §5.4 A command an extension asked the bridge to invoke on interaction.
///
/// Extensions describe interactions declaratively — `props.onClick` and
/// `props.onChange` name a registered command rather than carrying a JS
/// closure, so the descriptor survives the JSON boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandInvocation {
    /// Registered command id to execute.
    pub command: String,
    /// Positional arguments forwarded to the command.
    pub args: Vec<serde_json::Value>,
}

impl CommandInvocation {
    /// Parse `{ "command": "...", "args": [...] }`. A bare string is accepted
    /// as shorthand for a no-argument invocation.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        if let Some(command) = value.as_str() {
            return Some(Self {
                command: command.to_string(),
                args: Vec::new(),
            });
        }
        let command = value.get("command")?.as_str()?.to_string();
        let args = match value.get("args") {
            Some(serde_json::Value::Array(items)) => items.clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        Some(Self { command, args })
    }
}

/// §5.4 A single display-list draw operation.
///
/// Display lists bypass VDOM reconciliation for widgets that repaint
/// continuously (clocks, meters), so a ticking widget never invalidates the
/// surrounding chrome tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum DrawOp {
    /// Draw a text run at a pixel offset within the display-list region.
    #[serde(rename = "drawText")]
    DrawText {
        text: String,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
        #[serde(default)]
        color: Option<String>,
    },
    /// Fill an axis-aligned rectangle.
    #[serde(rename = "fillRect")]
    FillRect {
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
        #[serde(default, alias = "w")]
        width: f32,
        #[serde(default, alias = "h")]
        height: f32,
        #[serde(default)]
        color: Option<String>,
    },
}

/// Parse the JSON array a display-list renderer method returns.
///
/// Unknown ops are rejected rather than skipped: a typo in an op name would
/// otherwise silently paint nothing, which is far harder to diagnose.
pub fn parse_display_list(value: &serde_json::Value) -> Result<Vec<DrawOp>> {
    serde_json::from_value(value.clone())
        .map_err(|e| anyhow::anyhow!("display list parse error: {}", e))
}

/// Theme colors the bridge needs for semantic classes and defaults.
///
/// `extension_host` must not depend on `theme`, so the embedding view resolves
/// these from `cx.theme()` and hands them in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VDomPalette {
    pub text: Hsla,
    pub muted_text: Hsla,
    pub background: Hsla,
    pub selected_background: Hsla,
    pub border: Hsla,
}

impl Default for VDomPalette {
    fn default() -> Self {
        Self {
            text: gpui::white(),
            muted_text: gpui::opaque_grey(0.6, 1.0),
            background: gpui::transparent_black(),
            selected_background: gpui::opaque_grey(0.3, 1.0),
            border: gpui::opaque_grey(0.4, 1.0),
        }
    }
}

/// Callback the bridge invokes when an element's interaction fires.
pub type CommandDispatch = Rc<dyn Fn(CommandInvocation, &mut Window, &mut App)>;

/// §5.4 Converts VDOM trees into GPUI element trees.
///
/// Holds the state a pure function cannot: focus handles must survive across
/// frames for text inputs to keep focus, and display-list output arrives out
/// of band from the host's renderer tick.
pub struct VDomRenderer {
    palette: VDomPalette,
    display_lists: BTreeMap<SharedString, Vec<DrawOp>>,
    /// Display-list region ids present in the frame currently being rendered.
    /// [`render_frame`](Self::render_frame) uses this to evict cached ops for
    /// regions that disappeared from the VDOM, keeping native-side
    /// display-list state bounded by what extensions actually render (§5.4
    /// cache discipline, §5.2 per-extension resource limits).
    seen_display_regions: HashSet<SharedString>,
    focus_handles: HashMap<SharedString, FocusHandle>,
    dispatch: Option<CommandDispatch>,
}

impl Default for VDomRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl VDomRenderer {
    pub fn new() -> Self {
        Self {
            palette: VDomPalette::default(),
            display_lists: BTreeMap::new(),
            seen_display_regions: HashSet::new(),
            focus_handles: HashMap::new(),
            dispatch: None,
        }
    }

    pub fn set_palette(&mut self, palette: VDomPalette) {
        self.palette = palette;
    }

    pub fn palette(&self) -> &VDomPalette {
        &self.palette
    }

    /// Publish the draw ops a display-list renderer produced for `region_id`.
    pub fn set_display_list(&mut self, region_id: impl Into<SharedString>, ops: Vec<DrawOp>) {
        self.display_lists.insert(region_id.into(), ops);
    }

    pub fn display_list(&self, region_id: &str) -> Option<&[DrawOp]> {
        self.display_lists.get(region_id).map(Vec::as_slice)
    }

    /// Register the callback that executes `onClick` / `onChange` commands.
    pub fn set_dispatch(&mut self, dispatch: CommandDispatch) {
        self.dispatch = Some(dispatch);
    }

    /// Convert a VDOM tree into a GPUI element tree.
    ///
    /// Single-node convenience: renders the node as a one-node frame, evicting
    /// display-list regions that are not part of it. Embedders that render
    /// several top-level nodes per GPUI frame must use
    /// [`render_frame`](Self::render_frame) so the eviction sees the whole
    /// frame at once.
    pub fn render(&mut self, node: &VDomNode, cx: &mut App) -> AnyElement {
        let mut elements = self.render_frame(std::slice::from_ref(node), cx);
        elements
            .pop()
            .expect("render_frame returns one element per node")
    }

    /// Render a whole frame of top-level nodes in one pass, then evict cached
    /// display-list ops for regions that did not appear anywhere in the frame.
    ///
    /// §5.4 requires the native side to cache display lists so ticking widgets
    /// repaint without full VDOM reconciliation; a cache is only bounded if
    /// entries for regions that disappeared are dropped. Without this an
    /// extension that cycles region ids (or stops rendering a region) would
    /// grow the native-side cache without limit (§5.2 per-extension resource
    /// limits). Regions that appear again later simply repopulate the cache
    /// from their `drawOps` on the next frame.
    pub fn render_frame(&mut self, nodes: &[VDomNode], cx: &mut App) -> Vec<AnyElement> {
        let mut elements = Vec::with_capacity(nodes.len());
        for node in nodes {
            elements.push(self.render_node(node, &mut ElementPath::root(), cx));
        }
        self.display_lists
            .retain(|region, _| self.seen_display_regions.contains(region));
        self.seen_display_regions.clear();
        elements
    }

    fn render_node(&mut self, node: &VDomNode, path: &mut ElementPath, cx: &mut App) -> AnyElement {
        match node.element_type.as_str() {
            "display-list" => self.render_display_list(node),
            "input" => self.render_input(node, path, cx),
            "spacer" => apply_styles(div().flex_grow(1.0), node, &self.palette).into_any_element(),
            _ => self.render_container(node, path, cx),
        }
    }

    fn render_container(
        &mut self,
        node: &VDomNode,
        path: &mut ElementPath,
        cx: &mut App,
    ) -> AnyElement {
        let click = node.props.get("onClick").and_then(CommandInvocation::parse);
        let is_button = node.element_type == "button";

        // Only interactive nodes need a stateful element id. Plain containers
        // remain stateless while buttons expose keyboard and accessibility
        // semantics equivalent to a native control.
        if click.is_none() && !is_button {
            let element = self.style_and_fill(div(), node, path, cx);
            return element.into_any_element();
        }

        let mut element =
            self.style_and_fill(div().id(self.element_id(node, path)), node, path, cx);
        if let Some(label) = node
            .props
            .get("aria-label")
            .or_else(|| node.props.get("ariaLabel"))
            .and_then(|value| value.as_str())
        {
            element = element.aria_label(SharedString::from(label.to_owned()));
        }

        let button_focus_handle = if is_button {
            Some(
                self.focus_handles
                    .entry(self.element_key(node, path).into())
                    .or_insert_with(|| cx.focus_handle())
                    .clone(),
            )
        } else {
            None
        };
        if let Some(focus_handle) = button_focus_handle.as_ref() {
            element = element
                .role(Role::Button)
                .tab_stop(true)
                .track_focus(focus_handle)
                .cursor_pointer();
        }

        if let (Some(invocation), Some(dispatch)) = (click.clone(), self.dispatch.clone()) {
            let focus_handle = button_focus_handle.clone();
            element = element.on_click(move |_event: &ClickEvent, window, cx| {
                if let Some(focus_handle) = focus_handle.as_ref() {
                    window.focus(focus_handle, cx);
                }
                dispatch(invocation.clone(), window, cx);
            });
        }
        if is_button && let (Some(invocation), Some(dispatch)) = (click, self.dispatch.clone()) {
            element = element.on_key_down(move |event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    dispatch(invocation.clone(), window, cx);
                }
            });
        }
        element.into_any_element()
    }

    /// A text field driven entirely by the extension: the displayed value comes
    /// from `props.value` and every edit is dispatched through `onChange`, so
    /// the extension stays the single owner of the text.
    fn render_input(
        &mut self,
        node: &VDomNode,
        path: &mut ElementPath,
        cx: &mut App,
    ) -> AnyElement {
        let id: SharedString = self.element_key(node, path).into();
        let focus_handle = self
            .focus_handles
            .entry(id.clone())
            .or_insert_with(|| cx.focus_handle())
            .clone();

        let value = node
            .props
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let placeholder = node
            .props
            .get("placeholder")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let change = node
            .props
            .get("onChange")
            .and_then(CommandInvocation::parse);

        let showing_placeholder = value.is_empty() && !placeholder.is_empty();
        let label: SharedString = if showing_placeholder {
            placeholder.into()
        } else {
            value.clone().into()
        };

        let mut element = div()
            .id(ElementId::Name(id))
            .role(Role::TextInput)
            .tab_stop(true)
            .track_focus(&focus_handle)
            .border_1()
            .border_color(self.palette.border)
            .px(px(4.0))
            .text_color(if showing_placeholder {
                self.palette.muted_text
            } else {
                self.palette.text
            });
        if let Some(aria_label) = node
            .props
            .get("aria-label")
            .or_else(|| node.props.get("ariaLabel"))
            .and_then(|value| value.as_str())
        {
            element = element.aria_label(SharedString::from(aria_label.to_owned()));
        }
        element = apply_styles(element, node, &self.palette);

        if let (Some(invocation), Some(dispatch)) = (change, self.dispatch.clone()) {
            element = element.on_key_down(move |event: &KeyDownEvent, window, cx| {
                let Some(next) = apply_keystroke(&value, event) else {
                    return;
                };
                let mut invocation = invocation.clone();
                invocation.args = vec![serde_json::Value::String(next)];
                dispatch(invocation, window, cx);
            });
        }

        element.child(label).into_any_element()
    }

    /// Paint the draw ops the host collected for this region. Positions are
    /// absolute within the region so ops never disturb sibling layout.
    fn render_display_list(&mut self, node: &VDomNode) -> AnyElement {
        let region_id = node
            .props
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Presence alone keeps the region alive for this frame: even a node
        // without fresh drawOps (renderer threw) must not be evicted while it
        // is still part of the VDOM.
        self.seen_display_regions
            .insert(SharedString::from(region_id.to_string()));
        if let Some(value) = node.props.get("drawOps") {
            match parse_display_list(value) {
                Ok(ops) => self.set_display_list(region_id.to_string(), ops),
                Err(error) => {
                    tracing::warn!(region_id, %error, "extension display list rejected");
                    self.display_lists.remove(region_id);
                }
            }
        }
        let mut container = apply_styles(div().relative(), node, &self.palette);
        let Some(ops) = self.display_lists.get(region_id) else {
            return container.into_any_element();
        };
        for op in ops {
            match op {
                DrawOp::DrawText { text, x, y, color } => {
                    let text_color = color
                        .as_deref()
                        .and_then(parse_color)
                        .unwrap_or(self.palette.text);
                    let label: SharedString = text.clone().into();
                    container = container.child(
                        div()
                            .absolute()
                            .left(px(*x))
                            .top(px(*y))
                            .text_color(text_color)
                            .child(label),
                    );
                }
                DrawOp::FillRect {
                    x,
                    y,
                    width,
                    height,
                    color,
                } => {
                    let fill = color
                        .as_deref()
                        .and_then(parse_color)
                        .unwrap_or(self.palette.selected_background);
                    container = container.child(
                        div()
                            .absolute()
                            .left(px(*x))
                            .top(px(*y))
                            .w(px(*width))
                            .h(px(*height))
                            .bg(fill),
                    );
                }
            }
        }
        container.into_any_element()
    }

    fn style_and_fill<E>(
        &mut self,
        element: E,
        node: &VDomNode,
        path: &mut ElementPath,
        cx: &mut App,
    ) -> E
    where
        E: Styled + ParentElement,
    {
        let mut element = apply_styles(element, node, &self.palette);
        element = apply_layout_default(element, node);
        for (index, child) in node.children.iter().enumerate() {
            match child {
                VDomChild::Text(text) => {
                    let label: SharedString = text.clone().into();
                    element = element.child(label);
                }
                VDomChild::Node(child_node) => {
                    path.push(index);
                    let child_element = self.render_node(child_node, path, cx);
                    path.pop();
                    element = element.child(child_element);
                }
            }
        }
        element
    }

    fn element_id(&self, node: &VDomNode, path: &ElementPath) -> ElementId {
        ElementId::Name(self.element_key(node, path).into())
    }

    /// Prefer the extension-supplied id so focus survives re-renders that
    /// reorder siblings; fall back to the tree path for anonymous nodes.
    fn element_key(&self, node: &VDomNode, path: &ElementPath) -> String {
        match node.props.get("id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => format!("vdom:{id}"),
            _ => path.key(),
        }
    }
}

/// Position of a node within the VDOM tree, used to synthesize stable element
/// ids for nodes the extension did not name.
struct ElementPath(Vec<usize>);

impl ElementPath {
    fn root() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, index: usize) {
        self.0.push(index);
    }

    fn pop(&mut self) {
        self.0.pop();
    }

    fn key(&self) -> String {
        let mut key = String::from("vdom");
        for index in &self.0 {
            key.push('-');
            key.push_str(&index.to_string());
        }
        key
    }
}

/// Default layout per element type: containers are flex rows, inline elements
/// hug their content so they do not stretch across the container.
fn apply_layout_default<E: Styled>(element: E, node: &VDomNode) -> E {
    if node.style.contains_key("flexDirection") {
        return element;
    }
    match node.element_type.as_str() {
        "span" | "text" | "button" => element.flex_none(),
        _ => element.flex(),
    }
}

fn apply_styles<E: Styled>(mut element: E, node: &VDomNode, palette: &VDomPalette) -> E {
    if let Some(direction) = node.style.get("flexDirection") {
        match direction.as_str() {
            "row" => element = element.flex().flex_row(),
            "column" => element = element.flex().flex_col(),
            _ => {}
        }
    }
    if let Some(wrap) = node.style.get("flexWrap") {
        match wrap.as_str() {
            "wrap" => element = element.flex_wrap(),
            "wrap-reverse" => element = element.flex_wrap_reverse(),
            _ => {}
        }
    }
    if let Some(gap) = node.style.get("gap").and_then(|v| parse_px(v)) {
        element = element.gap(px(gap));
    }
    if let Some(justify) = node.style.get("justifyContent") {
        match justify.as_str() {
            "space-between" => element = element.justify_between(),
            "space-around" => element = element.justify_around(),
            "center" => element = element.justify_center(),
            "flex-start" | "start" => element = element.justify_start(),
            "flex-end" | "end" => element = element.justify_end(),
            _ => {}
        }
    }
    if let Some(align) = node.style.get("alignItems") {
        match align.as_str() {
            "center" => element = element.items_center(),
            "flex-start" | "start" => element = element.items_start(),
            "flex-end" | "end" => element = element.items_end(),
            _ => {}
        }
    }
    if let Some(width) = node.style.get("width").and_then(|v| parse_px(v)) {
        element = element.w(px(width));
    }
    if let Some(height) = node.style.get("height").and_then(|v| parse_px(v)) {
        element = element.h(px(height));
    }

    // Each style is best-effort: an unparseable value is skipped rather than
    // failing the render, so one malformed property never blanks the chrome.
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
    if let Some(weight) = node.style.get("fontWeight") {
        if matches!(weight.as_str(), "bold" | "600" | "700" | "800" | "900") {
            element = element.font_weight(gpui::FontWeight::BOLD);
        }
    }

    apply_classes(element, node, palette)
}

/// Semantic classes extensions use for state that has no inline-style
/// equivalent; they resolve against the host theme rather than fixed colors.
fn apply_classes<E: Styled>(mut element: E, node: &VDomNode, palette: &VDomPalette) -> E {
    let Some(classes) = node.props.get("class").and_then(|v| v.as_str()) else {
        return element;
    };
    for class in classes.split_whitespace() {
        match class {
            "selected" | "active" => element = element.bg(palette.selected_background),
            "dim" | "muted" => element = element.text_color(palette.muted_text),
            "emphasis" => element = element.font_weight(gpui::FontWeight::BOLD),
            _ => {}
        }
    }
    element
}

/// Fold a keystroke into the field text. Returns None for keys that do not
/// change the text so the bridge does not dispatch a no-op command.
fn apply_keystroke(current: &str, event: &KeyDownEvent) -> Option<String> {
    match event.keystroke.key.as_str() {
        "backspace" => {
            let mut next = current.to_string();
            next.pop()?;
            Some(next)
        }
        _ => {
            let typed = event.keystroke.key_char.as_deref()?;
            if typed.is_empty()
                || event.keystroke.modifiers.control
                || event.keystroke.modifiers.platform
            {
                return None;
            }
            Some(format!("{current}{typed}"))
        }
    }
}

/// §5.4 parse a CSS hex color into an `Hsla`. Accepts `#rgb`, `#rrggbb` and
/// `#rrggbbaa`. Returns None for unparseable values so the bridge can skip the
/// style rather than panic.
pub fn parse_color(value: &str) -> Option<gpui::Hsla> {
    let hex = value.trim().strip_prefix('#')?;
    let expanded = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 | 8 => hex.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let parsed = u32::from_str_radix(&expanded, 16).ok()?;
    if expanded.len() == 8 {
        Some(gpui::rgba(parsed).into())
    } else {
        Some(gpui::rgb(parsed).into())
    }
}

/// §5.4 parse a CSS pixel length (`Npx`, or a bare number) into an f32.
/// Returns None for non-numeric or non-px values.
pub fn parse_px(value: &str) -> Option<f32> {
    let trimmed = value.trim();
    let num = trimmed.strip_suffix("px").unwrap_or(trimmed).trim();
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
        assert_eq!(node.props.get("id").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(node.children.len(), 1);
        match &node.children[0] {
            VDomChild::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected text child, got {other:?}"),
        }
    }

    #[test]
    fn parse_javascript_scalar_children() {
        let json = serde_json::json!({
            "type": "div",
            "children": [0, false, null, "text"]
        });
        let node = parse_vdom(&json).expect("parse scalar children");
        assert_eq!(node.children.len(), 3);
        assert!(matches!(&node.children[0], VDomChild::Text(text) if text == "0"));
        assert!(matches!(&node.children[1], VDomChild::Text(text) if text == "false"));
        assert!(matches!(&node.children[2], VDomChild::Text(text) if text == "text"));
    }

    #[test]
    fn parse_color_and_px_helpers_round_trip_core_styles() {
        // §5.4 the bridge must turn CSS-like style strings into GPUI values.
        // These helpers are the pure primitives apply_styles uses; without
        // them, color/background/fontSize/padding are silently ignored.
        assert_eq!(parse_color("#ff0000"), Some(gpui::rgb(0xff0000).into()));
        assert_eq!(parse_color("#f00"), Some(gpui::rgb(0xff0000).into()));
        assert_eq!(parse_color("#000000"), Some(gpui::rgb(0x000000).into()));
        assert_eq!(parse_color("not-a-color"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_px("14px"), Some(14.0));
        assert_eq!(parse_px("480px"), Some(480.0));
        assert_eq!(parse_px("8"), Some(8.0));
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
            other => panic!("expected node child, got {other:?}"),
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

    #[test]
    fn command_invocation_parses_both_shorthand_and_full_form() {
        // The built-in chrome extensions use both forms: command-palette emits
        // {command, args} for entry selection while simpler bindings emit a
        // bare command id.
        let full = serde_json::json!({
            "command": "z3rm.command-palette.select",
            "args": ["entry-1"]
        });
        assert_eq!(
            CommandInvocation::parse(&full),
            Some(CommandInvocation {
                command: "z3rm.command-palette.select".into(),
                args: vec![serde_json::Value::String("entry-1".into())],
            })
        );

        let shorthand = serde_json::json!("z3rm.command-palette.close");
        assert_eq!(
            CommandInvocation::parse(&shorthand),
            Some(CommandInvocation {
                command: "z3rm.command-palette.close".into(),
                args: Vec::new(),
            })
        );

        let scalar_arg = serde_json::json!({ "command": "c", "args": 5 });
        assert_eq!(
            CommandInvocation::parse(&scalar_arg).map(|i| i.args.len()),
            Some(1)
        );

        assert_eq!(CommandInvocation::parse(&serde_json::json!({})), None);
    }

    #[test]
    fn display_list_round_trips_the_status_bar_clock_payload() {
        // This is verbatim what extensions/z3rm-status-bar renderClock() emits.
        let json = serde_json::json!([
            { "op": "drawText", "text": "09:30", "x": 0, "y": 0 }
        ]);
        let ops = parse_display_list(&json).expect("parse display list");
        assert_eq!(
            ops,
            vec![DrawOp::DrawText {
                text: "09:30".into(),
                x: 0.0,
                y: 0.0,
                color: None,
            }]
        );
    }

    #[test]
    fn display_list_rejects_unknown_ops() {
        let json = serde_json::json!([{ "op": "drawUnicorn" }]);
        assert!(parse_display_list(&json).is_err());
    }

    #[test]
    fn display_list_parses_fill_rect_with_color() {
        let json = serde_json::json!([
            { "op": "fillRect", "x": 1, "y": 2, "width": 30, "height": 4, "color": "#112233" }
        ]);
        let ops = parse_display_list(&json).expect("parse display list");
        assert_eq!(
            ops,
            vec![DrawOp::FillRect {
                x: 1.0,
                y: 2.0,
                width: 30.0,
                height: 4.0,
                color: Some("#112233".into()),
            }]
        );
    }

    #[test]
    fn display_list_accepts_spec_short_rectangle_dimensions() {
        let json = serde_json::json!([
            { "op": "fillRect", "x": 1, "y": 2, "w": 30, "h": 4 }
        ]);
        let ops = parse_display_list(&json).expect("parse display list");
        assert_eq!(
            ops,
            vec![DrawOp::FillRect {
                x: 1.0,
                y: 2.0,
                width: 30.0,
                height: 4.0,
                color: None,
            }]
        );
    }

    #[test]
    fn renderer_stores_display_lists_by_region_id() {
        let mut renderer = VDomRenderer::new();
        assert_eq!(renderer.display_list("clock"), None);
        renderer.set_display_list(
            "clock",
            vec![DrawOp::DrawText {
                text: "09:30".into(),
                x: 0.0,
                y: 0.0,
                color: None,
            }],
        );
        assert_eq!(renderer.display_list("clock").map(<[DrawOp]>::len), Some(1));
    }

    fn display_list_node(id: &str) -> VDomNode {
        VDomNode {
            element_type: "display-list".into(),
            props: [
                ("id".to_string(), serde_json::json!(id)),
                (
                    "drawOps".to_string(),
                    serde_json::json!([
                        { "op": "drawText", "text": "t", "x": 0, "y": 0 }
                    ]),
                ),
            ]
            .into_iter()
            .collect(),
            style: BTreeMap::new(),
            children: Vec::new(),
        }
    }

    /// §5.4: a display-list region that stops appearing in the VDOM must be
    /// evicted from the native cache — otherwise a ticking extension that
    /// cycles region ids grows the renderer's state without bound (§5.2).
    #[gpui::test]
    fn render_frame_evicts_regions_missing_from_the_frame(cx: &mut gpui::TestAppContext) {
        let mut renderer = VDomRenderer::new();
        let clock = display_list_node("clock");

        // Frame 1 renders the clock region: ops are cached.
        let elements = cx.update(|cx| renderer.render_frame(&[clock.clone()], cx));
        assert_eq!(elements.len(), 1);
        assert_eq!(renderer.display_list("clock").map(<[DrawOp]>::len), Some(1));

        // Frame 2 no longer contains the clock: its cached ops must be gone.
        let empty = VDomNode {
            element_type: "div".into(),
            props: BTreeMap::new(),
            style: BTreeMap::new(),
            children: Vec::new(),
        };
        cx.update(|cx| renderer.render_frame(&[empty], cx));
        assert_eq!(
            renderer.display_list("clock"),
            None,
            "regions absent from the frame must be evicted"
        );
    }

    /// §5.4: regions that keep rendering survive eviction, and a frame with
    /// several top-level nodes evicts only what the whole frame dropped.
    #[gpui::test]
    fn render_frame_keeps_regions_still_rendered_and_evicts_per_frame(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut renderer = VDomRenderer::new();
        let clock = display_list_node("clock");
        let meter = display_list_node("meter");

        // Both regions in one multi-node frame.
        assert_eq!(
            cx.update(|cx| renderer.render_frame(&[clock.clone(), meter.clone()], cx))
                .len(),
            2
        );
        assert_eq!(renderer.display_list("clock").map(<[DrawOp]>::len), Some(1));
        assert_eq!(renderer.display_list("meter").map(<[DrawOp]>::len), Some(1));

        // Next frame drops the meter but keeps the clock: only the meter is
        // evicted, the clock cache survives and repaints.
        assert_eq!(
            cx.update(|cx| renderer.render_frame(&[clock.clone()], cx))
                .len(),
            1
        );
        assert_eq!(
            renderer.display_list("meter"),
            None,
            "dropped region must be evicted"
        );
        assert_eq!(
            renderer.display_list("clock").map(<[DrawOp]>::len),
            Some(1),
            "still-rendered region must keep its cache"
        );

        // The region reappears: the cache repopulates from the new drawOps.
        assert_eq!(
            cx.update(|cx| renderer.render_frame(&[clock.clone(), meter.clone()], cx))
                .len(),
            2
        );
        assert_eq!(renderer.display_list("meter").map(<[DrawOp]>::len), Some(1));
    }

    /// §5.4: the single-node `render()` entry treats its node as a frame, so
    /// stale regions are evicted there too.
    #[gpui::test]
    fn render_evicts_stale_regions_as_a_single_node_frame(cx: &mut gpui::TestAppContext) {
        let mut renderer = VDomRenderer::new();
        cx.update(|cx| renderer.render(&display_list_node("clock"), cx));
        assert_eq!(renderer.display_list("clock").map(<[DrawOp]>::len), Some(1));

        let plain = VDomNode {
            element_type: "span".into(),
            props: BTreeMap::new(),
            style: BTreeMap::new(),
            children: Vec::new(),
        };
        cx.update(|cx| renderer.render(&plain, cx));
        assert_eq!(
            renderer.display_list("clock"),
            None,
            "stale region must be evicted by the next single-node frame"
        );
    }

    #[test]
    fn keystroke_folding_matches_controlled_input_semantics() {
        // The bridge owns no text state: each keystroke produces the next full
        // value, which is dispatched so the extension can re-render from it.
        let typed = KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers: gpui::Modifiers::default(),
                key: "a".into(),
                key_char: Some("a".into()),
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(apply_keystroke("gi", &typed), Some("gia".to_string()));

        let backspace = KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers: gpui::Modifiers::default(),
                key: "backspace".into(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(apply_keystroke("gi", &backspace), Some("g".to_string()));
        assert_eq!(apply_keystroke("", &backspace), None);

        let chord = KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers: gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
                key: "c".into(),
                key_char: Some("c".into()),
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(apply_keystroke("gi", &chord), None);
    }

    #[test]
    fn element_key_prefers_extension_id_over_tree_path() {
        let renderer = VDomRenderer::new();
        let mut path = ElementPath::root();
        path.push(2);
        path.push(1);

        let named = VDomNode {
            element_type: "div".into(),
            props: [("id".to_string(), serde_json::json!("palette-query"))]
                .into_iter()
                .collect(),
            style: BTreeMap::new(),
            children: Vec::new(),
        };
        assert_eq!(renderer.element_key(&named, &path), "vdom:palette-query");

        let anonymous = VDomNode {
            element_type: "div".into(),
            props: BTreeMap::new(),
            style: BTreeMap::new(),
            children: Vec::new(),
        };
        assert_eq!(renderer.element_key(&anonymous, &path), "vdom-2-1");
    }
}
