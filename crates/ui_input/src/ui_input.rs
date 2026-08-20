//! This crate provides UI components that can be used for form-like scenarios, such as a input and number field.
//!
//! It can't be located in the `ui` crate because it depends on `editor`.
//!
mod input_field;

use std::{
    any::Any,
    sync::{Arc, OnceLock},
};

use gpui::{FocusHandle, Subscription};
pub use input_field::*;
use ui::{AnyElement, App, Window};

pub trait ErasedEditor: 'static {
    fn text(&self, cx: &App) -> String;
    fn set_text(&self, text: &str, window: &mut Window, cx: &mut App);
    fn clear(&self, window: &mut Window, cx: &mut App);
    fn set_placeholder_text(&self, text: &str, window: &mut Window, _: &mut App);
    /// Name this field for assistive technology without drawing anything. A
    /// placeholder would also name it, at the cost of putting grey text in an
    /// empty field that nobody asked for.
    fn set_a11y_label(&self, label: &str, cx: &mut App);
    /// Supplementary text announced after the name — the question a prompt is
    /// asking, which is usually drawn beside the field rather than in it.
    fn set_a11y_description(&self, description: &str, cx: &mut App);
    fn move_selection_to_end(&self, window: &mut Window, _: &mut App);
    fn select_all(&self, window: &mut Window, cx: &mut App);
    fn set_masked(&self, masked: bool, window: &mut Window, cx: &mut App);
    fn set_read_only(&self, read_only: bool, cx: &mut App);
    /// Declare that the element around this editor already carries its role,
    /// name and text, so it must not report itself as a second input.
    fn set_a11y_wrapped(&self, wrapped: bool, cx: &mut App);
    fn set_multiline(&self, max_lines: Option<usize>, window: &mut Window, cx: &mut App);

    fn focus_handle(&self, cx: &App) -> FocusHandle;

    fn subscribe(
        &self,
        callback: Box<dyn FnMut(ErasedEditorEvent, &mut Window, &mut App) + 'static>,
        window: &mut Window,
        cx: &mut App,
    ) -> Subscription;
    fn render(&self, window: &mut Window, cx: &App) -> AnyElement;
    fn as_any(&self) -> &dyn Any;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ErasedEditorEvent {
    BufferEdited,
    Blurred,
}
pub static ERASED_EDITOR_FACTORY: OnceLock<fn(&mut Window, &mut App) -> Arc<dyn ErasedEditor>> =
    OnceLock::new();
