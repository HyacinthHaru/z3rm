use super::*;
use gpui::{TestAppContext, WindowHandle};
use settings::SettingsStore;

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
