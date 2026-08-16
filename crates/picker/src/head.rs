use std::sync::Arc;

use gpui::{App, Entity, FocusHandle, Focusable, prelude::*};
use ui::prelude::*;
use ui_input::{ErasedEditor, ErasedEditorEvent};

/// The head of a [`Picker`](crate::Picker).
pub(crate) enum Head {
    /// Picker has an editor that allows the user to filter the list.
    Editor(Arc<dyn ErasedEditor>),

    /// Picker has no head, it's just a list of items.
    Empty(Entity<EmptyHead>),
}

impl Head {
    pub fn editor<V: 'static>(
        placeholder_text: Arc<str>,
        mut edit_handler: impl FnMut(&mut V, &ErasedEditorEvent, &mut Window, &mut Context<V>) + 'static,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        let editor = (ui_input::ERASED_EDITOR_FACTORY.get().unwrap())(window, cx);

        editor.set_placeholder_text(placeholder_text.as_ref(), window, cx);
        let this = cx.weak_entity();
        editor
            .subscribe(
                Box::new(move |event, window, cx| {
                    this.update(cx, |this, cx| (edit_handler)(this, &event, window, cx))
                        .ok();
                }),
                window,
                cx,
            )
            .detach();
        Self::Editor(editor)
    }

    pub fn empty<V: 'static>(
        label: SharedString,
        blur_handler: impl FnMut(&mut V, &mut Window, &mut Context<V>) + 'static,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        let head = cx.new(|cx| EmptyHead::new(label, cx));
        cx.on_blur(&head.focus_handle(cx), window, blur_handler)
            .detach();
        Self::Empty(head)
    }
}

/// An invisible element that can hold focus.
pub(crate) struct EmptyHead {
    focus_handle: FocusHandle,
    /// What assistive technology announces when focus lands here. A
    /// non-searchable picker has no query field, so this invisible element
    /// holds focus, and unnamed it is announced as nothing at all.
    label: SharedString,
}

impl EmptyHead {
    fn new(label: SharedString, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            label,
        }
    }
}

impl Render for EmptyHead {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // An id and a role are what make this an accessibility node at all;
        // without them focus is discarded and the whole window is announced in
        // place of the picker.
        div()
            .id("picker-empty-head")
            .role(gpui::Role::Group)
            .aria_label(self.label.clone())
            .track_focus(&self.focus_handle(cx))
    }
}

impl Focusable for EmptyHead {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
