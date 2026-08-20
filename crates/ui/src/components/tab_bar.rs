use gpui::{AnyElement, ScrollHandle};
use smallvec::SmallVec;

use crate::Tab;
use crate::prelude::*;

#[derive(IntoElement, RegisterComponent)]
pub struct TabBar {
    id: ElementId,
    start_children: SmallVec<[AnyElement; 2]>,
    children: SmallVec<[AnyElement; 2]>,
    end_children: SmallVec<[AnyElement; 2]>,
    scroll_handle: Option<ScrollHandle>,
    /// Names the list of tabs.
    ///
    /// Required rather than a builder, and unlike most of this component's
    /// options it cannot be left out: the `TabList` role is set either way, so
    /// a bar with no name is a node announced as a bare "tab list". A window
    /// with more than one of them — pinned tabs beside unpinned ones — then
    /// offers a reader two lists it cannot tell apart.
    aria_label: SharedString,
}

impl TabBar {
    pub fn new(id: impl Into<ElementId>, aria_label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            start_children: SmallVec::new(),
            children: SmallVec::new(),
            end_children: SmallVec::new(),
            scroll_handle: None,
            aria_label: aria_label.into(),
        }
    }

    pub fn track_scroll(mut self, scroll_handle: &ScrollHandle) -> Self {
        self.scroll_handle = Some(scroll_handle.clone());
        self
    }

    pub fn start_children_mut(&mut self) -> &mut SmallVec<[AnyElement; 2]> {
        &mut self.start_children
    }

    pub fn start_child(mut self, start_child: impl IntoElement) -> Self
    where
        Self: Sized,
    {
        self.start_children_mut()
            .push(start_child.into_element().into_any());
        self
    }

    pub fn start_children(
        mut self,
        start_children: impl IntoIterator<Item = impl IntoElement>,
    ) -> Self
    where
        Self: Sized,
    {
        self.start_children_mut().extend(
            start_children
                .into_iter()
                .map(|child| child.into_any_element()),
        );
        self
    }

    pub fn end_children_mut(&mut self) -> &mut SmallVec<[AnyElement; 2]> {
        &mut self.end_children
    }

    pub fn end_child(mut self, end_child: impl IntoElement) -> Self
    where
        Self: Sized,
    {
        self.end_children_mut()
            .push(end_child.into_element().into_any());
        self
    }

    pub fn end_children(mut self, end_children: impl IntoIterator<Item = impl IntoElement>) -> Self
    where
        Self: Sized,
    {
        self.end_children_mut().extend(
            end_children
                .into_iter()
                .map(|child| child.into_any_element()),
        );
        self
    }
}

impl ParentElement for TabBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for TabBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .id(self.id)
            .group("tab_bar")
            .flex()
            .flex_none()
            .w_full()
            .h(Tab::container_height(cx))
            .bg(cx.theme().colors().tab_bar_background)
            .when(!self.start_children.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .gap(DynamicSpacing::Base04.rems(cx))
                        .px(DynamicSpacing::Base06.rems(cx))
                        .border_b_1()
                        .border_r_1()
                        .border_color(cx.theme().colors().border)
                        .children(self.start_children),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .h_full()
                    .overflow_x_hidden()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .border_b_1()
                            .border_color(cx.theme().colors().border),
                    )
                    .child(
                        h_flex()
                            .id("tabs")
                            // The element that directly parents the tabs, so
                            // the toolbar buttons in the start/end slots stay
                            // outside the set.
                            .role(gpui::Role::TabList)
                            .aria_label(self.aria_label)
                            .flex_grow_1()
                            .overflow_x_scroll()
                            .when_some(self.scroll_handle, |cx, scroll_handle| {
                                cx.track_scroll(&scroll_handle)
                            })
                            .children(self.children),
                    ),
            )
            .when(!self.end_children.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .gap(DynamicSpacing::Base04.rems(cx))
                        .px(DynamicSpacing::Base06.rems(cx))
                        .border_color(cx.theme().colors().border)
                        .border_b_1()
                        .border_l_1()
                        .children(self.end_children),
                )
            })
    }
}

