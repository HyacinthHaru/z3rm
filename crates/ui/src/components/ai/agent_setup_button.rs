use crate::prelude::*;
use gpui::{ClickEvent, SharedString};

#[derive(IntoElement, RegisterComponent)]
pub struct AgentSetupButton {
    id: ElementId,
    icon: Option<Icon>,
    name: Option<SharedString>,
    state: Option<AnyElement>,
    disabled: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl AgentSetupButton {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            icon: None,
            name: None,
            state: None,
            disabled: false,
            on_click: None,
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn name(mut self, name: impl Into<SharedString>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn state(mut self, element: impl IntoElement) -> Self {
        self.state = Some(element.into_any_element());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl Component for AgentSetupButton {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn description() -> &'static str {
        "A large, two-section button used in agent onboarding flows \
        to launch the setup of a provider or tool, showing an icon, name, \
        and current setup state."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        single_example(
            "Default",
            AgentSetupButton::new("preview")
                .icon(Icon::new(IconName::ZedAgent))
                .name("Zed Agent")
                .into_any_element(),
        )
        .into_any_element()
    }
}

impl RenderOnce for AgentSetupButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_clickable = !self.disabled && self.on_click.is_some();
        let announced_name = self.name.clone().filter(|_| is_clickable);

        let has_top_section = self.icon.is_some() || self.name.is_some();
        let top_section = has_top_section.then(|| {
            h_flex()
                .p_1p5()
                .gap_1()
                .justify_center()
                .when_some(self.icon, |this, icon| this.child(icon))
                .when_some(self.name, |this, name| {
                    this.child(Label::new(name).size(LabelSize::Small))
                })
        });

        let bottom_section = self.state.map(|state_element| {
            h_flex()
                .p_0p5()
                .h_full()
                .justify_center()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().element_background.opacity(0.5))
                .child(state_element)
        });

        // A card built out of an icon, a `Label` and a state element, none of
        // which is a node, wrapped in a click. Without a role it produced no
        // node at all, so setting the agent up was reachable by mouse only.
        // Named only when it has a name and a click: an inert card with
        // nothing to say stays out of the tree rather than announcing an empty
        // button.
        v_flex()
            .id(self.id)
            .when_some(announced_name, |this, name| {
                this.role(gpui::Role::Button).aria_label(name)
            })
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .when(is_clickable, |this| {
                this.cursor_pointer().hover(|style| {
                    style
                        .bg(cx.theme().colors().element_hover)
                        .border_color(cx.theme().colors().border)
                })
            })
            .when_some(top_section, |this, section| this.child(section))
            .when_some(bottom_section, |this, section| this.child(section))
            .when_some(self.on_click.filter(|_| is_clickable), |this, on_click| {
                this.on_click(on_click)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// The card is an icon, a `Label` and a state element wrapped in a click.
    /// None of those is a node, so without a role the whole thing is absent
    /// from the tree and setting the agent up is a mouse-only affordance.
    #[gpui::test]
    fn a_setup_card_that_does_something_is_named(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        struct Host;
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                div()
                    .child(
                        AgentSetupButton::new("connect")
                            .name("Connect an agent")
                            .on_click(|_, _, _| {}),
                    )
                    // Disabled, so it does nothing and says nothing: a card the
                    // user cannot act on has no business in the tree.
                    .child(
                        AgentSetupButton::new("soon")
                            .name("Coming soon")
                            .disabled(true)
                            .on_click(|_, _, _| {}),
                    )
            }
        }

        let window = cx.add_window(|_, _| Host);
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the harness window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "agent setup");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "agent setup");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "agent setup");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "agent setup");

        let buttons: Vec<&str> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter(|node| node["aria"]["role"] == "Button")
            .filter_map(|node| node["aria"]["label"].as_str())
            .collect();
        assert_eq!(
            buttons,
            vec!["Connect an agent"],
            "the card that acts is named; the one that cannot act stays out"
        );
    }
}
