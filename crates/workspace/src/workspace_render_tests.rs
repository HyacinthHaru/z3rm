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
use gpui::{AppContext, TestAppContext, UpdateGlobal as _};
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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "notifications");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "notifications");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "notifications");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "notifications");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "notifications");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "notifications");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "notifications");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "notifications");
    gpui::a11y_checks::assert_controls_have_area(&tree, "notifications");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "notifications");
    gpui::a11y_checks::assert_live_regions_can_speak(&tree, "notifications");

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
    // What is actually spoken, and it has to be in both fields: macOS raises
    // an announcement only when the region has a value and speaks that value,
    // while Windows and Linux raise theirs on a name change and speak the
    // name. Neither platform descends into the notifications drawn inside.
    assert_eq!(
        log["aria"]["label"].as_str(),
        Some("the mux server went away"),
        "the region's name is the announcement, not a standing title"
    );
    assert_eq!(
        log["aria"]["value"].as_str(),
        Some("the mux server went away"),
        "the newest notification's text has to be the region's value"
    );

    // Separately, whoever goes looking for the notification has to find it
    // named. The body is plain text and contributes no node of its own, so
    // this is the wrapper's label rather than anything the region announces.
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

/// An item that shortens its title for the strip says so through
/// `tab_announcement_text`, and `tab_announcement` has to be the thing that
/// asks. Both halves are one-liners in the items that need it — the commit
/// view and project search — so what could regress unnoticed is this wiring:
/// point it back at `tab_content_text` and their own tests still pass.
#[gpui::test]
async fn a_tab_announces_the_title_the_item_gives_it(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        let item = cx.new(|cx| {
            let mut item = TestItem::new(cx).with_label("Fix a crash when…");
            item.tab_announcement =
                Some("Fix a crash when the mux server goes away".into());
            item
        });
        pane.add_item(Box::new(item), true, true, None, window, cx);
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

    let tabs: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Tab")
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    assert_eq!(
        tabs,
        vec!["Fix a crash when the mux server goes away"],
        "the tab announces what the item said to announce, not what it drew"
    );
}

/// A notification is spoken the moment it arrives, over whatever the user was
/// doing, so a multi-line message — a language server prompt's markdown, an
/// error with its causes — is a paragraph read aloud unprompted. The stack
/// announces the first line; the notification itself keeps the whole message
/// as its name, which is what a reader gets when they go to it.
#[gpui::test]
async fn a_multi_line_notification_announces_only_its_first_line(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    const MESSAGE: &str = "could not push to origin\ncaused by: permission denied";
    workspace.update(cx, |workspace, cx| {
        struct MultiLine;
        workspace.show_notification(crate::NotificationId::unique::<MultiLine>(), cx, |cx| {
            cx.new(|cx| {
                crate::notifications::simple_message_notification::MessageNotification::new(
                    MESSAGE, cx,
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
    gpui::a11y_checks::assert_live_regions_can_speak(&tree, "multi-line notification");
    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

    let log = nodes
        .values()
        .find(|node| node["aria"]["role"] == "Log")
        .expect("the notification stack must be reported as a log");
    assert_eq!(
        log["aria"]["value"].as_str(),
        Some("could not push to origin"),
        "the announcement stops at the first line"
    );

    // A failure and a piece of news arrive through the same component and are
    // told apart by a red warning icon, which is not a node and carries no
    // text. The severity has to be in the words.
    workspace.update(cx, |workspace, cx| {
        struct Failure;
        workspace.show_notification(crate::NotificationId::unique::<Failure>(), cx, |cx| {
            cx.new(|cx| {
                crate::notifications::simple_message_notification::MessageNotification::
                    from_workspace_error("could not reach the mux server", cx)
            })
        });
    });
    cx.run_until_parked();
    let failure_json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let failure_tree: serde_json::Value =
        serde_json::from_str(&failure_json).expect("the dump is valid JSON");
    let failure_log = failure_tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .find(|node| node["aria"]["role"] == "Log")
        .expect("the notification stack must be reported as a log");
    assert_eq!(
        failure_log["aria"]["value"].as_str(),
        Some("Error: could not reach the mux server"),
        "an error has to say it is one: {failure_json}"
    );

    let named = nodes
        .values()
        .filter_map(|node| node["aria"]["label"].as_str())
        .any(|label| label == MESSAGE);
    assert!(
        named,
        "and the notification itself keeps the whole message for whoever reads it: {json}"
    );
}

/// Notifications stack, and the one that just arrived is the one a user needs
/// to hear. The region carries a single value, so it has to be the newest.
#[gpui::test]
async fn test_the_newest_notification_is_the_one_announced(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    for message in ["the mux server went away", "the language server crashed"] {
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(crate::NotificationId::named(message.into()), cx, |cx| {
                cx.new(|cx| {
                    crate::notifications::simple_message_notification::MessageNotification::new(
                        message, cx,
                    )
                })
            });
        });
        cx.run_until_parked();
    }

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    gpui::a11y_checks::assert_live_regions_can_speak(&tree, "notifications");

    let log = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .find(|node| node["aria"]["role"] == "Log")
        .expect("the notification stack must be reported as a log");
    assert_eq!(
        log["aria"]["value"].as_str(),
        Some("the language server crashed"),
        "the second notification is the one that just arrived"
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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "zoom button");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "zoom button");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "zoom button");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "zoom button");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "zoom button");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "zoom button");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "zoom button");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "zoom button");
    gpui::a11y_checks::assert_controls_have_area(&tree, "zoom button");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "zoom button");

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

    // Zooming hides the other panes, which from the tree is indistinguishable
    // from a window that only ever had one.
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    let zoomed: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter_map(|node| node["aria"]["label"].as_str())
        .filter(|label| label.ends_with(", zoomed"))
        .collect();
    assert_eq!(
        zoomed.len(),
        1,
        "the zoomed pane has to say so: {zoomed:?}"
    );
}

