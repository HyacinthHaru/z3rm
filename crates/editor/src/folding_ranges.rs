use text::BufferId;
use ui::{Context, Window};

use crate::Editor;

impl Editor {
    pub(super) fn refresh_folding_ranges(
        &mut self,
        _for_buffer: Option<BufferId>,
        _window: &Window,
        _cx: &mut Context<Self>,
    ) {
        // The removed LSP service cannot provide document folding ranges.
    }

    pub fn document_folding_ranges_enabled(&self, cx: &ui::App) -> bool {
        self.use_document_folding_ranges && self.display_map.read(cx).has_lsp_folding_ranges()
    }

    pub(super) fn clear_disabled_lsp_folding_ranges(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.use_document_folding_ranges {
            return;
        }

        let buffers_to_clear = self
            .buffer
            .read(cx)
            .all_buffers()
            .into_iter()
            .map(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        if !buffers_to_clear.is_empty() {
            self.display_map.update(cx, |display_map, cx| {
                for buffer_id in buffers_to_clear {
                    display_map.clear_lsp_folding_ranges(buffer_id, cx);
                }
            });
            cx.notify();
        }

        self.refresh_folding_ranges(None, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use crate::test::{editor_test_context::EditorTestContext, init_test};

    #[gpui::test]
    async fn lsp_folding_ranges_are_disabled_without_a_language_server(
        cx: &mut gpui::TestAppContext,
    ) {
        init_test(cx, |_| {});
        let mut context = EditorTestContext::new(cx).await;

        context.update_editor(|editor, window, cx| {
            editor.refresh_folding_ranges(None, window, cx);
        });
        context.run_until_parked();

        context.editor(|editor, _, cx| {
            assert!(!editor.document_folding_ranges_enabled(cx));
        });
    }
}
