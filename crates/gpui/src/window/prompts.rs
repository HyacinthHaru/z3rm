use std::ops::Deref;

use futures::channel::oneshot;

use crate::{
    AnyView, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, PromptButton, PromptLevel,
    Render, StatefulInteractiveElement, Styled, div, opaque_grey, white,
};

use super::Window;
use crate::util::FluentBuilder as _;

/// The event emitted when a prompt's option is selected.
/// The usize is the index of the selected option, from the actions
/// passed to the prompt.
pub struct PromptResponse(pub usize);

/// A prompt that can be rendered in the window.
pub trait Prompt: EventEmitter<PromptResponse> + Focusable {}

impl<V: EventEmitter<PromptResponse> + Focusable> Prompt for V {}

/// A handle to a prompt that can be used to interact with it.
pub struct PromptHandle {
    sender: oneshot::Sender<usize>,
}

impl PromptHandle {
    pub(crate) fn new(sender: oneshot::Sender<usize>) -> Self {
        Self { sender }
    }

    /// Construct a new prompt handle from a view of the appropriate types
    pub fn with_view<V: Prompt + Render>(
        self,
        view: Entity<V>,
        window: &mut Window,
        cx: &mut App,
    ) -> RenderablePromptHandle {
        let mut sender = Some(self.sender);
        let previous_focus = window.focused(cx);
        let window_handle = window.window_handle();
        cx.subscribe(&view, move |_: Entity<V>, e: &PromptResponse, cx| {
            if let Some(sender) = sender.take() {
                sender.send(e.0).ok();
                window_handle
                    .update(cx, |_, window, cx| {
                        window.prompt.take();
                        if let Some(previous_focus) = &previous_focus {
                            window.focus(previous_focus, cx);
                        }
                    })
                    .ok();
            }
        })
        .detach();

        window.focus(&view.focus_handle(cx), cx);

        RenderablePromptHandle {
            view: Box::new(view),
        }
    }
}

/// A prompt handle capable of being rendered in a window.
pub struct RenderablePromptHandle {
    pub(crate) view: Box<dyn PromptViewHandle>,
}

/// Use this function in conjunction with [App::set_prompt_builder] to force
/// GPUI to always use the fallback prompt renderer.
pub fn fallback_prompt_renderer(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let renderer = cx.new(|cx| FallbackPromptRenderer {
        _level: level,
        message: message.to_string(),
        detail: detail.map(ToString::to_string),
        actions: actions.to_vec(),
        focus: cx.focus_handle(),
    });

    handle.with_view(renderer, window, cx)
}

/// The default GPUI fallback for rendering prompts, when the platform doesn't support it.
pub struct FallbackPromptRenderer {
    _level: PromptLevel,
    message: String,
    detail: Option<String>,
    actions: Vec<PromptButton>,
    focus: FocusHandle,
}

impl Render for FallbackPromptRenderer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = div()
            // A prompt asks the user to decide something. Without an id and a
            // role it produces no accessibility node, so focus is discarded and
            // the question itself is never announced.
            .id("fallback-prompt")
            .role(crate::Role::AlertDialog)
            .aria_modal()
            .aria_label(self.message.clone())
            // The detail line is where the consequence lives — what is about to
            // be lost or overwritten. It is plain text, so it is not a node,
            // and the dialog is announced the moment it takes focus.
            .when_some(self.detail.clone(), |this, detail| {
                this.aria_description(detail)
            })
            .cursor_default()
            .track_focus(&self.focus)
            .w_72()
            .bg(white())
            .rounded_lg()
            .overflow_hidden()
            .p_3()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_around()
                    .child(div().overflow_hidden().child(self.message.clone())),
            )
            .children(self.detail.clone().map(|detail| {
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_around()
                    .text_sm()
                    .mb_2()
                    .child(div().child(detail))
            }))
            .children(self.actions.iter().enumerate().map(|(ix, action)| {
                div()
                    .flex()
                    .flex_row()
                    .justify_around()
                    .border_1()
                    .border_color(opaque_grey(0.2, 0.5))
                    .mt_1()
                    .rounded_xs()
                    .cursor_pointer()
                    .text_sm()
                    .child(action.label().clone())
                    .id(ix)
                    // The label is a plain string child, which names nothing.
                    // Without a role these are clickable divs: the dialog is
                    // announced and then offers no way out of itself.
                    .role(crate::Role::Button)
                    .aria_label(action.label().clone())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(PromptResponse(ix));
                        cx.stop_propagation();
                    }))
            }));

        div()
            .size_full()
            .child(
                div()
                    .size_full()
                    .bg(opaque_grey(0.5, 0.6))
                    .absolute()
                    .top_0()
                    .left_0(),
            )
            .child(
                div()
                    .size_full()
                    .absolute()
                    .top_0()
                    .left_0()
                    .flex()
                    .flex_col()
                    .justify_around()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .justify_around()
                            .child(prompt),
                    ),
            )
    }
}

