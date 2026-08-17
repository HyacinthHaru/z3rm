use gpui::{AnyElement, prelude::*};
use smallvec::SmallVec;
use ui::prelude::*;

#[derive(IntoElement)]
pub struct ExtensionCard {
    overridden_by_dev_extension: bool,
    /// What this card is about. Its contents are labels — name, version,
    /// authors, description — and a label contributes no accessibility node,
    /// so without this a card reaches a reader as its buttons and nothing else.
    aria_label: Option<SharedString>,
    children: SmallVec<[AnyElement; 2]>,
}

impl ExtensionCard {
    pub fn new() -> Self {
        Self {
            overridden_by_dev_extension: false,
            aria_label: None,
            children: SmallVec::new(),
        }
    }

    /// Names the card, which is also what makes it reported as a group at all.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    pub fn overridden_by_dev_extension(mut self, overridden: bool) -> Self {
        self.overridden_by_dev_extension = overridden;
        self
    }
}

impl ParentElement for ExtensionCard {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for ExtensionCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Keyed by the name so two cards cannot collide on one id. Always
        // stateful so both arms have the same type; the role and the name only
        // appear when the caller supplied one.
        let label = self.aria_label.clone();
        div()
            .id(ElementId::Name(
                label.clone().unwrap_or_else(|| "extension-card".into()),
            ))
            .w_full()
            .when_some(label, |this, label| {
                this.role(gpui::Role::Group).aria_label(label)
            })
            .child(
            v_flex()
                .mt_4()
                .w_full()
                .h(rems_from_px(110.))
                .p_3()
                .gap_2()
                .bg(cx.theme().colors().elevated_surface_background.opacity(0.5))
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .rounded_md()
                .children(self.children)
                .when(self.overridden_by_dev_extension, |card| {
                    card.child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .left_0()
                            .block_mouse_except_scroll()
                            .cursor_default()
                            .size_full()
                            .justify_center()
                            .bg(cx.theme().colors().elevated_surface_background.alpha(0.8))
                            .child(Label::new("Overridden by dev extension.")),
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, Window};
    use ui::Label;

    /// A card's contents are labels — name, version, authors, description — and
    /// a label contributes no accessibility node, so a card reaches a reader as
    /// its buttons and nothing else unless it names itself.
    #[gpui::test]
    fn a_named_card_is_a_group_and_an_unnamed_one_is_not(cx: &mut TestAppContext) {
        struct CardHost;
        impl Render for CardHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .child(
                        ExtensionCard::new()
                            .aria_label("Vim Mode, v0.1.2, by Zed Industries")
                            .child(Label::new("Vim Mode")),
                    )
                    // Naming is opt-in: a card the caller has nothing to say
                    // about stays out of the tree rather than announcing an
                    // empty group.
                    .child(ExtensionCard::new().child(Label::new("Anonymous")))
            }
        }

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, _| CardHost);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the harness window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "extension cards");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "extension cards");

        let groups: Vec<&str> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter(|node| node["aria"]["role"] == "Group")
            .filter_map(|node| node["aria"]["label"].as_str())
            .collect();
        assert_eq!(
            groups,
            vec!["Vim Mode, v0.1.2, by Zed Industries"],
            "the named card is a group and the unnamed one adds nothing"
        );
    }
}
