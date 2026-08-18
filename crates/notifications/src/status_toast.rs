use std::rc::Rc;

use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement};
use ui::{Tooltip, prelude::*};
use workspace::{ToastAction, ToastView};
use zed_actions::toast;

#[derive(RegisterComponent)]
pub struct StatusToast {
    icon: Option<Icon>,
    text: SharedString,
    action: Option<ToastAction>,
    show_dismiss: bool,
    auto_dismiss: bool,
    this_handle: Entity<Self>,
    focus_handle: FocusHandle,
}

impl StatusToast {
    pub fn new(
        text: impl Into<SharedString>,
        cx: &mut App,
        f: impl FnOnce(Self, &mut Context<Self>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            f(
                Self {
                    text: text.into(),
                    icon: None,
                    action: None,
                    show_dismiss: false,
                    auto_dismiss: true,
                    this_handle: cx.entity(),
                    focus_handle,
                },
                cx,
            )
        })
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn auto_dismiss(mut self, auto_dismiss: bool) -> Self {
        self.auto_dismiss = auto_dismiss;
        self
    }

    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        let this_handle = self.this_handle.clone();
        self.action = Some(ToastAction::new(
            label.into(),
            Some(Rc::new(move |window, cx| {
                this_handle.update(cx, |_, cx| {
                    cx.emit(DismissEvent);
                });
                f(window, cx);
            })),
        ));
        self
    }

    pub fn dismiss_button(mut self, show: bool) -> Self {
        self.show_dismiss = show;
        self
    }
}

impl Render for StatusToast {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_action_or_dismiss = self.action.is_some() || self.show_dismiss;

        h_flex()
            .id("status-toast")
            // The toast layer around this is a polite live region, but a live
            // region announces the text of what appears inside it, and the
            // message is a `Label`, which is not a node. Without a name here a
            // toast arrives saying only "Dismiss".
            .role(gpui::Role::Group)
            .aria_label(self.text.clone())
            .elevation_3(cx)
            .gap_2()
            .py_1p5()
            .pl_2p5()
            .map(|this| {
                if has_action_or_dismiss {
                    this.pr_1p5()
                } else {
                    this.pr_2p5()
                }
            })
            .flex_none()
            .bg(cx.theme().colors().surface_background)
            .shadow_lg()
            .when_some(self.icon.clone(), |this, icon| this.child(icon))
            .child(Label::new(self.text.clone()).color(Color::Default))
            .when_some(self.action.as_ref(), |this, action| {
                this.child(
                    Button::new(action.id.clone(), action.label.clone())
                        .tooltip(Tooltip::for_action_title(
                            action.label.clone(),
                            &toast::RunAction,
                        ))
                        .color(Color::Muted)
                        .when_some(action.on_click.clone(), |el, handler| {
                            el.on_click(move |_click_event, window, cx| handler(window, cx))
                        }),
                )
            })
            .when(self.show_dismiss, |this| {
                let handle = self.this_handle.clone();
                this.child(
                    IconButton::new("dismiss", IconName::Close)
                .aria_label("Dismiss")
                        .shape(ui::IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Dismiss"))
                        .on_click(move |_click_event, _window, cx| {
                            handle.update(cx, |_, cx| {
                                cx.emit(DismissEvent);
                            });
                        }),
                )
            })
    }
}

impl ToastView for StatusToast {
    fn action(&self) -> Option<ToastAction> {
        self.action.clone()
    }

    fn announcement(&self, _cx: &App) -> SharedString {
        // The action's label is part of it: a toast offering "Undo" that
        // announces only what happened leaves the user with no idea that
        // anything can be done about it before it disappears.
        match &self.action {
            Some(action) => format!("{}. {}", self.text, action.label).into(),
            None => self.text.clone(),
        }
    }

    fn auto_dismiss(&self) -> bool {
        self.auto_dismiss
    }
}

impl Focusable for StatusToast {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for StatusToast {}

impl Component for StatusToast {
    fn scope() -> ComponentScope {
        ComponentScope::Notification
    }

    fn description() -> &'static str {
        "A compact, transient toast used to surface status updates \
        such as completed operations or pending updates, with optional icon, \
        action, and dismiss affordances."
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        let text_example = StatusToast::new("Operation completed", cx, |this, _| this);

        let action_example = StatusToast::new("Update ready to install", cx, |this, _cx| {
            this.action("Restart", |_, _| {})
        });

        let dismiss_button_example =
            StatusToast::new("Dismiss Button", cx, |this, _| this.dismiss_button(true));

        let icon_example = StatusToast::new(
            "Nathan Sobo accepted your contact request",
            cx,
            |this, _| {
                this.icon(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
            },
        );

        let success_example = StatusToast::new("Pushed 4 changes to `zed/main`", cx, |this, _| {
            this.icon(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Success),
            )
        });

        let error_example = StatusToast::new(
            "git push: Couldn't find remote origin `iamnbutler/zed`",
            cx,
            |this, _cx| {
                this.icon(
                    Icon::new(IconName::XCircle)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .action("More Info", |_, _| {})
            },
        );

        let warning_example = StatusToast::new("You have outdated settings", cx, |this, _cx| {
            this.icon(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .action("More Info", |_, _| {})
        });

        let pr_example =
            StatusToast::new("`zed/new-notification-system` created!", cx, |this, _cx| {
                this.icon(
                    Icon::new(IconName::GitBranch)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .action("Open Pull Request", |_, cx| {
                    cx.open_url("https://github.com/")
                })
            });

        v_flex()
            .gap_6()
            .p_4()
            .children(vec![
                example_group_with_title(
                    "Basic Toast",
                    vec![
                        single_example("Text", div().child(text_example).into_any_element()),
                        single_example("Action", div().child(action_example).into_any_element()),
                        single_example("Icon", div().child(icon_example).into_any_element()),
                        single_example(
                            "Dismiss Button",
                            div().child(dismiss_button_example).into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Examples",
                    vec![
                        single_example("Success", div().child(success_example).into_any_element()),
                        single_example("Error", div().child(error_example).into_any_element()),
                        single_example("Warning", div().child(warning_example).into_any_element()),
                        single_example("Create PR", div().child(pr_example).into_any_element()),
                    ],
                )
                .vertical(),
            ])
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// A toast is transient status the user never navigates to, so it is only
    /// ever perceived if it is announced — and what a live region announces is
    /// the text of the nodes that appear inside it. The message is a `Label`,
    /// which is not a node, so the toast has to carry the message itself.
    #[gpui::test]
    fn a_toast_announces_what_it_says(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, cx| {
            let toast = StatusToast::new("Failed to restore notes.md", cx, |this, _| {
                this.dismiss_button(true)
            });
            ToastHost(toast)
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
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "status toast");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "status toast");
        gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "status toast");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "status toast");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "status toast");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "status toast");

        let announced = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter_map(|node| node["aria"]["label"].as_str())
            .any(|label| label == "Failed to restore notes.md");
        assert!(
            announced,
            "a toast that arrives saying only \"Dismiss\" has told the user nothing: {json}"
        );
    }

    struct ToastHost(Entity<StatusToast>);

    impl Render for ToastHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }
}
