use core::num;

use gpui::App;
use language::CursorShape;
use project::project_settings::DiagnosticSeverity;
/// 兼容占位类型 - 设置重构后缺失的类型 (spec §16 Plan 16)
/// 这些类型已从 settings crate 移除, 在此定义以保持下游代码编译。

/// 代码透镜
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CodeLens {
    #[default]
    On,
    Off,
}

impl CodeLens {
    /// 是否内联显示代码透镜
    pub fn inline(&self) -> bool {
        matches!(self, CodeLens::On)
    }
}

/// 补全详情对齐
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CompletionDetailAlignment {
    #[default]
    Left,
    Right,
}

/// 补全菜单项种类
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CompletionMenuItemKind {
    #[default]
    All,
    Symbols,
    Keywords,
    Types,
}

/// 当前行高亮
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CurrentLineHighlight {
    #[default]
    Line,
    Gutter,
    All,
    None,
}

/// 延迟毫秒数
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DelayMs(pub u64);

/// 差异视图样式
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DiffViewStyle {
    #[default]
    Unified,
    Split,
}

/// 显示位置
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DisplayIn {
    #[default]
    ActiveEditor,
    AllEditors,
}

/// 文档颜色渲染模式
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DocumentColorsRenderMode {
    #[default]
    Inlay,
    Background,
    Border,
    Full,
    None,
}

/// 多缓冲区双击行为
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DoubleClickInMultibuffer {
    #[default]
    Select,
    Open,
}

/// 跳转定义回退策略
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum GoToDefinitionFallback {
    #[default]
    Lens,
    Search,
    Never,
    None,
    FindAllReferences,
}

/// 跳转定义滚动策略
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum GoToDefinitionScrollStrategy {
    #[default]
    Center,
    Minimum,
    Top,
    Preserve,
}

/// 缩略图
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum MinimapThumb {
    #[default]
    Always,
    Hover,
}
/// 缩略图边框
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum MinimapThumbBorder {
    #[default]
    Full,
    LeftOnly,
    LeftOpen,
    RightOpen,
    None,
    Rounded,
    Square,
}

/// 多光标修饰键
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum MultiCursorModifier {
    #[default]
    Alt,
    CmdOrCtrl,
}

/// 滚动超过最后一行
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ScrollBeyondLastLine {
    #[default]
    OnePage,
    Off,
    VerticalScrollMargin,
}

/// 滚动条诊断显示
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ScrollbarDiagnostics {
    #[default]
    None,
    All,
    Error,
    Warning,
    Information,
}

/// 种子查询设置 (来自 workspace::settings_stubs)
pub use workspace::settings_stubs::SeedQuerySetting;

/// 显示缩略图
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ShowMinimap {
    #[default]
    Auto,
    Always,
    Never,
}

/// 代码片段排序
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SnippetSortOrder {
    #[default]
    Relevance,
    Alphabetical,
    Frequency,
}

/// 相对行号
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum RelativeLineNumbers {
    #[default]
    Disabled,
    Enabled,
    Wrapped,
}

impl RelativeLineNumbers {
    /// 是否启用了相对行号
    pub fn enabled(&self) -> bool {
        !matches!(self, RelativeLineNumbers::Disabled)
    }

    /// 是否使用环绕模式 (wrapped buffer rows)
    pub fn wrapped(&self) -> bool {
        matches!(self, RelativeLineNumbers::Wrapped)
    }
}

use settings::{RegisterSetting, Settings};
use ui::scrollbars::ShowScrollbar;

