//! Focused regression tests for workspace render traps:
//! - every `BottomDockLayout` variant must render the center pane group
//!   (the `Stacked`/`SideBySide` fallback once rendered only the bottom dock,
//!   making the whole editor area disappear);
//! - a pane must keep rendering its content when its project handle is
//!   transiently unavailable instead of blanking out;
//! - `run_create_worktree_tasks` must drive the real worktree scan flow and
//!   surface failures as workspace notifications rather than no-op'ing.

use crate::item::PreviewTabsSettings;
use crate::item::test::TestItem;
use crate::{BottomDockLayout, MultiWorkspace, TabBarSettings, WorkspaceSettings};
use fs::{FakeFs, Fs};
use gpui::{AppContext, TestAppContext};
use project::{Project, WorktreeSettings, project_settings::ProjectSettings};
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
async fn test_pane_renders_fallback_when_empty_with_dead_project_handle(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("pane-content").is_some(),
        "pane placeholder must render while the project handle is live"
    );

    // Empty pane + dead project handle: the pane must render a non-empty,
    // actionable fallback that reports the unavailable context instead of
    // collapsing to a blank div.
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
        "pane must keep rendering a placeholder while the project handle is unavailable"
    );
    assert!(
        cx.debug_bounds("pane-content-unavailable").is_some(),
        "the unavailable-context fallback must render when the project handle is gone"
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

/// Region navigation ([`crate::FocusNextPart`]) is the discoverable way to move
/// between landmarks, and in a mux window the session tree is the primary one.
/// It was absent from the rotation, so F6 cycled title bar, docks, editor and
/// status bar and never reached it.
#[gpui::test]
async fn test_region_navigation_reaches_the_session_sidebar(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let sidebar_focus_handle = cx.update(|_window, cx| cx.focus_handle());
    workspace.update(cx, |workspace, _| {
        workspace.set_sidebar_focus_handle(Some(sidebar_focus_handle.clone()));
    });
    cx.run_until_parked();

    // A rotation is at most one lap; the exact index depends on which docks
    // happen to be open, which this test deliberately does not fix.
    let mut reached = false;
    for _ in 0..8 {
        reached = workspace.update_in(cx, |workspace, window, cx| {
            workspace.move_part_focus(true, window, cx);
            // Checked inside the same update: this handle belongs to no
            // rendered element, so it cannot hold focus across a frame.
            window
                .focused(cx)
                .is_some_and(|handle| handle == sidebar_focus_handle)
        });
        if reached {
            break;
        }
        cx.run_until_parked();
    }
    assert!(
        reached,
        "FocusNextPart must reach the session sidebar within one full rotation"
    );
}

/// Notifications appear away from wherever the user is working and several can
/// stack up. Without a live region the container is just a `div`, so nothing is
/// ever announced and a screen-reader user has no idea anything happened.
#[gpui::test]
async fn test_notifications_are_announced_as_a_live_region(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    workspace.update(cx, |workspace, cx| {
        struct TestNotification;
        workspace.show_notification(crate::NotificationId::unique::<TestNotification>(), cx, |cx| {
            cx.new(|cx| {
                crate::notifications::simple_message_notification::MessageNotification::new(
                    "the mux server went away",
                    cx,
                )
            })
        });
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "notifications");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "notifications");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "notifications");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "notifications");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "notifications");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "notifications");

    let log = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .find(|node| node["aria"]["role"] == "Log")
        .expect("the notification stack must be reported as a log");
    assert_eq!(
        log["aria"]["live"].as_str(),
        Some("Polite"),
        "a notification that arrives on its own has to be announced"
    );
    assert_eq!(log["aria"]["label"].as_str(), Some("Notifications"));
    // A live region announces what is inside it, and the notification body is
    // plain text that contributes no node of its own, so the region can exist
    // and still have nothing to read out.
    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");
    let announced: Vec<&str> = log["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().and_then(|id| nodes.get(id)))
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    assert!(
        announced.contains(&"the mux server went away"),
        "the region has to contain what the notification says: {announced:?}"
    );
}

