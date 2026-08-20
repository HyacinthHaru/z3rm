//! Components used in multiple pickers

use gpui::Entity;
use project::Project;
use ui::{CommonAnimationExt, Tooltip, prelude::*};

pub fn project_scan_indicator(
    has_query: bool,
    project: &Entity<Project>,
    cx: &App,
) -> Option<impl IntoElement> {
    let is_project_scan_running = {
        let worktree_store = project.read(cx).worktree_store();
        !worktree_store.read(cx).initial_scan_completed()
    };
    (has_query && is_project_scan_running).then(|| {
        h_flex()
            .id("project-scan-indicator")
            // A spinner and a tooltip, neither of which is a node: while the
            // scan runs the list is incomplete, and a reader had no way to know
            // that the results in front of them are provisional. Announced
            // rather than left to be found: someone typing into the picker is
            // listening to the match count, not exploring the header, and the
            // count is exactly the number this qualifies. It appears once, when
            // the query becomes non-empty, and the node is gone when the scan
            // finishes, so there is nothing to un-say.
            .role(gpui::Role::Status)
            .aria_live(gpui::accesskit::Live::Polite)
            .aria_announcement("Project scan in progress, results are incomplete")
            .tooltip(Tooltip::text("Project Scan in Progress…"))
            .child(
                Icon::new(IconName::LoadCircle)
                    .color(Color::Accent)
                    .size(IconSize::Small)
                    .with_rotate_animation(2),
            )
    })
}