/// Two files with the same name are the ordinary case in any project with a
/// `mod.rs` or an `index.ts`, and the tab bar disambiguates them by widening
/// the path it shows. The announced name is built from the same detail level,
/// so if that ever stops being true a reader hears the same word twice with no
/// way to tell the tabs apart.
#[gpui::test]
async fn test_two_tabs_with_the_same_file_name_are_told_apart(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        for descriptions in [
            vec!["main.rs", "src/main.rs"],
            vec!["main.rs", "tests/main.rs"],
        ] {
            pane.add_item(
                Box::new(cx.new(|cx| {
                    let mut item = TestItem::new(cx).with_label("main.rs");
                    item.tab_descriptions = Some(descriptions);
                    item
                })),
                true,
                true,
                None,
                window,
                cx,
            );
        }
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

    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_controls_have_area(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "ambiguous tabs");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "ambiguous tabs");

    let mut tabs: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Tab")
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    tabs.sort();
    assert_eq!(
        tabs,
        vec!["src/main.rs", "tests/main.rs"],
        "the tab bar widened the path it shows, and the name has to follow it"
    );
}

/// A tab shows unsaved changes as a coloured dot beside its name, and an icon
/// contributes no accessibility node, so nothing told a reader which of the
/// open files had unsaved work in them.
#[gpui::test]
async fn test_a_tab_says_it_has_unsaved_changes(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        for (label, dirty) in [("saved", false), ("edited", true)] {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label(label).with_dirty(dirty))),
                true,
                true,
                None,
                window,
                cx,
            );
        }
        // The same dot, meaning something else. A terminal reports `is_dirty`
        // when it rang the bell or is still running, and announcing that as
        // unsaved work is worse than announcing nothing: it describes a state
        // the item cannot be in and offers a save that does not exist.
        pane.add_item(
            Box::new(cx.new(|cx| {
                TestItem::new(cx)
                    .with_label("bash")
                    .with_dirty(true)
                    .with_dirty_announcement("bell")
            })),
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

    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "dirty tab");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "dirty tab");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "dirty tab");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "dirty tab");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "dirty tab");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "dirty tab");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "dirty tab");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "dirty tab");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "dirty tab");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "dirty tab");
    gpui::a11y_checks::assert_controls_have_area(&tree, "dirty tab");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "dirty tab");

    let mut tabs: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Tab")
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    tabs.sort();
    assert_eq!(
        tabs,
        vec!["bash, bell", "edited, unsaved changes", "saved"],
        "the dot beside the name is the only other thing that says this"
    );

    // One close button per tab, so a shared name would be the same word
    // repeated across the bar with nothing to tell them apart.
    let mut close_buttons: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Button")
        .filter_map(|node| node["aria"]["label"].as_str())
        .filter(|label| label.starts_with("Close Tab"))
        .collect();
    close_buttons.sort();
    assert_eq!(
        close_buttons,
        vec!["Close Tab: bash", "Close Tab: edited", "Close Tab: saved"],
        "each close button says which tab it closes"
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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "tab bar");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "tab bar");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "tab bar");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "tab bar");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "tab bar");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "tab bar");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "tab bar");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "tab bar");
    gpui::a11y_checks::assert_controls_have_area(&tree, "tab bar");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "tab bar");

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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "split panes");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "split panes");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "split panes");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "split panes");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "split panes");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "split panes");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "split panes");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "split panes");
    gpui::a11y_checks::assert_controls_have_area(&tree, "split panes");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "split panes");

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

    // And in the name, which is the half that reaches macOS: it exposes
    // neither `position_in_set` nor `size_of_set`, so the assertion above
    // passes on a platform where nothing of it is announced.
    let mut named: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Group")
        .filter(|node| node["aria"]["position_in_set"].as_u64().is_some())
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    named.sort_unstable();
    assert_eq!(
        named,
        vec!["Empty pane, pane 2 of 2", "shell, pane 1 of 2"],
        "the position has to be in the name too, or macOS hears neither"
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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "idle live regions");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "idle live regions");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "idle live regions");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "idle live regions");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "idle live regions");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "idle live regions");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "idle live regions");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "idle live regions");
    gpui::a11y_checks::assert_controls_have_area(&tree, "idle live regions");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "idle live regions");

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