/// A node that advertises an action but cannot be operated by it reads as
/// working right up until someone tries. GPUI advertises `Click` for anything
/// with a click handler and answers it by synthesizing a mouse press at the
/// node's centre — which lands wherever the layout puts it, not necessarily on
/// the control that asked for the action.
#[gpui::test]
async fn test_the_zoom_button_can_be_operated_through_its_action(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(
            Box::new(cx.new(|cx| TestItem::new(cx).with_label("shell"))),
            true,
            true,
            None,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "zoom button");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "zoom button");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "zoom button");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "zoom button");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "zoom button");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "zoom button");

    let zoom = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .find(|node| node["element_id"].as_str() == Some("Name(\"toggle_zoom\")"))
        .expect("the zoom control must be in the tree");
    assert_eq!(zoom["aria"]["label"].as_str(), Some("Zoom In"));
    assert!(
        zoom["aria"]["on_action"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|action| action == "Click"),
        "the control has to advertise that it can be clicked: {zoom}"
    );

    let node_id = zoom["accesskit_id"]
        .as_str()
        .and_then(|id| id.parse::<u64>().ok())
        .expect("every node in the dump carries its AccessKit id");
    let delivered = cx.simulate_a11y_action(
        cx.window_handle(),
        gpui::accesskit::ActionRequest {
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: gpui::accesskit::NodeId(node_id),
            action: gpui::accesskit::Action::Click,
            data: None,
        },
    );
    assert!(delivered, "the action must reach the window");
    cx.run_until_parked();

    assert!(
        pane.read_with(cx, |pane, _| pane.is_zoomed()),
        "advertising Click is worth nothing if it does not zoom the pane"
    );
}

/// A tab carries a close button inside its own bounds, so answering `Click` at
/// the tab node's centre is only correct while the close button stays off
/// centre. Activating a tab and closing it are not close enough for a mistake
/// to be recoverable.
#[gpui::test]
async fn test_clicking_a_tab_through_its_action_activates_it(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        for label in ["shell", "logs"] {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label(label))),
                true,
                true,
                None,
                window,
                cx,
            );
        }
    });
    cx.run_until_parked();
    assert_eq!(
        pane.read_with(cx, |pane, _| pane.active_item_index()),
        1,
        "the second item starts active, so activating the first is a real change"
    );

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "tab bar");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "tab bar");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "tab bar");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "tab bar");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "tab bar");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "tab bar");

    let first_tab = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .find(|node| {
            node["aria"]["role"] == "Tab" && node["aria"]["label"].as_str() == Some("shell")
        })
        .expect("every open item must be reported as a named tab");
    let node_id = first_tab["accesskit_id"]
        .as_str()
        .and_then(|id| id.parse::<u64>().ok())
        .expect("every node in the dump carries its AccessKit id");

    let delivered = cx.simulate_a11y_action(
        cx.window_handle(),
        gpui::accesskit::ActionRequest {
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: gpui::accesskit::NodeId(node_id),
            action: gpui::accesskit::Action::Click,
            data: None,
        },
    );
    assert!(delivered, "the action must reach the window");
    cx.run_until_parked();

    assert_eq!(
        pane.read_with(cx, |pane, _| pane.items_len()),
        2,
        "clicking a tab must not land on its close button"
    );
    assert_eq!(
        pane.read_with(cx, |pane, _| pane.active_item_index()),
        0,
        "clicking a tab has to activate it"
    );
}