/// Imports from the VSCode settings at
/// https://code.visualstudio.com/docs/reference/default-settings
#[derive(Clone, RegisterSetting)]
pub struct EditorSettings {
    pub cursor_blink: bool,
    pub cursor_shape: Option<CursorShape>,
    pub current_line_highlight: CurrentLineHighlight,
    pub selection_highlight: bool,
    pub rounded_selection: bool,
    pub lsp_highlight_debounce: DelayMs,
    pub hover_popover_enabled: bool,
    pub hover_popover_delay: DelayMs,
    pub hover_popover_sticky: bool,
    pub hover_popover_hiding_delay: DelayMs,
    pub toolbar: Toolbar,
    pub scrollbar: Scrollbar,
    pub minimap: Minimap,
    pub gutter: Gutter,
    pub scroll_beyond_last_line: ScrollBeyondLastLine,
    pub vertical_scroll_margin: f64,
    pub autoscroll_on_clicks: bool,
    pub horizontal_scroll_margin: f32,
    pub scroll_sensitivity: f32,
    pub mouse_wheel_zoom: bool,
    pub fast_scroll_sensitivity: f32,
    pub sticky_scroll: StickyScroll,
    pub relative_line_numbers: RelativeLineNumbers,
    pub seed_search_query_from_cursor: SeedQuerySetting,
    pub use_smartcase_search: bool,
    pub multi_cursor_modifier: MultiCursorModifier,
    pub redact_private_values: bool,
    pub expand_excerpt_lines: u32,
    pub excerpt_context_lines: u32,
    pub middle_click_paste: bool,
    pub double_click_in_multibuffer: DoubleClickInMultibuffer,
    pub search_wrap: bool,
    pub search: SearchSettings,
    pub auto_signature_help: bool,
    pub show_signature_help_after_edits: bool,
    pub go_to_definition_fallback: GoToDefinitionFallback,
    pub go_to_definition_scroll_strategy: GoToDefinitionScrollStrategy,
    pub jupyter: Jupyter,
    pub snippet_sort_order: SnippetSortOrder,
    pub diagnostics_max_severity: Option<DiagnosticSeverity>,
    pub inline_code_actions: bool,
    pub drag_and_drop_selection: DragAndDropSelection,
    pub code_lens: CodeLens,
    pub lsp_document_colors: DocumentColorsRenderMode,
    pub lsp_document_links: bool,
    pub minimum_contrast_for_highlights: f32,
    pub completion_menu_scrollbar: ShowScrollbar,
    pub completion_detail_alignment: CompletionDetailAlignment,
    pub completion_menu_item_kind: CompletionMenuItemKind,
    pub diff_view_style: DiffViewStyle,
    pub minimum_split_diff_width: f32,
}
#[derive(Debug, Clone)]
pub struct Jupyter {
    /// Whether the Jupyter feature is enabled.
    ///
    /// Default: true
    pub enabled: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StickyScroll {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolbar {
    pub breadcrumbs: bool,
    pub quick_actions: bool,
    pub selections_menu: bool,
    pub agent_review: bool,
    pub code_actions: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Scrollbar {
    pub show: ShowScrollbar,
    pub git_diff: bool,
    pub selected_text: bool,
    pub selected_symbol: bool,
    pub search_results: bool,
    pub diagnostics: ScrollbarDiagnostics,
    pub cursors: bool,
    pub axes: ScrollbarAxes,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Minimap {
    pub show: ShowMinimap,
    pub display_in: DisplayIn,
    pub thumb: MinimapThumb,
    pub thumb_border: MinimapThumbBorder,
    pub current_line_highlight: Option<CurrentLineHighlight>,
    pub max_width_columns: num::NonZeroU32,
}

impl Minimap {
    pub fn minimap_enabled(&self) -> bool {
        self.show != ShowMinimap::Never
    }

    #[inline]
    pub fn on_active_editor(&self) -> bool {
        self.display_in == DisplayIn::ActiveEditor
    }

    pub fn with_show_override(self) -> Self {
        Self {
            show: ShowMinimap::Always,
            ..self
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Gutter {
    pub min_line_number_digits: usize,
    pub line_numbers: bool,
    pub runnables: bool,
    pub breakpoints: bool,
    pub bookmarks: bool,
    pub folds: bool,
}

/// Forcefully enable or disable the scrollbar for each axis
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ScrollbarAxes {
    /// When false, forcefully disables the horizontal scrollbar. Otherwise, obey other settings.
    ///
    /// Default: true
    pub horizontal: bool,

    /// When false, forcefully disables the vertical scrollbar. Otherwise, obey other settings.
    ///
    /// Default: true
    pub vertical: bool,
}

/// Whether to allow drag and drop text selection in buffer.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct DragAndDropSelection {
    /// When true, enables drag and drop text selection in buffer.
    ///
    /// Default: true
    pub enabled: bool,

    /// The delay in milliseconds that must elapse before drag and drop is allowed. Otherwise, a new text selection is created.
    ///
    /// Default: 300
    pub delay: DelayMs,
}

/// Default options for buffer and project search items.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct SearchSettings {
    /// Whether to show the project search button in the status bar.
    pub button: bool,
    /// Whether to only match on whole words.
    pub whole_word: bool,
    /// Whether to match case sensitively.
    pub case_sensitive: bool,
    /// Whether to include gitignored files in search results.
    pub include_ignored: bool,
    /// Whether to interpret the search query as a regular expression.
    pub regex: bool,
    /// Whether to center the cursor on each search match when navigating.
    pub center_on_match: bool,
}

impl EditorSettings {
    pub fn jupyter_enabled(cx: &App) -> bool {
        EditorSettings::get_global(cx).jupyter.enabled
    }
}

/// `NonZeroU32::new` is fallible, so the default is written as a `const` match
/// to keep it compile-time checked instead of unwrapping at runtime.
const DEFAULT_MINIMAP_MAX_WIDTH_COLUMNS: num::NonZeroU32 = match num::NonZeroU32::new(128) {
    Some(columns) => columns,
    None => num::NonZeroU32::MIN,
};

impl Settings for EditorSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let editor = content.editor.clone().unwrap_or_default();
        let toolbar = editor.toolbar.unwrap_or_default();
        let scrollbar = editor.scrollbar.unwrap_or_default();
        let scrollbar_axes = scrollbar.axes.unwrap_or_default();
        let minimap = editor.minimap.unwrap_or_default();
        let gutter = editor.gutter.unwrap_or_default();
        let sticky_scroll = editor.sticky_scroll.unwrap_or_default();
        let search = editor.search.unwrap_or_default();
        let jupyter = editor.jupyter.unwrap_or_default();
        let drag_and_drop_selection = editor.drag_and_drop_selection.unwrap_or_default();

        Self {
            cursor_blink: editor.cursor_blink.unwrap_or(true),
            cursor_shape: editor.cursor_shape.map(CursorShape::from),
            current_line_highlight: editor
                .current_line_highlight
                .map_or_else(CurrentLineHighlight::default, current_line_highlight_from),
            selection_highlight: editor.selection_highlight.unwrap_or(true),
            rounded_selection: editor.rounded_selection.unwrap_or(true),
            lsp_highlight_debounce: DelayMs(editor.lsp_highlight_debounce.unwrap_or(100)),
            hover_popover_enabled: editor.hover_popover_enabled.unwrap_or(true),
            hover_popover_delay: DelayMs(editor.hover_popover_delay.unwrap_or(50)),
            hover_popover_sticky: editor.hover_popover_sticky.unwrap_or(true),
            hover_popover_hiding_delay: DelayMs(editor.hover_popover_hiding_delay.unwrap_or(100)),
            toolbar: Toolbar {
                breadcrumbs: toolbar.breadcrumbs.unwrap_or(true),
                quick_actions: toolbar.quick_actions.unwrap_or(true),
                selections_menu: toolbar.selections_menu.unwrap_or(true),
                agent_review: toolbar.agent_review.unwrap_or(true),
                code_actions: toolbar.code_actions.unwrap_or(true),
            },
            scrollbar: Scrollbar {
                show: scrollbar
                    .show
                    .map_or(ShowScrollbar::Auto, ui_scrollbar_settings_from_raw),
                git_diff: scrollbar.git_diff.unwrap_or(true),
                selected_text: scrollbar.selected_text.unwrap_or(true),
                selected_symbol: scrollbar.selected_symbol.unwrap_or(true),
                search_results: scrollbar.search_results.unwrap_or(true),
                diagnostics: scrollbar.diagnostics.map_or_else(
                    ScrollbarDiagnostics::default,
                    |diagnostics| match diagnostics {
                        settings::ScrollbarDiagnostics::None => ScrollbarDiagnostics::None,
                        settings::ScrollbarDiagnostics::All => ScrollbarDiagnostics::All,
                        settings::ScrollbarDiagnostics::Error => ScrollbarDiagnostics::Error,
                        settings::ScrollbarDiagnostics::Warning => ScrollbarDiagnostics::Warning,
                        settings::ScrollbarDiagnostics::Information => {
                            ScrollbarDiagnostics::Information
                        }
                    },
                ),
                cursors: scrollbar.cursors.unwrap_or(true),
                axes: ScrollbarAxes {
                    horizontal: scrollbar_axes.horizontal.unwrap_or(true),
                    vertical: scrollbar_axes.vertical.unwrap_or(true),
                },
            },
            minimap: Minimap {
                show: minimap
                    .show
                    .map_or_else(ShowMinimap::default, |show| match show {
                        settings::ShowMinimap::Auto => ShowMinimap::Auto,
                        settings::ShowMinimap::Always => ShowMinimap::Always,
                        settings::ShowMinimap::Never => ShowMinimap::Never,
                    }),
                display_in: minimap
                    .display_in
                    .map_or_else(DisplayIn::default, |display_in| match display_in {
                        settings::DisplayIn::ActiveEditor => DisplayIn::ActiveEditor,
                        settings::DisplayIn::AllEditors => DisplayIn::AllEditors,
                    }),
                thumb: minimap
                    .thumb
                    .map_or_else(MinimapThumb::default, |thumb| match thumb {
                        settings::MinimapThumb::Always => MinimapThumb::Always,
                        settings::MinimapThumb::Hover => MinimapThumb::Hover,
                    }),
                thumb_border: minimap.thumb_border.map_or_else(
                    MinimapThumbBorder::default,
                    |thumb_border| match thumb_border {
                        settings::MinimapThumbBorder::Full => MinimapThumbBorder::Full,
                        settings::MinimapThumbBorder::LeftOnly => MinimapThumbBorder::LeftOnly,
                        settings::MinimapThumbBorder::LeftOpen => MinimapThumbBorder::LeftOpen,
                        settings::MinimapThumbBorder::RightOpen => MinimapThumbBorder::RightOpen,
                        settings::MinimapThumbBorder::None => MinimapThumbBorder::None,
                        settings::MinimapThumbBorder::Rounded => MinimapThumbBorder::Rounded,
                        settings::MinimapThumbBorder::Square => MinimapThumbBorder::Square,
                    },
                ),
                current_line_highlight: minimap
                    .current_line_highlight
                    .map(current_line_highlight_from),
                max_width_columns: minimap
                    .max_width_columns
                    .unwrap_or(DEFAULT_MINIMAP_MAX_WIDTH_COLUMNS),
            },
            gutter: Gutter {
                min_line_number_digits: gutter.min_line_number_digits.unwrap_or(2),
                line_numbers: gutter.line_numbers.unwrap_or(true),
                runnables: gutter.runnables.unwrap_or(true),
                breakpoints: gutter.breakpoints.unwrap_or(true),
                bookmarks: gutter.bookmarks.unwrap_or(true),
                folds: gutter.folds.unwrap_or(true),
            },
            scroll_beyond_last_line: editor.scroll_beyond_last_line.map_or_else(
                ScrollBeyondLastLine::default,
                |scroll_beyond_last_line| match scroll_beyond_last_line {
                    settings::ScrollBeyondLastLine::OnePage => ScrollBeyondLastLine::OnePage,
                    settings::ScrollBeyondLastLine::Off => ScrollBeyondLastLine::Off,
                    settings::ScrollBeyondLastLine::VerticalScrollMargin => {
                        ScrollBeyondLastLine::VerticalScrollMargin
                    }
                },
            ),
            vertical_scroll_margin: editor.vertical_scroll_margin.unwrap_or(0.0),
            autoscroll_on_clicks: editor.autoscroll_on_clicks.unwrap_or(true),
            horizontal_scroll_margin: editor.horizontal_scroll_margin.unwrap_or(0.0),
            scroll_sensitivity: editor.scroll_sensitivity.unwrap_or(1.0),
            mouse_wheel_zoom: editor.mouse_wheel_zoom.unwrap_or(false),
            fast_scroll_sensitivity: editor.fast_scroll_sensitivity.unwrap_or(2.0),
            sticky_scroll: StickyScroll {
                enabled: sticky_scroll.enabled.unwrap_or(false),
            },
            relative_line_numbers: editor.relative_line_numbers.map_or_else(
                RelativeLineNumbers::default,
                |relative_line_numbers| match relative_line_numbers {
                    settings::RelativeLineNumbers::Disabled => RelativeLineNumbers::Disabled,
                    settings::RelativeLineNumbers::Enabled => RelativeLineNumbers::Enabled,
                    settings::RelativeLineNumbers::Wrapped => RelativeLineNumbers::Wrapped,
                },
            ),
            seed_search_query_from_cursor: editor.seed_search_query_from_cursor.map_or_else(
                SeedQuerySetting::default,
                |seed_query| match seed_query {
                    settings::SeedQuerySetting::None => SeedQuerySetting::None,
                    settings::SeedQuerySetting::Selection => SeedQuerySetting::Selection,
                    settings::SeedQuerySetting::Line => SeedQuerySetting::Line,
                    settings::SeedQuerySetting::Surround => SeedQuerySetting::Surround,
                },
            ),
            use_smartcase_search: editor.use_smartcase_search.unwrap_or(true),
            multi_cursor_modifier: editor.multi_cursor_modifier.map_or_else(
                MultiCursorModifier::default,
                |multi_cursor_modifier| match multi_cursor_modifier {
                    settings::MultiCursorModifier::Alt => MultiCursorModifier::Alt,
                    settings::MultiCursorModifier::CmdOrCtrl => MultiCursorModifier::CmdOrCtrl,
                },
            ),
            redact_private_values: editor.redact_private_values.unwrap_or(false),
            expand_excerpt_lines: editor.expand_excerpt_lines.unwrap_or(3),
            excerpt_context_lines: editor.excerpt_context_lines.unwrap_or(2),
            middle_click_paste: editor.middle_click_paste.unwrap_or(false),
            double_click_in_multibuffer: editor.double_click_in_multibuffer.map_or_else(
                DoubleClickInMultibuffer::default,
                |double_click| match double_click {
                    settings::DoubleClickInMultibuffer::Select => DoubleClickInMultibuffer::Select,
                    settings::DoubleClickInMultibuffer::Open => DoubleClickInMultibuffer::Open,
                },
            ),
            search_wrap: editor.search_wrap.unwrap_or(true),
            search: SearchSettings {
                button: search.button.unwrap_or(true),
                whole_word: search.whole_word.unwrap_or(false),
                case_sensitive: search.case_sensitive.unwrap_or(false),
                include_ignored: search.include_ignored.unwrap_or(false),
                regex: search.regex.unwrap_or(false),
                center_on_match: search.center_on_match.unwrap_or(true),
            },
            auto_signature_help: editor.auto_signature_help.unwrap_or(true),
            show_signature_help_after_edits: editor
                .show_signature_help_after_edits
                .unwrap_or(false),
            go_to_definition_fallback: editor.go_to_definition_fallback.map_or_else(
                GoToDefinitionFallback::default,
                |fallback| match fallback {
                    settings::GoToDefinitionFallback::Lens => GoToDefinitionFallback::Lens,
                    settings::GoToDefinitionFallback::Search => GoToDefinitionFallback::Search,
                    settings::GoToDefinitionFallback::Never => GoToDefinitionFallback::Never,
                    settings::GoToDefinitionFallback::None => GoToDefinitionFallback::None,
                    settings::GoToDefinitionFallback::FindAllReferences => {
                        GoToDefinitionFallback::FindAllReferences
                    }
                },
            ),
            go_to_definition_scroll_strategy: editor.go_to_definition_scroll_strategy.map_or_else(
                GoToDefinitionScrollStrategy::default,
                |strategy| match strategy {
                    settings::GoToDefinitionScrollStrategy::Center => {
                        GoToDefinitionScrollStrategy::Center
                    }
                    settings::GoToDefinitionScrollStrategy::Minimum => {
                        GoToDefinitionScrollStrategy::Minimum
                    }
                    settings::GoToDefinitionScrollStrategy::Top => {
                        GoToDefinitionScrollStrategy::Top
                    }
                    settings::GoToDefinitionScrollStrategy::Preserve => {
                        GoToDefinitionScrollStrategy::Preserve
                    }
                },
            ),
            jupyter: Jupyter {
                enabled: jupyter.enabled.unwrap_or(true),
            },
            snippet_sort_order: editor.snippet_sort_order.map_or_else(
                SnippetSortOrder::default,
                |snippet_sort_order| match snippet_sort_order {
                    settings::SnippetSortOrder::Relevance => SnippetSortOrder::Relevance,
                    settings::SnippetSortOrder::Alphabetical => SnippetSortOrder::Alphabetical,
                    settings::SnippetSortOrder::Frequency => SnippetSortOrder::Frequency,
                },
            ),
            diagnostics_max_severity: editor.diagnostics_max_severity.map(
                |severity| match severity {
                    settings::DiagnosticSeverityContent::Off => DiagnosticSeverity::Off,
                    settings::DiagnosticSeverityContent::Error => DiagnosticSeverity::Error,
                    settings::DiagnosticSeverityContent::Warning => DiagnosticSeverity::Warning,
                    settings::DiagnosticSeverityContent::Info => DiagnosticSeverity::Info,
                    settings::DiagnosticSeverityContent::Hint => DiagnosticSeverity::Hint,
                    settings::DiagnosticSeverityContent::All => DiagnosticSeverity::Hint,
                },
            ),
            inline_code_actions: editor.inline_code_actions.unwrap_or(false),
            drag_and_drop_selection: DragAndDropSelection {
                enabled: drag_and_drop_selection.enabled.unwrap_or(true),
                delay: DelayMs(drag_and_drop_selection.delay.unwrap_or(300)),
            },
            code_lens: editor.code_lens.map_or_else(
                CodeLens::default,
                |code_lens| match code_lens {
                    settings::CodeLens::On => CodeLens::On,
                    settings::CodeLens::Off => CodeLens::Off,
                },
            ),
            lsp_document_colors: editor.lsp_document_colors.map_or_else(
                DocumentColorsRenderMode::default,
                |render_mode| match render_mode {
                    settings::DocumentColorsRenderMode::Inlay => DocumentColorsRenderMode::Inlay,
                    settings::DocumentColorsRenderMode::Background => {
                        DocumentColorsRenderMode::Background
                    }
                    settings::DocumentColorsRenderMode::Border => DocumentColorsRenderMode::Border,
                    settings::DocumentColorsRenderMode::Full => DocumentColorsRenderMode::Full,
                    settings::DocumentColorsRenderMode::None => DocumentColorsRenderMode::None,
                },
            ),
            lsp_document_links: editor.lsp_document_links.unwrap_or(true),
            minimum_contrast_for_highlights: editor.minimum_contrast_for_highlights.unwrap_or(0.15),
            completion_menu_scrollbar: editor
                .completion_menu_scrollbar
                .map_or(ShowScrollbar::Auto, ui_scrollbar_settings_from_raw),
            completion_detail_alignment: editor.completion_detail_alignment.map_or_else(
                CompletionDetailAlignment::default,
                |alignment| match alignment {
                    settings::CompletionDetailAlignment::Left => CompletionDetailAlignment::Left,
                    settings::CompletionDetailAlignment::Right => CompletionDetailAlignment::Right,
                },
            ),
            completion_menu_item_kind: editor.completion_menu_item_kind.map_or_else(
                CompletionMenuItemKind::default,
                |item_kind| match item_kind {
                    settings::CompletionMenuItemKind::All => CompletionMenuItemKind::All,
                    settings::CompletionMenuItemKind::Symbols => CompletionMenuItemKind::Symbols,
                    settings::CompletionMenuItemKind::Keywords => CompletionMenuItemKind::Keywords,
                    settings::CompletionMenuItemKind::Types => CompletionMenuItemKind::Types,
                },
            ),
            diff_view_style: editor.diff_view_style.map_or_else(
                DiffViewStyle::default,
                |diff_view_style| match diff_view_style {
                    settings::DiffViewStyle::Unified => DiffViewStyle::Unified,
                    settings::DiffViewStyle::Split => DiffViewStyle::Split,
                },
            ),
            minimum_split_diff_width: editor.minimum_split_diff_width.unwrap_or(100.0),
        }
    }
}

fn current_line_highlight_from(
    current_line_highlight: settings::CurrentLineHighlight,
) -> CurrentLineHighlight {
    match current_line_highlight {
        settings::CurrentLineHighlight::Gutter => CurrentLineHighlight::Gutter,
        settings::CurrentLineHighlight::Line => CurrentLineHighlight::Line,
        settings::CurrentLineHighlight::All => CurrentLineHighlight::All,
        settings::CurrentLineHighlight::None => CurrentLineHighlight::None,
    }
}

#[derive(Default)]
pub struct EditorSettingsScrollbarProxy;

impl ui::scrollbars::ScrollbarVisibility for EditorSettingsScrollbarProxy {
    fn visibility(&self, cx: &App) -> ShowScrollbar {
        EditorSettings::get_global(cx).scrollbar.show
    }
}

pub fn ui_scrollbar_settings_from_raw(
    value: settings::ShowScrollbar,
) -> ui::scrollbars::ShowScrollbar {
    match value {
        settings::ShowScrollbar::Auto => ShowScrollbar::Auto,
        settings::ShowScrollbar::System => ShowScrollbar::System,
        settings::ShowScrollbar::Always => ShowScrollbar::Always,
        settings::ShowScrollbar::Never => ShowScrollbar::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::{
        DragAndDropSelectionContent, EditorSettingsContent, GutterContent, JupyterContent,
        MinimapContent, ScrollbarAxesContent, ScrollbarContent, SearchSettingsContent,
        SettingsContent, StickyScrollContent, ToolbarContent,
    };

    fn content_with_editor_settings() -> SettingsContent {
        let mut content = SettingsContent::default();
        content.editor = Some(EditorSettingsContent {
            cursor_blink: Some(false),
            cursor_shape: Some(settings::CursorShape::Underline),
            current_line_highlight: Some(settings::CurrentLineHighlight::All),
            lsp_highlight_debounce: Some(250),
            hover_popover_delay: Some(75),
            toolbar: Some(ToolbarContent {
                breadcrumbs: Some(false),
                quick_actions: Some(false),
                ..Default::default()
            }),
            scrollbar: Some(ScrollbarContent {
                show: Some(settings::ShowScrollbar::Never),
                diagnostics: Some(settings::ScrollbarDiagnostics::Error),
                cursors: Some(false),
                axes: Some(ScrollbarAxesContent {
                    horizontal: Some(false),
                    vertical: Some(true),
                }),
                ..Default::default()
            }),
            minimap: Some(MinimapContent {
                show: Some(settings::ShowMinimap::Always),
                thumb: Some(settings::MinimapThumb::Hover),
                current_line_highlight: Some(settings::CurrentLineHighlight::Gutter),
                max_width_columns: num::NonZeroU32::new(64),
                ..Default::default()
            }),
            gutter: Some(GutterContent {
                min_line_number_digits: Some(5),
                line_numbers: Some(false),
                ..Default::default()
            }),
            scroll_beyond_last_line: Some(settings::ScrollBeyondLastLine::Off),
            vertical_scroll_margin: Some(8.0),
            horizontal_scroll_margin: Some(4.0),
            scroll_sensitivity: Some(0.5),
            fast_scroll_sensitivity: Some(6.0),
            sticky_scroll: Some(StickyScrollContent {
                enabled: Some(true),
            }),
            relative_line_numbers: Some(settings::RelativeLineNumbers::Wrapped),
            seed_search_query_from_cursor: Some(settings::SeedQuerySetting::Selection),
            use_smartcase_search: Some(false),
            multi_cursor_modifier: Some(settings::MultiCursorModifier::CmdOrCtrl),
            expand_excerpt_lines: Some(7),
            excerpt_context_lines: Some(9),
            double_click_in_multibuffer: Some(settings::DoubleClickInMultibuffer::Open),
            search_wrap: Some(false),
            search: Some(SearchSettingsContent {
                button: Some(false),
                whole_word: Some(true),
                case_sensitive: Some(true),
                include_ignored: Some(true),
                regex: Some(true),
                center_on_match: Some(false),
            }),
            go_to_definition_fallback: Some(settings::GoToDefinitionFallback::FindAllReferences),
            go_to_definition_scroll_strategy: Some(settings::GoToDefinitionScrollStrategy::Top),
            jupyter: Some(JupyterContent {
                enabled: Some(false),
            }),
            snippet_sort_order: Some(settings::SnippetSortOrder::Alphabetical),
            diagnostics_max_severity: Some(settings::DiagnosticSeverityContent::Warning),
            drag_and_drop_selection: Some(DragAndDropSelectionContent {
                enabled: Some(false),
                delay: Some(900),
            }),
            code_lens: Some(settings::CodeLens::Off),
            lsp_document_colors: Some(settings::DocumentColorsRenderMode::Border),
            lsp_document_links: Some(false),
            minimum_contrast_for_highlights: Some(0.75),
            completion_menu_scrollbar: Some(settings::ShowScrollbar::Always),
            completion_detail_alignment: Some(settings::CompletionDetailAlignment::Right),
            completion_menu_item_kind: Some(settings::CompletionMenuItemKind::Symbols),
            diff_view_style: Some(settings::DiffViewStyle::Split),
            minimum_split_diff_width: Some(640.0),
            ..Default::default()
        });
        content
    }

    #[test]
    fn test_reads_top_level_editor_settings_from_content() {
        let settings = EditorSettings::from_settings(&content_with_editor_settings());

        assert!(!settings.cursor_blink);
        assert_eq!(settings.cursor_shape, Some(CursorShape::Underline));
        assert_eq!(settings.current_line_highlight, CurrentLineHighlight::All);
        assert_eq!(settings.lsp_highlight_debounce, DelayMs(250));
        assert_eq!(settings.hover_popover_delay, DelayMs(75));
        assert_eq!(settings.scroll_beyond_last_line, ScrollBeyondLastLine::Off);
        assert_eq!(settings.vertical_scroll_margin, 8.0);
        assert_eq!(settings.horizontal_scroll_margin, 4.0);
        assert_eq!(settings.scroll_sensitivity, 0.5);
        assert_eq!(settings.fast_scroll_sensitivity, 6.0);
        assert_eq!(settings.relative_line_numbers, RelativeLineNumbers::Wrapped);
        assert_eq!(
            settings.seed_search_query_from_cursor,
            SeedQuerySetting::Selection
        );
        assert!(!settings.use_smartcase_search);
        assert_eq!(
            settings.multi_cursor_modifier,
            MultiCursorModifier::CmdOrCtrl
        );
        assert_eq!(settings.expand_excerpt_lines, 7);
        assert_eq!(settings.excerpt_context_lines, 9);
        assert_eq!(
            settings.double_click_in_multibuffer,
            DoubleClickInMultibuffer::Open
        );
        assert!(!settings.search_wrap);
        assert_eq!(
            settings.go_to_definition_fallback,
            GoToDefinitionFallback::FindAllReferences
        );
        assert_eq!(
            settings.go_to_definition_scroll_strategy,
            GoToDefinitionScrollStrategy::Top
        );
        assert_eq!(settings.snippet_sort_order, SnippetSortOrder::Alphabetical);
        assert_eq!(
            settings.diagnostics_max_severity,
            Some(DiagnosticSeverity::Warning)
        );
        assert_eq!(settings.code_lens, CodeLens::Off);
        assert_eq!(
            settings.lsp_document_colors,
            DocumentColorsRenderMode::Border
        );
        assert!(!settings.lsp_document_links);
        assert_eq!(settings.minimum_contrast_for_highlights, 0.75);
        assert_eq!(settings.completion_menu_scrollbar, ShowScrollbar::Always);
        assert_eq!(
            settings.completion_detail_alignment,
            CompletionDetailAlignment::Right
        );
        assert_eq!(
            settings.completion_menu_item_kind,
            CompletionMenuItemKind::Symbols
        );
        assert_eq!(settings.diff_view_style, DiffViewStyle::Split);
        assert_eq!(settings.minimum_split_diff_width, 640.0);
    }

    #[test]
    fn test_reads_nested_editor_settings_from_content() {
        let settings = EditorSettings::from_settings(&content_with_editor_settings());

        assert!(!settings.toolbar.breadcrumbs);
        assert!(!settings.toolbar.quick_actions);
        // Sibling fields the user did not mention keep their own defaults.
        assert!(settings.toolbar.selections_menu);

        assert_eq!(settings.scrollbar.show, ShowScrollbar::Never);
        assert_eq!(settings.scrollbar.diagnostics, ScrollbarDiagnostics::Error);
        assert!(!settings.scrollbar.cursors);
        assert!(!settings.scrollbar.axes.horizontal);
        assert!(settings.scrollbar.axes.vertical);
        assert!(settings.scrollbar.git_diff);

        assert_eq!(settings.minimap.show, ShowMinimap::Always);
        assert_eq!(settings.minimap.thumb, MinimapThumb::Hover);
        assert_eq!(
            settings.minimap.current_line_highlight,
            Some(CurrentLineHighlight::Gutter)
        );
        assert_eq!(settings.minimap.max_width_columns.get(), 64);

        assert_eq!(settings.gutter.min_line_number_digits, 5);
        assert!(!settings.gutter.line_numbers);
        assert!(settings.gutter.runnables);

        assert!(settings.sticky_scroll.enabled);
        assert!(!settings.jupyter.enabled);

        assert!(!settings.drag_and_drop_selection.enabled);
        assert_eq!(settings.drag_and_drop_selection.delay, DelayMs(900));
    }

    #[test]
    fn test_reads_search_settings_from_content() {
        let settings = EditorSettings::from_settings(&content_with_editor_settings());

        assert_eq!(
            settings.search,
            SearchSettings {
                button: false,
                whole_word: true,
                case_sensitive: true,
                include_ignored: true,
                regex: true,
                center_on_match: false,
            }
        );
    }

    #[test]
    fn test_falls_back_to_defaults_when_unset() {
        let settings = EditorSettings::from_settings(&SettingsContent::default());

        assert!(settings.cursor_blink);
        assert_eq!(settings.cursor_shape, None);
        assert_eq!(settings.current_line_highlight, CurrentLineHighlight::Line);
        assert!(settings.selection_highlight);
        assert!(settings.rounded_selection);
        assert_eq!(settings.lsp_highlight_debounce, DelayMs(100));
        assert!(settings.hover_popover_enabled);
        assert_eq!(settings.hover_popover_delay, DelayMs(50));
        assert!(settings.hover_popover_sticky);
        assert_eq!(settings.hover_popover_hiding_delay, DelayMs(100));
        assert_eq!(
            settings.scroll_beyond_last_line,
            ScrollBeyondLastLine::OnePage
        );
        assert_eq!(settings.vertical_scroll_margin, 0.0);
        assert!(settings.autoscroll_on_clicks);
        assert_eq!(settings.horizontal_scroll_margin, 0.0);
        assert_eq!(settings.scroll_sensitivity, 1.0);
        assert!(!settings.mouse_wheel_zoom);
        assert_eq!(settings.fast_scroll_sensitivity, 2.0);
        assert_eq!(
            settings.relative_line_numbers,
            RelativeLineNumbers::Disabled
        );
        assert_eq!(
            settings.seed_search_query_from_cursor,
            SeedQuerySetting::None
        );
        assert!(settings.use_smartcase_search);
        assert_eq!(settings.multi_cursor_modifier, MultiCursorModifier::Alt);
        assert!(!settings.redact_private_values);
        assert_eq!(settings.expand_excerpt_lines, 3);
        assert_eq!(settings.excerpt_context_lines, 2);
        assert!(!settings.middle_click_paste);
        assert_eq!(
            settings.double_click_in_multibuffer,
            DoubleClickInMultibuffer::Select
        );
        assert!(settings.search_wrap);
        assert_eq!(
            settings.search,
            SearchSettings {
                button: true,
                whole_word: false,
                case_sensitive: false,
                include_ignored: false,
                regex: false,
                center_on_match: true,
            }
        );
        assert!(settings.auto_signature_help);
        assert!(!settings.show_signature_help_after_edits);
        assert_eq!(
            settings.go_to_definition_fallback,
            GoToDefinitionFallback::Lens
        );
        assert_eq!(
            settings.go_to_definition_scroll_strategy,
            GoToDefinitionScrollStrategy::Center
        );
        assert!(settings.jupyter.enabled);
        assert_eq!(settings.snippet_sort_order, SnippetSortOrder::Relevance);
        assert_eq!(settings.diagnostics_max_severity, None);
        assert!(!settings.inline_code_actions);
        assert!(settings.drag_and_drop_selection.enabled);
        assert_eq!(settings.drag_and_drop_selection.delay, DelayMs(300));
        assert_eq!(settings.code_lens, CodeLens::On);
        assert_eq!(
            settings.lsp_document_colors,
            DocumentColorsRenderMode::Inlay
        );
        assert!(settings.lsp_document_links);
        assert_eq!(settings.minimum_contrast_for_highlights, 0.15);
        assert_eq!(settings.completion_menu_scrollbar, ShowScrollbar::Auto);
        assert_eq!(
            settings.completion_detail_alignment,
            CompletionDetailAlignment::Left
        );
        assert_eq!(
            settings.completion_menu_item_kind,
            CompletionMenuItemKind::All
        );
        assert_eq!(settings.diff_view_style, DiffViewStyle::Unified);
        assert_eq!(settings.minimum_split_diff_width, 100.0);

        assert!(settings.toolbar.breadcrumbs);
        assert!(settings.toolbar.quick_actions);
        assert!(settings.toolbar.selections_menu);
        assert!(settings.toolbar.agent_review);
        assert!(settings.toolbar.code_actions);

        assert_eq!(settings.scrollbar.show, ShowScrollbar::Auto);
        assert!(settings.scrollbar.git_diff);
        assert!(settings.scrollbar.selected_text);
        assert!(settings.scrollbar.selected_symbol);
        assert!(settings.scrollbar.search_results);
        assert_eq!(settings.scrollbar.diagnostics, ScrollbarDiagnostics::None);
        assert!(settings.scrollbar.cursors);
        assert!(settings.scrollbar.axes.horizontal);
        assert!(settings.scrollbar.axes.vertical);

        assert_eq!(settings.minimap.show, ShowMinimap::Auto);
        assert_eq!(settings.minimap.display_in, DisplayIn::ActiveEditor);
        assert_eq!(settings.minimap.thumb, MinimapThumb::Always);
        assert_eq!(settings.minimap.thumb_border, MinimapThumbBorder::Full);
        assert_eq!(settings.minimap.current_line_highlight, None);
        assert_eq!(settings.minimap.max_width_columns.get(), 128);

        assert_eq!(settings.gutter.min_line_number_digits, 2);
        assert!(settings.gutter.line_numbers);
        assert!(settings.gutter.runnables);
        assert!(settings.gutter.breakpoints);
        assert!(settings.gutter.bookmarks);
        assert!(settings.gutter.folds);

        assert!(!settings.sticky_scroll.enabled);
    }

    /// Guards the JSON shape written in `assets/settings/default.json`: every
    /// key there has to deserialize, and the values have to agree with the
    /// fallbacks `from_settings` applies when the key is absent.
    ///
    /// Parsed through `UserSettingsContent` rather than `SettingsContent` so the
    /// test walks the same flattening that `SettingsStore` uses at startup.
    #[test]
    fn test_default_json_editor_section_matches_fallbacks() {
        let user_content = <settings::UserSettingsContent as settings::RootUserSettings>::parse_json_with_comments(
            settings::default_settings().as_ref(),
        )
        .expect("assets/settings/default.json should parse");
        let content = *user_content.content;
        let editor = content
            .editor
            .as_ref()
            .expect("assets/settings/default.json should define an `editor` section");

        let from_default_json = EditorSettings::from_settings(&content);
        let from_fallbacks = EditorSettings::from_settings(&SettingsContent::default());

        assert_eq!(
            from_default_json.cursor_blink, from_fallbacks.cursor_blink,
            "editor.cursor_blink in default.json disagrees with the Rust fallback"
        );
        assert_eq!(from_default_json.cursor_shape, from_fallbacks.cursor_shape);
        assert_eq!(
            from_default_json.current_line_highlight,
            from_fallbacks.current_line_highlight
        );
        assert_eq!(
            from_default_json.lsp_highlight_debounce,
            from_fallbacks.lsp_highlight_debounce
        );
        assert_eq!(
            from_default_json.hover_popover_delay,
            from_fallbacks.hover_popover_delay
        );
        assert_eq!(from_default_json.toolbar, from_fallbacks.toolbar);
        assert_eq!(from_default_json.scrollbar, from_fallbacks.scrollbar);
        assert_eq!(from_default_json.minimap, from_fallbacks.minimap);
        assert_eq!(from_default_json.gutter, from_fallbacks.gutter);
        assert_eq!(
            from_default_json.scroll_beyond_last_line,
            from_fallbacks.scroll_beyond_last_line
        );
        assert_eq!(
            from_default_json.vertical_scroll_margin,
            from_fallbacks.vertical_scroll_margin
        );
        assert_eq!(
            from_default_json.sticky_scroll,
            from_fallbacks.sticky_scroll
        );
        assert_eq!(
            from_default_json.relative_line_numbers,
            from_fallbacks.relative_line_numbers
        );
        assert_eq!(
            from_default_json.seed_search_query_from_cursor,
            from_fallbacks.seed_search_query_from_cursor
        );
        assert_eq!(from_default_json.search, from_fallbacks.search);
        assert_eq!(
            from_default_json.diagnostics_max_severity,
            from_fallbacks.diagnostics_max_severity
        );
        assert_eq!(
            from_default_json.drag_and_drop_selection,
            from_fallbacks.drag_and_drop_selection
        );
        assert_eq!(
            from_default_json.diff_view_style,
            from_fallbacks.diff_view_style
        );
        assert_eq!(
            from_default_json.minimum_split_diff_width,
            from_fallbacks.minimum_split_diff_width
        );

        // A misspelled key deserializes to `None`, so spot-check that the
        // nested sections really were populated rather than skipped.
        assert!(editor.search.is_some());
        assert!(editor.scrollbar.is_some());
        assert!(editor.minimap.is_some());
        assert!(editor.gutter.is_some());
        assert!(editor.toolbar.is_some());
    }

    #[test]
    fn test_parses_editor_settings_from_json() {
        let content: SettingsContent = settings::parse_json_with_comments(
            r#"{
                "editor": {
                    "cursor_blink": false,
                    "current_line_highlight": "gutter",
                    "relative_line_numbers": "wrapped",
                    "scrollbar": { "show": "never", "axes": { "horizontal": false } },
                    "search": {
                        "whole_word": true,
                        "regex": true,
                        "center_on_match": false
                    }
                }
            }"#,
        )
        .expect("editor settings should parse");

        let settings = EditorSettings::from_settings(&content);

        assert!(!settings.cursor_blink);
        assert_eq!(
            settings.current_line_highlight,
            CurrentLineHighlight::Gutter
        );
        assert_eq!(settings.relative_line_numbers, RelativeLineNumbers::Wrapped);
        assert_eq!(settings.scrollbar.show, ShowScrollbar::Never);
        assert!(!settings.scrollbar.axes.horizontal);
        assert!(settings.scrollbar.axes.vertical);
        assert!(settings.search.whole_word);
        assert!(settings.search.regex);
        assert!(!settings.search.center_on_match);
        assert!(!settings.search.case_sensitive);
        assert!(settings.search.button);
    }

    /// A user who overrides one key must keep the rest of `default.json`, which
    /// only holds if the nested `Option`s merge recursively instead of being
    /// replaced wholesale.
    #[test]
    fn test_user_override_merges_into_default_json() {
        use settings::MergeFromTrait as _;

        let mut merged = <settings::UserSettingsContent as settings::RootUserSettings>::parse_json_with_comments(
            settings::default_settings().as_ref(),
        )
        .map(|user_content| *user_content.content)
        .expect("assets/settings/default.json should parse");

        let user: SettingsContent = settings::parse_json_with_comments(
            r#"{ "editor": { "search": { "regex": true }, "cursor_blink": false } }"#,
        )
        .expect("user settings should parse");
        merged.merge_from(&user);

        let settings = EditorSettings::from_settings(&merged);

        assert!(settings.search.regex);
        assert!(!settings.cursor_blink);
        // Sibling keys defined only in default.json survive the merge.
        assert!(settings.search.button);
        assert!(settings.search.center_on_match);
        assert!(!settings.search.whole_word);
        assert_eq!(settings.minimap.max_width_columns.get(), 128);
    }
}