/// A file that will not open is an error state nothing rendered in a frame.
/// The view takes focus while carrying no id and no role, so focus moved to a
/// node that does not exist; and the heading is a bare string with the reason
/// in a `Label`, so neither reached the tree. A reader was told about the
/// window, and not that the file had failed or why.
#[gpui::test]
async fn test_a_file_that_will_not_open_says_so(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        let view = cx.new(|cx| {
            crate::invalid_item_view::InvalidItemView::new(
                std::path::Path::new(path!("/root/broken.png")),
                true,
                &anyhow::anyhow!("unsupported image format"),
                window,
                cx,
            )
        });
        pane.add_item(Box::new(view), true, true, None, window, cx);
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

    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "a file that will not open");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "a file that will not open");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "a file that will not open");
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "a file that will not open");

    let names: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    assert!(
        names
            .iter()
            .any(|name| *name == "Could not open broken.png: unsupported image format"),
        "the failure has to say which file and why: {names:?}"
    );
}

/// Closing a dialog is where a reader gets lost. The modal takes focus when it
/// opens, and if dismissing it leaves focus on a handle whose element is gone,
/// the tree has a focus that resolves to nothing — so assistive technology
/// announces the whole window instead of wherever the user now is. Nothing
/// closed a modal in a frame test before this.
#[gpui::test]
async fn test_focus_lands_somewhere_when_a_modal_closes(cx: &mut TestAppContext) {
    use gpui::{Context, EventEmitter, InteractiveElement as _, IntoElement, Render,
        StatefulInteractiveElement as _, Styled as _, Window};

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
            gpui::div()
                .id("test-modal-root")
                .role(gpui::Role::Group)
                .aria_label("Test modal")
                .w(gpui::px(320.))
                .h(gpui::px(160.))
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
    cx.update(|window, cx| window.draw(cx).clear(cx));

    // Dismissed the way Escape dismisses it, rather than by dropping the
    // entity: the question is what the product's own path leaves behind.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| TestModal {
            focus_handle: cx.focus_handle(),
        });
    });
    cx.run_until_parked();

    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "after a modal closes");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "after a modal closes");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "after a modal closes");

    assert!(
        !tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .any(|node| node["aria"]["label"] == "Test dialog"),
        "the dialog has to be gone from the tree, not merely invisible: {json}"
    );

    // `assert_focus_reached_the_tree` only fails when there *is* a focus and it
    // resolves to nothing, so it passes just as happily on a window that
    // focuses nobody. What the user needs is to be told where they now are, so
    // this asks for the focused node and for it to have a name.
    let focused = tree["gpui_focus"]
        .as_str()
        .and_then(|id| tree["nodes"].get(id))
        .unwrap_or_else(|| panic!("closing the dialog has to leave focus somewhere: {json}"));
    assert_eq!(
        focused["aria"]["label"].as_str(),
        Some("Empty pane"),
        "and somewhere that says what it is: {focused}"
    );
}