impl Component for TabBar {
    fn scope() -> ComponentScope {
        ComponentScope::Navigation
    }

    fn name() -> &'static str {
        "TabBar"
    }

    fn description() -> &'static str {
        "A horizontal bar containing tabs for navigation between different views \
        or sections."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "Basic Usage",
                    vec![
                        single_example(
                            "Empty TabBar",
                            TabBar::new("empty_tab_bar", "Tabs").into_any_element(),
                        ),
                        single_example(
                            "With Tabs",
                            TabBar::new("tab_bar_with_tabs", "Tabs")
                                .child(Tab::new("tab1"))
                                .child(Tab::new("tab2"))
                                .child(Tab::new("tab3"))
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "With Start and End Children",
                    vec![single_example(
                        "Full TabBar",
                        TabBar::new("full_tab_bar", "Tabs")
                            .start_child(Button::new("start_button", "Start"))
                            .child(Tab::new("tab1"))
                            .child(Tab::new("tab2"))
                            .child(Tab::new("tab3"))
                            .end_child(Button::new("end_button", "End"))
                            .into_any_element(),
                    )],
                ),
            ])
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    struct TabBarHarness;

    impl Render for TabBarHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            TabBar::new("test-tab-bar", "Tabs")
                .start_child(Button::new("new-tab", "New"))
                .child(Tab::new("shell").aria_label("shell"))
                .child(Tab::new("logs").aria_label("logs").toggle_state(true))
        }
    }

    /// Tabs are how a multiplexer is navigated. Without a tab list and per-tab
    /// semantics a screen reader gets an unlabelled row with no indication that
    /// the tabs form a set, or which one is current.
    #[gpui::test]
    fn tabs_are_exposed_as_a_tab_list(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let window = cx.add_window(|_, _| TabBarHarness);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the harness window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "tab bar");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "tab bar");
        gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "tab bar");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "tab bar");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "tab bar");
        gpui::a11y_checks::assert_controls_have_area(&tree, "tab bar");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "tab bar");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "tab bar");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "tab bar");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "tab bar");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "tab bar");
        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

        let tab_list = nodes
            .iter()
            .find(|(_, node)| node["aria"]["role"] == "TabList")
            .map(|(id, _)| id.clone())
            .expect("the tab strip must be reported as a tab list");

        // Read the tabs from inside the list: the toolbar button in the start
        // slot must stay outside the set.
        let mut announced: Vec<(String, bool)> = nodes[&tab_list]["children"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().and_then(|id| nodes.get(id)))
            .filter(|node| node["aria"]["role"] == "Tab")
            .map(|node| {
                (
                    node["aria"]["label"].as_str().unwrap_or_default().to_string(),
                    node["aria"]["selected"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        announced.sort();

        assert_eq!(
            announced,
            vec![("logs".to_string(), true), ("shell".to_string(), false)],
            "each tab must carry its name and whether it is the current one"
        );

        // The start-slot button is part of the bar too, and a bare "button" is
        // just as unusable as a bare "tab".
        let unnamed: Vec<String> = nodes
            .values()
            .filter(|node| matches!(node["aria"]["role"].as_str(), Some("Button" | "Tab")))
            .filter(|node| node["aria"]["label"].as_str().is_none_or(str::is_empty))
            .map(|node| format!("{} ({})", node["aria"]["role"], node["element_id"]))
            .collect();
        assert!(
            unnamed.is_empty(),
            "these nodes are announced as a bare role: {unnamed:?}"
        );

        // Every tab must be inside the list, not merely present in the frame:
        // "tab 2 of 3" and the arrow-key conventions come from containment.
        let tabs_in_frame = nodes
            .values()
            .filter(|node| node["aria"]["role"] == "Tab")
            .count();
        assert_eq!(
            announced.len(),
            tabs_in_frame,
            "every tab in the frame must be inside the tab list"
        );
    }
}
