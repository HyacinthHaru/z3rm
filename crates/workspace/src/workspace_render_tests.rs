//! Focused regression tests for workspace render traps:
//! - every `BottomDockLayout` variant must render the center pane group
//!   (the `Stacked`/`SideBySide` fallback once rendered only the bottom dock,
//!   making the whole editor area disappear);
//! - a pane must keep rendering its content when its project handle is
//!   transiently unavailable instead of blanking out;
//! - `run_create_worktree_tasks` must drive the real worktree scan flow and
//!   surface failures as workspace notifications rather than no-op'ing.

use crate::item::test::TestItem;
use crate::item::PreviewTabsSettings;
use crate::{BottomDockLayout, MultiWorkspace, TabBarSettings, WorkspaceSettings};
use fs::{FakeFs, Fs};
use gpui::{AppContext, TestAppContext};
use project::{project_settings::ProjectSettings, Project, WorktreeSettings};
use serde_json::json;
use settings::{Settings, SettingsStore};
use util::path;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        ProjectSettings::register(cx);
        WorktreeSettings::register(cx);
        WorkspaceSettings::register(cx);
        TabBarSettings::register(cx);
        PreviewTabsSettings::register(cx);
    });
}

#[gpui::test]
async fn test_all_bottom_dock_layouts_render_center(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    for layout in [
        BottomDockLayout::Full,
        BottomDockLayout::LeftAligned,
        BottomDockLayout::RightAligned,
        BottomDockLayout::Contained,
        BottomDockLayout::Stacked,
        BottomDockLayout::SideBySide,
    ] {
        cx.update(|_window, cx| {
            let mut settings = WorkspaceSettings::get_global(cx).clone();
            settings.bottom_dock_layout = layout;
            WorkspaceSettings::override_global(settings, cx);
        });
        workspace.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("workspace-center").is_some(),
            "bottom dock layout {layout:?} must render the center pane group"
        );
    }
}

#[gpui::test]
async fn test_pane_renders_content_with_dead_project_handle(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(
            Box::new(cx.new(TestItem::new)),
            true,
            true,
            None,
            window,
            cx,
        );
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pane-content").is_some(),
        "pane content must render while the project handle is live"
    );

    // Simulate a transient loss of the project handle: the pane must keep
    // rendering its active item instead of collapsing to an empty div.
    let dead_project = {
        let temp = Project::test(fs.clone(), [], cx).await;
        temp.downgrade()
    };
    assert!(
        dead_project.upgrade().is_none(),
        "temporary project must be dropped"
    );
    pane.update(cx, |pane, cx| {
        pane.set_project_for_test(dead_project);
        cx.notify();
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("pane-content").is_some(),
        "pane must keep rendering its content while the project handle is unavailable"
    );
}

#[gpui::test]
async fn test_run_create_worktree_tasks_keeps_scanned_worktrees(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.run_create_worktree_tasks(window, cx);
    });
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.visible_worktrees(cx).count(),
            1,
            "the new worktree must remain registered after setup"
        );
        assert!(
            workspace
                .project()
                .read(cx)
                .worktree_store()
                .read(cx)
                .initial_scan_completed(),
            "setup must run after the initial scan completes"
        );
        assert!(
            workspace.notification_ids().is_empty(),
            "successful setup must not surface error notifications"
        );
    });
}

#[gpui::test]
async fn test_run_create_worktree_tasks_surfaces_missing_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.run_create_worktree_tasks(window, cx);
    });
    // Remove the worktree before the spawned setup task observes the completed
    // scan; the failure must surface as a workspace notification.
    workspace.update(cx, |workspace, cx| {
        workspace.project().update(cx, |project, cx| {
            let worktree_id = project
                .worktree_store()
                .read(cx)
                .worktrees()
                .next()
                .expect("worktree must exist before removal")
                .read(cx)
                .id();
            project
                .worktree_store()
                .update(cx, |store, cx| store.remove_worktree(worktree_id, cx));
        });
    });
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, _| {
        assert!(
            !workspace.notification_ids().is_empty(),
            "a worktree disappearing during setup must surface a notification"
        );
    });
}
