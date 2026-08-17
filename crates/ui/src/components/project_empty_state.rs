use crate::{Divider, DividerColor, KeyBinding, prelude::*};
use gpui::{ClickEvent, FocusHandle};

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct ProjectEmptyState {
    label: SharedString,
    focus_handle: FocusHandle,
    open_project_key_binding: KeyBinding,
    on_open_project: Option<ClickHandler>,
    on_clone_repo: Option<ClickHandler>,
}

impl ProjectEmptyState {
    pub fn new(
        label: impl Into<SharedString>,
        focus_handle: FocusHandle,
        open_project_key_binding: KeyBinding,
    ) -> Self {
        Self {
            label: label.into(),
            focus_handle,
            open_project_key_binding,
            on_open_project: None,
            on_clone_repo: None,
        }
    }

    pub fn on_open_project(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_project = Some(Box::new(handler));
        self
    }

    pub fn on_clone_repo(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_clone_repo = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for ProjectEmptyState {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let id = format!("empty-state-{}", self.label);
        let label = format!("Choose one of the options below to use the {}", self.label);

        v_flex()
            .id(id)
            // The explanation is a plain label and contributes no node of its
            // own, and this element takes focus, so without a role the empty
            // panel discards that focus and announces the whole window.
            .role(gpui::Role::Group)
            .aria_label(label.clone())
            .p_4()
            .size_full()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle)
            .child(
                v_flex()
                    .w_48()
                    .max_w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_center()
                            .mb_2()
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(
                        // Both the project and git panels show this state at
                        // once when no folder is open, so the two buttons would
                        // otherwise appear twice under the same name.
                        Button::new("open_project", "Open Project")
                            .aria_label(format!("Open Project: {}", self.label))
                            .full_width()
                            .key_binding(self.open_project_key_binding)
                            .when_some(self.on_open_project, |button, handler| {
                                button.on_click(handler)
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Divider::horizontal().color(DividerColor::Border))
                            .child(Label::new("or").size(LabelSize::XSmall).color(Color::Muted))
                            .child(Divider::horizontal().color(DividerColor::Border)),
                    )
                    .child(
                        Button::new("clone_repo", "Clone Repository")
                            .aria_label(format!("Clone Repository: {}", self.label))
                            .full_width()
                            .when_some(self.on_clone_repo, |button, handler| {
                                button.on_click(handler)
                            }),
                    ),
            )
    }
}
