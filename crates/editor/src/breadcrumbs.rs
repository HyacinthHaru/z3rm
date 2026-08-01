//! Stub of the removed `breadcrumbs` crate.
use gpui::{Font, SharedString};
use language::HighlightedText;

pub struct RenderBreadcrumbText(
    pub fn(Vec<HighlightedText>, Option<Font>, Option<gpui::AnyElement>) -> gpui::AnyElement,
);

#[derive(Clone, Debug)]
pub struct Breadcrumb {
    pub text: SharedString,
}
