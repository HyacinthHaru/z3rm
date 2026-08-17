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
        AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement,
        Render, StatefulInteractiveElement as _, Styled as _, TestAppContext, Window, div,
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
                // The rows sit inside a tree, as they do in the settings
                // window: a `TreeItem` outside a `Tree` keeps its role and
                // loses the level and set semantics that go with containment.
                .child(
                    div()
                        .id("sessions")
                        .role(gpui::Role::Tree)
                        .aria_label("Sessions")
                        .child(TreeViewItem::new("session", "work").root_item(true).expanded(true))
                        .child(TreeViewItem::new("pane", "vim").toggle_state(true)),
                )
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
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "shared controls");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "shared controls");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "shared controls");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "shared controls");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "shared controls");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "shared controls");
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
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "modal headline");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "modal headline");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "modal headline");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "modal headline");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "modal headline");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "modal headline");

        assert!(
            tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .any(|node| node["aria"]["label"].as_str() == Some("Disconnected")),
            "a modal has to say what it is about: {json}"
        );
    }

    /// Greying a control out is the only thing that said it was disabled, and
    /// that says it to sighted users alone: everything else announced as an
    /// ordinary button, checkbox or switch that does nothing when pressed.
    #[gpui::test]
    fn a_disabled_control_says_so(cx: &mut TestAppContext) {
        use crate::{Button, Checkbox, Disableable as _};

        struct DisabledHost;
        impl Render for DisabledHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .child(Button::new("commit", "Commit").disabled(true))
                    .child(Button::new("cancel", "Cancel"))
                    .child(
                        Checkbox::new("amend", ToggleState::Unselected)
                            .label("Amend")
                            .disabled(true),
                    )
                    .child(
                        Switch::new("wrap", ToggleState::Selected)
                            .aria_label("Soft wrap")
                            .disabled(true),
                    )
            }
        }

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, _| DisabledHost);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the harness window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "disabled controls");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "disabled controls");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "disabled controls");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "disabled controls");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "disabled controls");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "disabled controls");
        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

        let by_name = |name: &str| {
            nodes
                .values()
                .find(|node| node["aria"]["label"] == name)
                .unwrap_or_else(|| panic!("no node named {name} in the tree: {json}"))
        };

        for name in ["Commit", "Amend", "Soft wrap"] {
            assert_eq!(
                by_name(name)["aria"]["disabled"].as_bool(),
                Some(true),
                "{name} is disabled and has to be announced that way: {json}"
            );
        }
        // The flag is only written when it is true, so an operable control
        // must not carry it at all.
        assert!(
            by_name("Cancel")["aria"]["disabled"].is_null(),
            "an operable button must not be announced as disabled: {json}"
        );
    }

    /// Each button in a toggle group is a `ButtonLike` holding a `Label`, and
    /// `ButtonLike` cannot name itself from a child, so every one of them —
    /// the extension filters, the git picker's tabs — announced as a bare
    /// "button" with nothing to tell them apart.
    #[gpui::test]
    fn a_toggle_button_group_names_its_buttons(cx: &mut TestAppContext) {
        use crate::{ToggleButtonGroup, ToggleButtonSimple};

        struct GroupHost;
        impl Render for GroupHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(
                    ToggleButtonGroup::single_row(
                        "extension-filters",
                        [
                            ToggleButtonSimple::new("All", |_, _, _| {}),
                            ToggleButtonSimple::new("Installed", |_, _, _| {}),
                        ],
                    )
                    .selected_index(1),
                )
            }
        }

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, _| GroupHost);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the harness window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "toggle button group");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "toggle button group");

        let mut buttons: Vec<(&str, Option<&str>)> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter(|node| node["aria"]["role"] == "Button")
            .map(|node| {
                (
                    node["aria"]["label"].as_str().unwrap_or_default(),
                    node["aria"]["toggled"].as_str(),
                )
            })
            .collect();
        buttons.sort();
        assert_eq!(
            buttons,
            vec![("All", None), ("Installed", Some("True"))],
            "each button says which one it is, and the chosen one says it is chosen"
        );
    }
}