/// A modal captures input and hides everything behind it. Its container had no
/// role and no id, so the focused element produced no accessibility node at
/// all: focus was discarded and the whole window announced instead of the
/// dialog the user is now inside.
#[gpui::test]
async fn test_modal_is_announced_as_a_dialog(cx: &mut TestAppContext) {
    use gpui::{Context, EventEmitter, InteractiveElement as _, IntoElement,
        Render, StatefulInteractiveElement as _, Styled as _, Window};

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
                // Sized like a real modal: the layer centres the dialog over a
                // zero-height container, so a modal with no size of its own
                // would leave the dialog with an empty rectangle.
                .w(gpui::px(320.))
                .h(gpui::px(160.))
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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "modal layer");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "modal layer");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "modal layer");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "modal layer");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "modal layer");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "modal layer");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "modal layer");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "modal layer");
    gpui::a11y_checks::assert_controls_have_area(&tree, "modal layer");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "modal layer");

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

    // A button that opens a menu is a disclosure, not a two-state toggle. The
    // tab bar has one of each, so this pins the difference: the menu triggers
    // report whether they are open, the zoom button whether it is pressed, and
    // neither reports both.
    let button_state = |label: &str| {
        tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .find(|node| node["aria"]["label"] == label)
            .map(|node| {
                (
                    node["aria"]["expanded"].as_bool(),
                    node["aria"]["toggled"].as_str().map(str::to_owned),
                )
            })
            .unwrap_or_else(|| panic!("no button named {label:?}: {json}"))
    };
    assert_eq!(
        button_state("New"),
        (Some(false), None),
        "a menu trigger says whether its menu is open, not whether it is pressed"
    );
    assert_eq!(
        button_state("Split Pane"),
        (Some(false), None),
        "a menu trigger says whether its menu is open, not whether it is pressed"
    );
    assert_eq!(
        button_state("Zoom In"),
        (None, Some("False".to_string())),
        "a real toggle still reports the state it actually has"
    );
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "workspace window");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "workspace window");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "workspace window");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "workspace window");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "workspace window");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "workspace window");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "workspace window");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "workspace window");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "workspace window");
    gpui::a11y_checks::assert_controls_have_area(&tree, "workspace window");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "workspace window");
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
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "empty pane");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "empty pane");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "empty pane");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "empty pane");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "empty pane");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "empty pane");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "empty pane");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "empty pane");
    gpui::a11y_checks::assert_controls_have_area(&tree, "empty pane");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "empty pane");

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
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "open item");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "open item");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "open item");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "open item");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "open item");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "open item");
        gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "open item");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "open item");
        gpui::a11y_checks::assert_controls_have_area(&tree, "open item");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "open item");

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

