//! Native status bar (spec §5.1). Day 0 baseline, replaced by QuickJS extensions.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div,
};

pub struct StatusBar {
    focus_handle: FocusHandle,
    session_name: SharedString,
    clock_text: SharedString,
}

impl StatusBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            session_name: "default".into(),
            clock_text: "".into(),
        }
    }

    pub fn set_session_name(&mut self, name: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.session_name = name.into();
        cx.notify();
    }

    pub fn set_clock(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.clock_text = text.into();
        cx.notify();
    }
}

impl Focusable for StatusBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for StatusBar {}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .justify_between()
            .px_4()
            .py_1()
            .bg(gpui::rgb(0x1e1e2e))
            .text_color(gpui::rgb(0xcdd6f4))
            .child(self.session_name.clone())
            .child(self.clock_text.clone())
    }
}
