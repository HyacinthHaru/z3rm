//! §5.5 Extension status bar — renders QuickJS extension VDOM output
//! in the workspace status bar.
//!
//! Extensions return VDOM JSON from their render() calls. This view
//! converts the VDOM to GPUI elements via vdom_bridge and displays them
//! in the status bar's right section.
//!
//! The host (§5.2) loads extensions on a background thread and pushes the
//! resulting VDOM trees here via [`ExtensionStatusBar::set_vdom_nodes`];
//! `cx.notify()` triggers a re-render so the chrome updates without polling.

use extension_host::vdom_bridge::{CommandDispatch, DrawOp, VDomNode, VDomPalette, VDomRenderer};
use gpui::{App, Context, Render, Window};
use theme::ActiveTheme;
use workspace::{HideStatusItem, StatusItemView};

/// Status bar view that renders extension VDOM output.
pub struct ExtensionStatusBar {
    /// VDOM trees from loaded extensions, rendered left-to-right.
    vdom_nodes: Vec<VDomNode>,
    renderer: VDomRenderer,
}

impl ExtensionStatusBar {
    pub fn new() -> Self {
        Self {
            vdom_nodes: Vec::new(),
            renderer: VDomRenderer::new(),
        }
    }

    /// Route `onClick` / `onChange` descriptors back to the extension host so
    /// chrome interactions reach the command that owns them.
    pub fn set_dispatch(&mut self, dispatch: CommandDispatch) {
        self.renderer.set_dispatch(dispatch);
    }

    /// §5.4 Publish the draw ops a display-list renderer produced this tick.
    pub fn set_display_list(&mut self, region_id: &str, ops: Vec<DrawOp>, cx: &mut Context<Self>) {
        self.renderer.set_display_list(region_id.to_string(), ops);
        cx.notify();
    }

    /// §5.5 Replace the full VDOM set and request a re-render.
    ///
    /// Called by the extension host after it collects VDOM trees from loaded
    /// QuickJS extensions. `cx.notify()` schedules a repaint so the status bar
    /// reflects the new chrome on the next frame.
    pub fn set_vdom_nodes(&mut self, nodes: Vec<VDomNode>, cx: &mut Context<Self>) {
        self.vdom_nodes = nodes;
        cx.notify();
    }

    /// Number of VDOM trees currently held (used by host/tests to assert the
    /// host pushed a non-empty result without driving a full GPUI render).
    #[allow(dead_code)]
    pub fn vdom_node_count(&self) -> usize {
        self.vdom_nodes.len()
    }

    /// Borrow the held VDOM trees (used by the host to merge incremental
    /// extension results before the next push).
    #[allow(dead_code)]
    pub fn vdom_nodes(&self) -> &[VDomNode] {
        &self.vdom_nodes
    }
}

impl Render for ExtensionStatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{div, ParentElement, Styled};

        let colors = cx.theme().colors();
        self.renderer.set_palette(VDomPalette {
            text: colors.text,
            muted_text: colors.text_muted,
            background: colors.element_background,
            selected_background: colors.element_selection_background,
            border: colors.border,
        });

        let mut container = div().flex().flex_row().gap(gpui::px(8.0));
        let nodes = std::mem::take(&mut self.vdom_nodes);
        for node in &nodes {
            container = container.child(self.renderer.render(node, cx));
        }
        self.vdom_nodes = nodes;

        container
    }
}

impl StatusItemView for ExtensionStatusBar {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Extension status bar doesn't react to pane item changes
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