/// Pinning a tab splits the bar in two, and each half is its own tab list. A
/// tab's place has to be within the list it is actually in: numbering across
/// both makes the first unpinned tab "3 of 7" inside a list of five, and leaves
/// a reader with two lists it cannot tell apart.
#[gpui::test]
async fn pinned_tabs_are_their_own_list(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                let tab_bar = settings.tab_bar.get_or_insert_default();
                tab_bar.show = true;
                tab_bar.show_pinned_tabs_in_separate_row = Some(true);
            });
        });
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        for label in ["pinned.rs", "notes.md", "shell", "logs", "main.rs"] {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label(label))),
                true,
                true,
                None,
                window,
                cx,
            );
        }
        pane.set_pinned_count(2);
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
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "pinned tabs");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "pinned tabs");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "pinned tabs");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "pinned tabs");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "pinned tabs");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "pinned tabs");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "pinned tabs");
    gpui::a11y_checks::assert_controls_have_area(&tree, "pinned tabs");

    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");
    let mut lists: Vec<&str> = nodes
        .values()
        .filter(|node| node["aria"]["role"] == "TabList")
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    lists.sort_unstable();
    assert_eq!(
        lists,
        vec!["Pinned tabs", "Tabs"],
        "two lists in one window have to say which is which"
    );

    // Each tab's set size must match the list it sits in, not the total.
    let mut sets: Vec<u64> = nodes
        .values()
        .filter(|node| node["aria"]["role"] == "Tab")
        .filter_map(|node| node["aria"]["size_of_set"].as_u64())
        .collect();
    sets.sort_unstable();
    assert_eq!(
        sets,
        vec![2, 2, 3, 3, 3],
        "two tabs are in a list of two and three are in a list of three"
    );

    let mut pinned_positions: Vec<u64> = nodes
        .values()
        .filter(|node| node["aria"]["role"] == "Tab" && node["aria"]["size_of_set"] == 2)
        .filter_map(|node| node["aria"]["position_in_set"].as_u64())
        .collect();
    pinned_positions.sort_unstable();
    assert_eq!(pinned_positions, vec![1, 2]);

    let mut unpinned_positions: Vec<u64> = nodes
        .values()
        .filter(|node| node["aria"]["role"] == "Tab" && node["aria"]["size_of_set"] == 3)
        .filter_map(|node| node["aria"]["position_in_set"].as_u64())
        .collect();
    unpinned_positions.sort_unstable();
    assert_eq!(
        unpinned_positions,
        vec![1, 2, 3],
        "the unpinned list starts at one, not after the pinned tabs"
    );
}

/// Stacked mode runs the tabs down the side and does not use the `TabBar`
/// component, so the role and name that component supplies are absent unless
/// the stacked container sets them itself. Tabs in no list lose "2 of 5" and
/// the bar stops being somewhere a reader can jump to.
#[gpui::test]
async fn the_stacked_tab_bar_is_still_a_tab_list(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                let tab_bar = settings.tab_bar.get_or_insert_default();
                tab_bar.show = true;
                // On, to prove the stacked bar ignores it: stacked mode merges
                // pinned and unpinned into one list, so numbering must not be
                // split even though the setting asks for separate rows.
                tab_bar.show_pinned_tabs_in_separate_row = Some(true);
            });
        });
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        for label in ["pinned.rs", "shell", "logs"] {
            pane.add_item(
                Box::new(cx.new(|cx| TestItem::new(cx).with_label(label))),
                true,
                true,
                None,
                window,
                cx,
            );
        }
        pane.set_pinned_count(1);
        pane.set_tabbar_style(crate::layout_projection::TabBarStyle::Stacked, cx);
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
    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "stacked tabs");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "stacked tabs");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "stacked tabs");
    // The check this is really about: a `Tab` outside a `TabList` keeps its
    // role and loses everything the containment gives it.
    gpui::a11y_checks::assert_roles_are_contained(&tree, "stacked tabs");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "stacked tabs");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "stacked tabs");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "stacked tabs");
    gpui::a11y_checks::assert_controls_have_area(&tree, "stacked tabs");

    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");
    let list = nodes
        .values()
        .find(|node| node["aria"]["role"] == "TabList")
        .expect("the stacked bar is a tab list");
    assert_eq!(list["aria"]["label"].as_str(), Some("Tabs"));
    assert_eq!(
        list["aria"]["orientation"].as_str(),
        Some("Vertical"),
        "the tabs run down the side, so up and down are what move between them"
    );

    let mut set: Vec<(u64, u64)> = nodes
        .values()
        .filter(|node| node["aria"]["role"] == "Tab")
        .map(|node| {
            (
                node["aria"]["position_in_set"].as_u64().unwrap_or_default(),
                node["aria"]["size_of_set"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    set.sort_unstable();
    assert_eq!(
        set,
        vec![(1, 3), (2, 3), (3, 3)],
        "stacked mode is one list, so the pinned tab is numbered with the rest"
    );
}

/// A tab draws its state and says none of it: struck through for a file that
/// is gone, italic for one the next file will replace, a coloured dot for
/// unsaved work. All three change what the next keystroke does.
#[gpui::test]
async fn a_tab_says_the_state_it_draws(cx: &mut TestAppContext) {
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
            Box::new(cx.new(|cx| TestItem::new(cx).with_label("saved.rs"))),
            true,
            true,
            None,
            window,
            cx,
        );
        pane.add_item(
            Box::new(cx.new(|cx| TestItem::new(cx).with_label("edited.rs").with_dirty(true))),
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

    let mut tabs: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "Tab")
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    tabs.sort_unstable();
    assert_eq!(
        tabs,
        vec!["edited.rs, unsaved changes", "saved.rs"],
        "a dot in the corner is the only other thing that says this"
    );
}

/// Opening a dialog moves focus into it; closing one has to give focus back.
/// Every other accessibility test on this branch reads one frame, and this is
/// not visible in one: a dialog that leaves focus nowhere looks identical to a
/// healthy window until the user presses a key and nothing happens.
#[gpui::test]
async fn dismissing_a_dialog_gives_focus_back(cx: &mut TestAppContext) {
    use gpui::{
        Context, EventEmitter, InteractiveElement as _, IntoElement, Render,
        StatefulInteractiveElement as _, Styled as _, Window,
    };

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
            gpui::div()
                .id("test-modal-root")
                .role(gpui::Role::Group)
                .aria_label("Test modal")
                .w(gpui::px(320.))
                .h(gpui::px(160.))
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
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    let item = cx.new(|cx| TestItem::new(cx).with_label("shell"));
    pane.update_in(cx, |pane, window, cx| {
        pane.add_item(Box::new(item.clone()), true, true, None, window, cx);
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    let focused_name = |cx: &mut gpui::VisualTestContext| {
        let json = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        let focused = tree["gpui_focus"].as_str().map(str::to_string);
        let name = focused
            .as_ref()
            .and_then(|id| tree["nodes"].get(id))
            .and_then(|node| node["aria"]["label"].as_str())
            .map(str::to_string);
        (name, tree)
    };

    let (before, _) = focused_name(cx);
    assert_eq!(
        before.as_deref(),
        Some("shell"),
        "the item holds focus before the dialog opens"
    );

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| TestModal {
            focus_handle: cx.focus_handle(),
        });
    });
    cx.run_until_parked();

    let (during, tree) = focused_name(cx);
    assert_eq!(
        during.as_deref(),
        Some("Test modal"),
        "opening a dialog moves focus into it"
    );
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "open dialog");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.modal_layer.clone().update(cx, |layer, cx| {
            layer.hide_modal(window, cx);
        });
    });
    cx.run_until_parked();

    let (after, tree) = focused_name(cx);
    assert_eq!(
        after.as_deref(),
        Some("shell"),
        "closing a dialog has to put focus back where it came from, not nowhere"
    );
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "dismissed dialog");
}

