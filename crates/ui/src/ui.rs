//! # UI – Zed UI Primitives & Components
//!
//! This crate provides a set of UI primitives and components that are used to build all of the elements in Zed's UI.
//!
//! ## Related Crates:
//!
//! - [`ui_macros`] - proc_macros support for this crate
//! - `ui_input` - the single line input component

pub mod component_prelude;
mod components;
pub mod prelude;
mod styles;
mod traits;
pub mod utils;

pub use components::*;
pub use prelude::*;
pub use styles::*;
pub use traits::animation_ext::*;

#[cfg(test)]
mod accessibility_tests {
    use crate::{
        ContextMenu, DropdownMenu, Modal, ModalHeader, Switch, ToggleState, Toggleable as _,
        TreeViewItem,
    };
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled as _,
        TestAppContext, Window, div,
    };


/// Three shared components carry roles and state that no test had ever read
/// back out of a frame: a switch's toggled state, a dropdown's expanded
/// state, and a tree row's level and selection. A role only becomes a node
/// when its element also has an id, so each of these is one missing `.id()`
/// away from announcing nothing.
#[gpui::test]
fn the_shared_controls_report_their_state(cx: &mut TestAppContext) {

    struct ControlsHost(Entity<ContextMenu>);

    impl Render for ControlsHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(Switch::new("mouse-reporting", ToggleState::Selected).aria_label("Mouse reporting"))
                .child(DropdownMenu::new("shell", "zsh", self.0.clone()))
                .child(TreeViewItem::new("session", "work").root_item(true).expanded(true))
                .child(TreeViewItem::new("pane", "vim").toggle_state(true))
        }
    }

    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });

    let window = cx.add_window(|window, cx| {
        let menu = ContextMenu::build(window, cx, |menu, _, _| menu.entry("zsh", None, |_, _| {}));
        ControlsHost(menu)
    });
    cx.activate_a11y(window.into());

    let json = cx
        .update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("the harness window is still open")
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

    let discarded = tree["frame"]["roles_without_id"]
        .as_array()
        .expect("the dump lists discarded roles");
    assert!(
        discarded.is_empty(),
        "these roles never became nodes for lack of an element id: {discarded:?}"
    );

    let by_role = |role: &str| {
        nodes
            .values()
            .find(|node| node["aria"]["role"] == role)
            .unwrap_or_else(|| panic!("no {role} in the tree: {json}"))
    };

    let switch = by_role("Switch");
    assert_eq!(switch["aria"]["label"].as_str(), Some("Mouse reporting"));
    assert_eq!(switch["aria"]["toggled"].as_str(), Some("True"));

    let dropdown = by_role("ComboBox");
    assert_eq!(dropdown["aria"]["label"].as_str(), Some("zsh"));
    assert_eq!(dropdown["aria"]["expanded"].as_bool(), Some(false));

    let mut rows: Vec<(&str, u64, bool)> = nodes
        .values()
        .filter(|node| node["aria"]["role"] == "TreeItem")
        .map(|node| {
            (
                node["aria"]["label"].as_str().unwrap_or_default(),
                node["aria"]["level"].as_u64().unwrap_or_default(),
                node["aria"]["selected"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(rows, vec![("vim", 2, true), ("work", 1, false)]);
    assert_eq!(
        by_role("TreeItem")["aria"]["role"].as_str(),
        Some("TreeItem")
    );
}

    /// A modal's heading is a plain `Headline`, which contributes no node, so
    /// a dialog built from one would be announced as an unnamed "dialog" with
    /// nothing readable inside it.
    #[gpui::test]
    fn a_modal_names_itself_with_its_headline(cx: &mut TestAppContext) {
        struct ModalHost;
        impl Render for ModalHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                Modal::new("host-modal", None)
                    .header(ModalHeader::new().headline("Disconnected"))
                    .child(div())
            }
        }

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, _| ModalHost);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the harness window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        assert!(
            tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .any(|node| node["aria"]["label"].as_str() == Some("Disconnected")),
            "a modal has to say what it is about: {json}"
        );
    }
}