/// Which pane of how many is conveyed only by the layout. Two panes running
/// the same program are the same name in the tree, and there is nothing to say
/// how many others exist or where in them the user is.
#[gpui::test]
async fn test_split_panes_say_which_one_they_are(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(
            Box::new(cx.new(|cx| TestItem::new(cx).with_label("shell"))),
            true,
            true,
            None,
            window,
            cx,
        );
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(pane.clone(), crate::SplitDirection::Right, window, cx);
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "split panes");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "split panes");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "split panes");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "split panes");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "split panes");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "split panes");

    let mut positions: Vec<(u64, u64)> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Group")
        .filter_map(|node| {
            Some((
                node["aria"]["position_in_set"].as_u64()?,
                node["aria"]["size_of_set"].as_u64()?,
            ))
        })
        .collect();
    positions.sort();

    assert_eq!(
        positions,
        vec![(1, 2), (2, 2)],
        "each pane has to say which of how many it is"
    );
}

/// The welcome screen takes focus when it opens. Its root carried no id and no
/// role, so that focus produced no node and a reader was told about the window
/// rather than the screen the user had just landed on.
#[gpui::test]
async fn test_the_welcome_screen_is_announced_when_it_takes_focus(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let welcome = cx.update(|window, cx| {
        cx.new(|cx| crate::welcome::WelcomePage::new(workspace.downgrade(), false, window, cx))
    });
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(Box::new(welcome.clone()), true, true, None, window, cx);
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            let handle = gpui::Focusable::focus_handle(welcome.read(cx), cx);
            window.focus(&handle, cx);
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "welcome screen");
    let focused = tree["gpui_focus"]
        .as_str()
        .and_then(|id| tree["nodes"].get(id))
        .expect("the focus must name a node in the dump");
    assert_eq!(focused["aria"]["label"].as_str(), Some("Welcome to Zed"));
}

/// A live region announces changes made *inside* it. A region that is created
/// at the same moment as its first message has nothing to compare against, so
/// both regions have to be in the tree before there is anything to say.
#[gpui::test]
async fn test_the_live_regions_exist_before_they_have_anything_to_say(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (_multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "idle live regions");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "idle live regions");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "idle live regions");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "idle live regions");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "idle live regions");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "idle live regions");

    let live_regions: Vec<(&str, &str)> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter_map(|node| {
            Some((node["aria"]["role"].as_str()?, node["aria"]["live"].as_str()?))
        })
        .collect();

    assert!(
        live_regions.contains(&("Log", "Polite")),
        "the notification stack must already be a live region: {live_regions:?}"
    );
    assert!(
        live_regions.contains(&("Status", "Polite")),
        "the toast layer must already be a live region: {live_regions:?}"
    );
}

/// A modal captures input and hides everything behind it. Its container had no
/// role and no id, so the focused element produced no accessibility node at
/// all: focus was discarded and the whole window announced instead of the
/// dialog the user is now inside.
#[gpui::test]
async fn test_modal_is_announced_as_a_dialog(cx: &mut TestAppContext) {
    use gpui::{Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, StatefulInteractiveElement as _, Window};

    struct TestModal {
        focus_handle: gpui::FocusHandle,
    }

    impl gpui::Focusable for TestModal {
        fn focus_handle(&self, _: &gpui::App) -> gpui::FocusHandle {
            self.focus_handle.clone()
        }
    }
    impl EventEmitter<gpui::DismissEvent> for TestModal {}
    impl crate::ModalView for TestModal {
        fn a11y_name(&self, _: &gpui::App) -> Option<gpui::SharedString> {
            Some("Test dialog".into())
        }
    }
    impl Render for TestModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // Shaped like a real modal: `show_modal` focuses the modal's own
            // handle, not the layer's container, so this is what has to carry
            // an id and a role for focus to reach the tree at all.
            gpui::div()
                .id("test-modal-root")
                .role(gpui::Role::Group)
                .aria_label("Test modal")
                .track_focus(&self.focus_handle)
        }
    }

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| TestModal {
            focus_handle: cx.focus_handle(),
        });
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "modal layer");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "modal layer");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "modal layer");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "modal layer");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "modal layer");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "modal layer");

    let dialog = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .find(|node| node["aria"]["role"] == "Dialog")
        .expect("an open modal must be reported as a dialog");
    assert_eq!(
        dialog["aria"]["modal"].as_bool(),
        Some(true),
        "content behind an open modal is unreachable, and has to be reported that way"
    );
    assert_eq!(
        dialog["aria"]["label"].as_str(),
        Some("Test dialog"),
        "a dialog with no name is announced as \"dialog\" and nothing else"
    );
    assert_eq!(
        tree["frame"]["focus_without_node"].as_str(),
        None,
        "a modal whose root carries a role must keep its focus in the tree"
    );

    // The focused element has to sit inside the dialog, or assistive technology
    // reports a modal context the user is not actually in.
    let focused = tree["gpui_focus"]
        .as_str()
        .expect("the modal's root holds focus");
    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");
    let mut pending: Vec<String> = dialog["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect();
    let mut focus_is_inside = false;
    while let Some(id) = pending.pop() {
        if id == focused {
            focus_is_inside = true;
            break;
        }
        if let Some(children) = nodes.get(&id).and_then(|node| node["children"].as_array()) {
            pending.extend(children.iter().filter_map(|id| id.as_str().map(str::to_string)));
        }
    }
    assert!(
        focus_is_inside,
        "the focused element must be a descendant of the dialog"
    );
}