/// Closing the item you are inside is the other side of the same problem, and
/// the harder one: a dialog knows where focus came from, while a closed tab
/// leaves focus on an element that no longer exists. Whatever the answer is —
/// the next tab, the pane, the window — it has to be a node, or the reader
/// falls back to announcing the whole window and the user cannot tell that
/// anything happened.
#[gpui::test]
async fn closing_the_focused_item_leaves_focus_somewhere(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root", json!({ "a.txt": "" })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let pane = workspace.read_with(cx, |ws, _| ws.active_pane().clone());

    pane.update_in(cx, |pane, window, cx| {
        for label in ["notes.md", "shell"] {
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

    cx.activate_a11y(cx.window_handle());
    let focus_state = |cx: &mut gpui::VisualTestContext| {
        let json = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        let name = tree["gpui_focus"]
            .as_str()
            .and_then(|id| tree["nodes"].get(id))
            .and_then(|node| node["aria"]["label"].as_str())
            .map(str::to_string);
        (name, tree)
    };

    let (before, _) = focus_state(cx);
    assert_eq!(
        before.as_deref(),
        Some("shell"),
        "the item added last holds focus"
    );

    pane.update_in(cx, |pane, window, cx| {
        pane.close_active_item(&crate::CloseActiveItem::default(), window, cx)
            .detach();
    });
    cx.run_until_parked();

    let (after, tree) = focus_state(cx);
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "after closing the focused item");
    assert!(
        after.is_some(),
        "focus has to land on something that is in the tree: {}",
        serde_json::to_string(&tree["frame"]).unwrap_or_default()
    );
    assert_ne!(
        after.as_deref(),
        Some("shell"),
        "focus must not stay on the item that was closed"
    );
}
