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
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_controls_have_area(&tree, "workspace with a dock");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "workspace with a dock");
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
        let mut rows: Vec<(String, Option<String>, u64)> = file_tree.1["children"]
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
                    node["aria"]["role_description"]
                        .as_str()
                        .map(str::to_string),
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
                .all(|(name, _, level)| !name.is_empty() && *level >= 1),
            "every row needs a name and a 1-based depth: {rows:?}"
        );

        // A folder and a file are both outline rows on macOS, and `expanded` —
        // the one property that would imply a folder — reaches Windows alone.
        // The role description reaches all three, and replaces what a reader
        // says in place of "row".
        assert!(
            rows.iter()
                .any(|(name, kind, _)| name.starts_with("src") && kind.as_deref() == Some("folder")),
            "a directory row has to say it is one: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|(name, kind, _)| name.starts_with("README.md") && kind.is_none()),
            "and a file row has to not: {rows:?}"
        );

        // Whether a folder is open decides what the next arrow-down lands on.
        // `aria_expanded` says it and reaches Windows alone, so the name has
        // to say it too or macOS and Linux hear a tree with no state in it.
        assert!(
            rows.iter()
                .any(|(name, _, _)| name == "src, collapsed"),
            "a closed folder has to say it is closed: {rows:?}"
        );
    }

    // And the other half: the state has to follow the folder, not be a fixed
    // word. Expanding through the panel's own action rather than by poking the
    // set, so this covers the path the keyboard takes.
    panel.update_in(&mut cx, |panel, window, cx| {
        panel.select_first(&Default::default(), window, cx);
        // Past the worktree root, which opens by itself, onto `src`.
        panel.select_next(&Default::default(), window, cx);
        panel.expand_selected_entry(&Default::default(), window, cx);
    });
    cx.run_until_parked();

    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");
    let expanded: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "TreeItem")
        .filter_map(|node| node["aria"]["label"].as_str())
        .collect();
    assert!(
        expanded.contains(&"src, expanded"),
        "an opened folder has to say it is open: {expanded:?}"
    );
}

/// A modified file is shown in a different colour, with a small "M" beside it
/// when the setting is on. Colour is not information a reader can reach and a
/// lone "M" is not information either, so the row says the word.
#[gpui::test]
async fn a_modified_file_says_it_is_modified(cx: &mut gpui::TestAppContext) {
    project_panel_tests::init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/workspace",
        json!({
            ".git": {},
            "src": { "nested.rs": "changed" },
            "edited.rs": "changed",
            "untouched.rs": "same",
        }),
    )
    .await;
    fs.set_head_and_index_for_repo(
        "/workspace/.git".as_ref(),
        &[
            ("edited.rs", "original".into()),
            ("untouched.rs", "same".into()),
            ("src/nested.rs", "original".into()),
        ],
    );

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

    cx.activate_a11y(cx.window_handle());
    let json = cx
        .update(|window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })
        .expect("activation makes the debug tree available");
    let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "project panel with git status");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "project panel with git status");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "project panel with git status");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "project panel with git status");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "project panel with git status");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "project panel with git status");
    gpui::a11y_checks::assert_controls_have_area(&tree, "project panel with git status");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "project panel with git status");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "project panel with git status");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "project panel with git status");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "project panel with git status");

    let mut rows: Vec<&str> = tree["nodes"]
        .as_object()
        .expect("the dump lists nodes")
        .values()
        .filter(|node| node["aria"]["role"] == "TreeItem")
        .filter_map(|node| node["aria"]["label"].as_str())
        .filter(|label| {
            label.starts_with("edited.rs")
                || label.starts_with("untouched.rs")
                || label.starts_with("src")
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        // A folder's summary is about what is inside it: the dot beside a
        // folder does not mean the folder itself was modified.
        vec![
            "edited.rs, modified",
            "src, collapsed, contains changes",
            "untouched.rs",
        ],
        "the colour beside the name is the only other thing that says this"
    );
}

/// Renaming happens in an editor that appears inside the row, pre-filled with
/// the current name — so it never shows a placeholder, and a single-line editor
/// takes its name from its placeholder. Without a name of its own the field the
/// user is typing into announces as "edit text" and nothing else.
#[gpui::test]
async fn the_rename_field_says_what_it_is(cx: &mut gpui::TestAppContext) {
    project_panel_tests::init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/workspace", json!({ "edited.rs": "changed" }))
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

    panel.update_in(&mut cx, |panel, window, cx| {
        window.focus(&gpui::Focusable::focus_handle(panel, cx), cx);
        panel.select_next(&Default::default(), window, cx);
        panel.rename(&Default::default(), window, cx);
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

    gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "renaming a file");
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "renaming a file");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "renaming a file");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "renaming a file");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "renaming a file");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "renaming a file");
    gpui::a11y_checks::assert_controls_have_area(&tree, "renaming a file");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "renaming a file");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "renaming a file");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "renaming a file");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "renaming a file");
    gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "renaming a file");

    let focused = tree["gpui_focus"]
        .as_str()
        .and_then(|id| nodes.get(id))
        .unwrap_or_else(|| panic!("the rename field holds focus: {json}"));
    assert_eq!(focused["aria"]["role"].as_str(), Some("TextInput"));
    assert_eq!(focused["aria"]["label"].as_str(), Some("File name"));
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
    gpui::a11y_checks::assert_names_are_distinguishable(&tree, "empty project panel");
    gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "empty project panel");
    gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "empty project panel");
    gpui::a11y_checks::assert_roles_are_contained(&tree, "empty project panel");
    gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "empty project panel");
    gpui::a11y_checks::assert_controls_have_area(&tree, "empty project panel");
    gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "empty project panel");
    gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "empty project panel");
    gpui::a11y_checks::assert_no_role_was_discarded(&tree, "empty project panel");
    gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "empty project panel");
}
