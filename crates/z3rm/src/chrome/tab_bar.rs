//! Native tab bar (spec §5.1). Day 0 baseline, replaced by QuickJS extensions.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div,
};

pub struct TabBar {
    focus_handle: FocusHandle,
    tabs: Vec<TabEntry>,
    active_index: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct TabEntry {
    pub id: String,
    pub title: SharedString,
}

impl TabBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            tabs: Vec::new(),
            active_index: None,
        }
    }

    pub fn set_tabs(&mut self, tabs: Vec<TabEntry>, active: Option<usize>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        self.active_index = active;
        cx.notify();
    }
}

impl Focusable for TabBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for TabBar {}

impl Render for TabBar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = div()
            .flex()
            .flex_row()
            .gap_1()
            .px_2()
            .py_1()
            .bg(gpui::rgb(0x1e1e2e))
            .border_b_1()
            .border_color(gpui::rgb(0x45475a));

        for (i, tab) in self.tabs.iter().enumerate() {
            let bg = if self.active_index == Some(i) {
                gpui::rgb(0x313244)
            } else {
                gpui::rgb(0x1e1e2e)
            };
            bar = bar.child(
                div()
                    .px_3()
                    .py_1()
                    .bg(bg)
                    .text_color(gpui::rgb(0xcdd6f4))
                    .child(tab.title.clone()),
            );
        }

        bar = bar.child(
            div()
                .px_2()
                .py_1()
                .text_color(gpui::rgb(0xa6adc8))
                .child("+"),
        );

        bar
    }
}
