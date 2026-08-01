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

use extension_host::vdom_bridge::{self, VDomNode};
use gpui::{App, Context, Render, Window};
use workspace::{HideStatusItem, StatusItemView};

/// Status bar view that renders extension VDOM output.
pub struct ExtensionStatusBar {
    /// VDOM trees from loaded extensions, rendered left-to-right.
    vdom_nodes: Vec<VDomNode>,
}

impl ExtensionStatusBar {
    pub fn new() -> Self {
        Self {
            vdom_nodes: Vec::new(),
        }
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{ParentElement, Styled, div};

        let mut container = div().flex().flex_row().gap(gpui::px(8.0));

        for node in &self.vdom_nodes {
            container = container.child(vdom_bridge::vdom_to_element(node));
        }

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
