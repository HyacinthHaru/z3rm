use super::*;
use gpui::{TestAppContext, UpdateGlobal as _, WindowHandle};
use settings::{InlayHintsSettingsContent, SettingsContent, SettingsStore};

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
    });
}

fn editor_with_text(text: &str, cx: &mut TestAppContext) -> WindowHandle<Editor> {
    let buffer = cx.new(|cx| language::Buffer::local(text, cx));
    cx.add_window(|window, cx| Editor::for_buffer(buffer, None, window, cx))
}

#[gpui::test]
fn inlay_hint_settings_preserve_language_defaults(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, &|settings: &mut SettingsContent| {
                settings.project.all_languages.defaults.inlay_hints =
                    Some(InlayHintsSettingsContent {
                        show_value_hints: Some(true),
                        ..Default::default()
                    });
            });
        });
    });
    let buffer = cx.new(|cx| language::Buffer::local("value", cx));
    let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
    let settings = multi_buffer.read_with(cx, |multi_buffer, cx| {
        let snapshot = multi_buffer.snapshot(cx);
        inlay_hint_settings(
            snapshot.anchor_before(MultiBufferOffset(0)),
            &snapshot,
            cx,
        )
    });

    assert!(settings.show_value_hints);
}

#[gpui::test]
async fn lsp_relevance_accepts_non_ignored_project_files(cx: &mut TestAppContext) {
    init(cx);

    let fs = project::FakeFs::new(cx.executor());
    fs.insert_tree(
        std::path::Path::new("/project"),
        serde_json::json!({"main.rs": "fn main() {}\n"}),
    )
    .await;
    let project =
        project::Project::test(fs, [std::path::Path::new("/project")], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(std::path::Path::new("/project/main.rs"), cx)
        })
        .await
        .expect("project file should open");

    let (editor, mut window_cx) = cx.add_window_view(|window, cx| {
        crate::test::build_editor_with_project(
            project.clone(),
            MultiBuffer::build_from_buffer(buffer.clone(), cx),
            window,
            cx,
        )
    });
    editor.update_in(window_cx, |editor, _, cx| {
        let file = buffer.read(cx).file().cloned();
        assert!(editor.is_lsp_relevant(file.as_ref(), cx));
    });
}

#[gpui::test]
fn insert_undo_redo_restores_text_and_selection(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("123456", cx);

    editor.update(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(2)..MultiBufferOffset(4)]);
        });
        editor.insert("cd", window, cx);
        assert_eq!(editor.text(cx), "12cd56");

        editor.undo(&Undo, window, cx);
        assert_eq!(editor.text(cx), "123456");
        assert_eq!(
            editor.selections.ranges(&editor.display_snapshot(cx)),
            vec![MultiBufferOffset(2)..MultiBufferOffset(4)]
        );

        editor.redo(&Redo, window, cx);
        assert_eq!(editor.text(cx), "12cd56");
        assert_eq!(
            editor.selections.ranges(&editor.display_snapshot(cx)),
            vec![MultiBufferOffset(4)..MultiBufferOffset(4)]
        );
    });
}

#[gpui::test]
fn backspace_and_delete_remove_adjacent_characters(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("abcd", cx);

    editor.update(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(2)..MultiBufferOffset(2)]);
        });
        editor.backspace(&Backspace, window, cx);
        assert_eq!(editor.text(cx), "acd");

        editor.delete(&Delete, window, cx);
        assert_eq!(editor.text(cx), "ad");
    });
}

#[gpui::test]
fn newline_actions_edit_expected_lines(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("one\ntwo", cx);

    editor.update(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(3)..MultiBufferOffset(3)]);
        });
        editor.newline(&Newline, window, cx);
        assert_eq!(editor.text(cx), "one\n\ntwo");
    });
}

#[gpui::test]
fn read_only_editor_rejects_mutating_actions(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("abc", cx);

    editor.update(cx, |editor, window, cx| {
        editor.set_read_only(true);
        editor.insert("x", window, cx);
        editor.backspace(&Backspace, window, cx);
        editor.delete(&Delete, window, cx);
        editor.newline(&Newline, window, cx);
        editor.undo(&Undo, window, cx);
        assert_eq!(editor.text(cx), "abc");
    });
}

#[gpui::test]
fn clear_action_removes_all_text(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("first\nsecond", cx);

    editor.update(cx, |editor, window, cx| {
        editor.clear(window, cx);
        assert_eq!(editor.text(cx), "");
    });
}

#[gpui::test]
fn indentation_actions_round_trip_selected_lines(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("one\ntwo\nthree", cx);

    editor.update(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(0)..MultiBufferOffset(7)]);
        });
        editor.indent(&Indent, window, cx);
        assert_eq!(editor.text(cx), "    one\n    two\nthree");

        editor.outdent(&Outdent, window, cx);
        assert_eq!(editor.text(cx), "one\ntwo\nthree");
    });
}

#[gpui::test]
fn tab_action_indents_a_cursor_and_backtab_reverses_it(cx: &mut TestAppContext) {
    init(cx);
    let editor = editor_with_text("one", cx);

    editor.update(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(0)..MultiBufferOffset(0)]);
        });
        editor.tab(&Tab, window, cx);
        assert_eq!(editor.text(cx), "    one");

        editor.backtab(&Backtab, window, cx);
        assert_eq!(editor.text(cx), "one");
    });
}
