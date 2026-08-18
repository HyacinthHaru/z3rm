use askpass::EncryptedPassword;
use editor::Editor;
use futures::channel::oneshot;
use gpui::{AppContext, DismissEvent, Entity, EventEmitter, Focusable, Styled};
use ui::{
    ActiveTheme, AnyElement, App, Button, Clickable, Color, Context, DynamicSpacing, Headline,
    HeadlineSize, Icon, IconName, IconSize, InteractiveElement, IntoElement, Label, LabelCommon,
    LabelSize, ParentElement, Render, SharedString, StyledExt, StyledTypography, Window, div,
    h_flex, v_flex,
};
use util::maybe;
use workspace::ModalView;
use zeroize::Zeroize;

pub(crate) struct AskPassModal {
    operation: SharedString,
    prompt: SharedString,
    editor: Entity<Editor>,
    tx: Option<oneshot::Sender<EncryptedPassword>>,
}

impl EventEmitter<DismissEvent> for AskPassModal {}
impl ModalView for AskPassModal {
    fn a11y_name(&self, _cx: &App) -> Option<SharedString> {
        // The same string the modal shows as its headline.
        Some(self.operation.clone())
    }
}
impl Focusable for AskPassModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl AskPassModal {
    pub fn new(
        operation: SharedString,
        prompt: SharedString,
        tx: oneshot::Sender<EncryptedPassword>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            if prompt.contains("yes/no") || prompt.contains("Username") {
                editor.set_masked(false, cx);
                editor.set_a11y_label("Answer");
            } else {
                editor.set_masked(true, cx);
                editor.set_a11y_label("Password");
            }
            // The prompt names the host or key being authenticated against and
            // is the only place it appears; as plain text it is not a node, so
            // without this the field asks for a secret without saying for what.
            editor.set_a11y_description(prompt.clone());
            editor
        });
        Self {
            operation,
            prompt,
            editor,
            tx: Some(tx),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        maybe!({
            let tx = self.tx.take()?;
            let mut text = self.editor.update(cx, |this, cx| {
                let text = this.text(cx);
                this.clear(window, cx);
                text
            });
            let pw = askpass::EncryptedPassword::try_from(text.as_ref()).ok()?;
            text.zeroize();
            tx.send(pw).ok();
            Some(())
        });

        cx.emit(DismissEvent);
    }

    fn render_hint(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let color = cx.theme().status().info_background;
        if (self.prompt.contains("Password") || self.prompt.contains("Username"))
            && self.prompt.contains("github.com")
        {
            return Some(
            div()
                .p_2()
                .bg(color)
                .border_t_1()
                .border_color(cx.theme().status().info_border)
                .child(
                    h_flex().gap_2()
                        .child(
                            Icon::new(IconName::Github).size(IconSize::Small)
                        )
                        .child(
                            Label::new("You may need to configure git for Github.")
                                .size(LabelSize::Small),
                        )
                        .child(Button::new("learn-more", "Learn more").color(Color::Accent).label_size(LabelSize::Small).on_click(|_, _, cx| {
                            cx.open_url("https://docs.github.com/en/get-started/git-basics/set-up-git#authenticating-with-github-from-git")
                        })),
                )
                .into_any_element(),
        );
        }
        None
    }
}

impl Render for AskPassModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PasswordPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .size_full()
            .child(
                h_flex()
                    .font_buffer(cx)
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pt(DynamicSpacing::Base08.rems(cx))
                    .pb(DynamicSpacing::Base04.rems(cx))
                    .rounded_t_sm()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(h_flex().gap_1().overflow_x_hidden().child(
                        div().max_w_96().overflow_x_hidden().text_ellipsis().child(
                            Headline::new(self.operation.clone()).size(HeadlineSize::XSmall),
                        ),
                    )),
            )
            .child(
                div()
                    .font_buffer(cx)
                    .text_buffer(cx)
                    .py_2()
                    .px_3()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .size_full()
                    .overflow_hidden()
                    .child(self.prompt.clone())
                    .child(self.editor.clone()),
            )
            .children(self.render_hint(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// The modal asks for a secret. Everything that says *which* secret — the
    /// repository, the host, the key file — lives in the prompt, and the prompt
    /// is plain text, which is not an accessibility node. Focus lands in the
    /// field the moment the modal opens, so if the field does not carry that
    /// detail nothing announces it at all.
    #[gpui::test]
    fn the_password_field_says_what_it_is_for(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let (tx, _rx) = oneshot::channel();
        let window = cx.add_window(|window, cx| {
            AskPassModal::new(
                "git push".into(),
                "Password for 'https://ada@github.com':".into(),
                tx,
                window,
                cx,
            )
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
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "askpass modal");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "askpass modal");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "askpass modal");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "askpass modal");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "askpass modal");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "askpass modal");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "askpass modal");
        gpui::a11y_checks::assert_controls_have_area(&tree, "askpass modal");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "askpass modal");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "askpass modal");

        let field = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .find(|node| node["aria"]["role"] == "TextInput")
            .unwrap_or_else(|| panic!("no text input in the tree: {json}"));
        assert_eq!(field["aria"]["label"].as_str(), Some("Password"));
        assert_eq!(
            field["aria"]["description"].as_str(),
            Some("Password for 'https://ada@github.com':"),
            "the field says which host is asking"
        );
    }
}
