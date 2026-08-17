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