/// A node with an interactive role and no name is announced as a bare "button"
/// with nothing to tell it apart. Checked across a whole rendered workspace
/// rather than per control, so chrome added later cannot quietly skip it.
#[gpui::test]
async fn test_every_interactive_node_in_the_window_has_a_name(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(
            Box::new(cx.new(|cx| TestItem::new(cx).with_label("shell"))),
            true,
            true,
            None,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "workspace window");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "workspace window");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "workspace window");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "workspace window");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "workspace window");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "workspace window");
}

/// A pane with no items has nothing inside to take focus, so focus stays on the
/// pane's own root. That root carried no id and no role, so the focus produced
/// no node at all and screen readers fell back to announcing the whole window.
#[gpui::test]
async fn test_an_empty_pane_holding_focus_is_announced(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());
    let pane_focus = pane.read_with(cx, |pane, cx| gpui::Focusable::focus_handle(pane, cx));

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.focus(&pane_focus, cx);
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "empty pane");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "empty pane");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "empty pane");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "empty pane");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "empty pane");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "empty pane");

    assert!(
        cx.update(|window, cx| window
            .focused(cx)
            .is_some_and(|handle| handle == pane_focus)),
        "the pane really does hold focus, so this is not a vacuous check"
    );
    assert_eq!(
        tree["frame"]["focus_without_node"].as_str(),
        None,
        "the focused pane must reach the tree"
    );
    let focused = tree["gpui_focus"]
        .as_str()
        .and_then(|id| tree["nodes"].get(id))
        .expect("the focus must name a node in the dump");
    assert_eq!(focused["aria"]["label"].as_str(), Some("Empty pane"));
}

/// The pane group is hosted in a cached view, and a cached view that replays
/// its recorded prepaint contributes no accessibility nodes. The tree is
/// rebuilt from scratch every frame, so the centre of the window could be
/// reported once and then disappear on the next redraw — leaving the reader
/// with whatever happened to be dirty that frame.
#[gpui::test]
async fn test_the_open_item_is_still_reported_on_later_frames(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(
            Box::new(cx.new(|cx| TestItem::new(cx).with_label("shell"))),
            true,
            true,
            None,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    for frame in 1..=3 {
        let json = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "open item");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "open item");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "open item");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "open item");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "open item");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "open item");

        let tabs: Vec<String> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter(|node| node["aria"]["role"] == "Tab")
            .filter_map(|node| node["aria"]["label"].as_str().map(str::to_string))
            .collect();

        assert!(
            tabs.iter().any(|label| label == "shell"),
            "the open item vanished from the tree on frame {frame}: {tabs:?}"
        );
    }
}
