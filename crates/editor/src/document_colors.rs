use std::ops::Range;

use gpui::{App, Context, Hsla};
use multi_buffer::Anchor;
use project::{DocumentColor, InlayId};
use settings::Settings as _;
use text::BufferId;
use ui::Window;

use crate::{
    DisplayPoint, Editor, EditorSettings, EditorSnapshot, editor_settings::DocumentColorsRenderMode,
};

#[derive(Debug)]
pub(super) struct LspColorData {
    render_mode: DocumentColorsRenderMode,
}

#[derive(Debug, Default)]
struct BufferColors {
    colors: Vec<(Range<Anchor>, DocumentColor, InlayId)>,
}

impl LspColorData {
    pub fn new(cx: &App) -> Self {
        Self {
            render_mode: EditorSettings::get_global(cx).lsp_document_colors,
        }
    }

    pub fn render_mode_updated(
        &mut self,
        new_render_mode: DocumentColorsRenderMode,
    ) -> Option<crate::InlaySplice> {
        if self.render_mode == new_render_mode {
            return None;
        }
        self.render_mode = new_render_mode;
        Some(crate::InlaySplice {
            to_remove: Vec::new(),
            to_insert: Vec::new(),
        })
    }

    pub fn editor_display_highlights(
        &self,
        _snapshot: &EditorSnapshot,
    ) -> (DocumentColorsRenderMode, Vec<(Range<DisplayPoint>, Hsla)>) {
        (self.render_mode, Vec::new())
    }
}

impl Editor {
    pub(super) fn refresh_document_colors(
        &mut self,
        _buffer_id: Option<BufferId>,
        _window: &Window,
        _cx: &mut Context<Self>,
    ) {
        // 只读编辑器：LSP 文档颜色已禁用。
    }
}
