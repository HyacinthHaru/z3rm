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
    let nodes = tree["nodes"].as_object().expect("the dump lists nodes");

    let file_tree = nodes
        .iter()
        .find(|(_, node)| node["aria"]["label"].as_str() == Some("Project files"))
        .map(|(id, node)| (id.clone(), node.clone()))
        .expect("the panel must be reported as a named tree");
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
                node["aria"]["label"].as_str().unwrap_or_default().to_string(),
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
        rows.iter().all(|(name, level)| !name.is_empty() && *level >= 1),
        "every row needs a name and a 1-based depth: {rows:?}"
    );
}
