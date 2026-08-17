#![cfg(test)]

//! Verifies the panel's accessibility semantics from a rendered frame rather
//! than from the builder calls that produce them. A role only becomes a node
//! when its element also has an id, and a duplicate id is discarded silently in
//! release builds, so "the code sets a role" and "a screen reader sees one" are
//! different claims.

use fs::FakeFs;
use gpui::VisualTestContext;
use project::Project;
use serde_json::json;
use workspace::MultiWorkspace;

use crate::ProjectPanel;
use crate::project_panel_tests;

/// The panel reported nothing before: focus on a role-less root was discarded
/// and the rows read as a flat run of text.
#[gpui::test]
async fn the_file_tree_is_exposed_as_a_named_tree(cx: &mut gpui::TestAppContext) {
    project_panel_tests::init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/workspace",
        json!({ "src": { "main.rs": "" }, "README.md": "" }),
    )
    .await;
    let project = Project::test(fs, ["/workspace".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window
        .read_with(cx, |mw, _| mw.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let panel = workspace.update_in(&mut cx, ProjectPanel::new);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_panel(panel.clone(), window, cx);
        workspace.open_panel::<ProjectPanel>(window, cx);
    });
    cx.run_until_parked();

    // Focus the panel and move the highlight onto a row: an active-descendant
    // claim is only honoured while the claiming row has a focused ancestor,
    // and with no selection there is nothing to claim.
    panel.update_in(&mut cx, |panel, window, cx| {
        window.focus(&gpui::Focusable::focus_handle(panel, cx), cx);
        panel.select_first(&Default::default(), window, cx);
    });
    cx.run_until_parked();

    cx.activate_a11y(cx.window_handle());
    // The selection lands a frame after the action, so the first frame is drawn
    // and discarded before anything is read out of the tree.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    // Checked on two consecutive frames: a dock hosts its panel in a cached
    // view, which would replay its recorded prepaint and contribute no nodes,
    // reporting the panel once and then losing it on the next redraw. GPUI
    // skips the cache while an accessibility frame is building; this is what
    // keeps that true.
    for frame in 1..=2 {
        let json = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
        let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

        // Run over the whole window, not just the panel: this is the only test
        // that renders a workspace with a dock open, so a defect in how the
        // pieces sit together shows up here or nowhere.
        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "workspace with a dock");
        let file_tree = nodes
            .iter()
            .find(|(_, node)| node["aria"]["label"].as_str() == Some("Project files"))
            .map(|(id, node)| (id.clone(), node.clone()))
            .unwrap_or_else(|| {
                panic!("the panel must be reported as a named tree on frame {frame}")
            });
        assert_eq!(file_tree.1["aria"]["role"].as_str(), Some("Tree"));

        // Read the rows from inside the tree: a row rendered outside it would keep
        // its role and silently lose the set semantics that go with containment.
        let mut rows: Vec<(String, u64)> = file_tree.1["children"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().and_then(|id| nodes.get(id)))
            .filter(|node| node["aria"]["role"] == "TreeItem")
            .map(|node| {
                (
                    node["aria"]["label"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    node["aria"]["level"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        rows.sort();

        assert!(
            !rows.is_empty(),
            "the tree must contain its rows, not merely exist alongside them"
        );
        assert!(
            rows.iter()
                .all(|(name, level)| !name.is_empty() && *level >= 1),
            "every row needs a name and a 1-based depth: {rows:?}"
        );
    }
}

/// A panel with no worktree shows an explanation and two buttons, and it takes
/// focus. The explanation is a plain label, which contributes no node of its
/// own, so without a name on the container the panel announces the whole
/// window and says nothing about why it is empty.
#[gpui::test]
async fn an_empty_project_panel_says_why_it_is_empty(cx: &mut gpui::TestAppContext) {
    project_panel_tests::init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window
        .read_with(cx, |mw, _| mw.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let panel = workspace.update_in(&mut cx, ProjectPanel::new);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_panel(panel, window, cx);
        workspace.open_panel::<ProjectPanel>(window, cx);
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

    let explained = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter_map(|node| node["aria"]["label"].as_str())
        .any(|label| label.starts_with("Choose one of the options below"));
    assert!(explained, "an empty panel has to say why it is empty: {json}");

    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "empty project panel");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "empty project panel");
}