impl EventEmitter<PromptResponse> for FallbackPromptRenderer {}

impl Focusable for FallbackPromptRenderer {
    fn focus_handle(&self, _: &crate::App) -> FocusHandle {
        self.focus.clone()
    }
}

pub(crate) trait PromptViewHandle {
    fn any_view(&self) -> AnyView;
}

impl<V: Prompt + Render> PromptViewHandle for Entity<V> {
    fn any_view(&self) -> AnyView {
        self.clone().into()
    }
}

pub(crate) enum PromptBuilder {
    Default,
    Custom(
        Box<
            dyn Fn(
                PromptLevel,
                &str,
                Option<&str>,
                &[PromptButton],
                PromptHandle,
                &mut Window,
                &mut App,
            ) -> RenderablePromptHandle,
        >,
    ),
}

impl Deref for PromptBuilder {
    type Target = dyn Fn(
        PromptLevel,
        &str,
        Option<&str>,
        &[PromptButton],
        PromptHandle,
        &mut Window,
        &mut App,
    ) -> RenderablePromptHandle;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Default => &fallback_prompt_renderer,
            Self::Custom(f) => f.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{AppContext as _, Render, TestAppContext, Window, div};

    struct EmptyView;
    impl Render for EmptyView {
        fn render(&mut self, _: &mut Window, _: &mut crate::Context<Self>) -> impl crate::IntoElement {
            div()
        }
    }

    /// A prompt asks the user to decide something. Its root tracked focus
    /// without a role, so it produced no accessibility node: focus was
    /// discarded and the question was never announced.
    #[crate::test]
    fn an_open_prompt_is_announced_with_its_message(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                cx.set_prompt_builder(crate::fallback_prompt_renderer);
                let _receiver = window.prompt(
                    crate::PromptLevel::Warning,
                    "Discard unsaved changes?",
                    None,
                    &["Discard", "Cancel"],
                    cx,
                );
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the prompt window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        assert_eq!(
            tree["frame"]["focus_without_node"].as_str(),
            None,
            "the prompt carries a role now, so its focus must reach the tree"
        );
        let dialog = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .find(|node| node["aria"]["role"] == "AlertDialog")
            .expect("an open prompt must be reported as an alert dialog");
        assert_eq!(
            dialog["aria"]["label"].as_str(),
            Some("Discard unsaved changes?"),
            "the prompt must be announced by the question it asks"
        );
        assert_eq!(dialog["aria"]["modal"].as_bool(), Some(true));
    }

    /// The prompt is the last thing standing between a user and an
    /// irreversible decision. Its buttons were clickable `div`s with an id, a
    /// click handler and a plain string child — no role, so no node: the dialog
    /// announced the question and then offered nothing to answer it with. The
    /// consequence of answering wrongly lives in the detail line, which is
    /// plain text and so is not a node either.
    #[crate::test]
    fn the_prompt_offers_its_answers_and_says_what_they_cost(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| EmptyView);
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                cx.set_prompt_builder(crate::fallback_prompt_renderer);
                let _receiver = window.prompt(
                    crate::PromptLevel::Warning,
                    "Do you want to save changes?",
                    Some("Your changes will be lost."),
                    &["Save", "Cancel", "Discard"],
                    cx,
                );
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the prompt window is still open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        crate::test::a11y_checks::assert_interactive_nodes_are_named(&tree, "prompt");
        crate::test::a11y_checks::assert_names_are_distinguishable(&tree, "prompt");
        crate::test::a11y_checks::assert_clickable_elements_are_reachable(&tree, "prompt");
        crate::test::a11y_checks::assert_no_role_was_discarded(&tree, "prompt");
        crate::test::a11y_checks::assert_controls_have_area(&tree, "prompt");

        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");
        let mut buttons: Vec<&str> = nodes
            .values()
            .filter(|node| node["aria"]["role"] == "Button")
            .filter_map(|node| node["aria"]["label"].as_str())
            .collect();
        buttons.sort_unstable();
        assert_eq!(
            buttons,
            vec!["Cancel", "Discard", "Save"],
            "every answer the prompt accepts has to be reachable"
        );

        let dialog = nodes
            .values()
            .find(|node| node["aria"]["role"] == "AlertDialog")
            .expect("a prompt is an alert dialog");
        assert_eq!(
            dialog["aria"]["description"].as_str(),
            Some("Your changes will be lost."),
            "the cost of the decision is only written in the detail line"
        );
    }
}
