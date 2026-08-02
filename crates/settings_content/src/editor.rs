use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};
use std::num::NonZeroU32;

use crate::{CursorShape, ShowScrollbar};

/// 编辑器与搜索设置 (spec §16 Plan 16)
///
/// 字段与 `editor::EditorSettings` 一一对应。
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct EditorSettingsContent {
    /// Whether the cursor blinks in the editor.
    ///
    /// Default: true
    pub cursor_blink: Option<bool>,

    /// Cursor shape for the default editor.
    ///
    /// Default: none, which follows the primary cursor shape.
    pub cursor_shape: Option<CursorShape>,

    /// How to highlight the current line in the editor.
    ///
    /// Default: line
    pub current_line_highlight: Option<CurrentLineHighlight>,

    /// Whether to highlight all occurrences of the selected text.
    ///
    /// Default: true
    pub selection_highlight: Option<bool>,

    /// Whether the text selection should have rounded corners.
    ///
    /// Default: true
    pub rounded_selection: Option<bool>,

    /// The debounce delay in milliseconds before querying highlights from the language server.
    ///
    /// Default: 100
    pub lsp_highlight_debounce: Option<u64>,

    /// Whether to show the informational hover box when moving the mouse over symbols.
    ///
    /// Default: true
    pub hover_popover_enabled: Option<bool>,

    /// Time in milliseconds before the hover popover is shown.
    ///
    /// Default: 50
    pub hover_popover_delay: Option<u64>,

    /// Whether the hover popover stays visible while the mouse moves towards it.
    ///
    /// Default: true
    pub hover_popover_sticky: Option<bool>,

    /// Time in milliseconds before the hover popover is hidden again.
    ///
    /// Default: 100
    pub hover_popover_hiding_delay: Option<u64>,

    /// Toolbar related settings.
    pub toolbar: Option<ToolbarContent>,

    /// Scrollbar related settings.
    pub scrollbar: Option<ScrollbarContent>,

    /// Minimap related settings.
    pub minimap: Option<MinimapContent>,

    /// Gutter related settings.
    pub gutter: Option<GutterContent>,

    /// Whether the editor will scroll beyond the last line.
    ///
    /// Default: one_page
    pub scroll_beyond_last_line: Option<ScrollBeyondLastLine>,

    /// The number of lines to keep above/below the cursor when scrolling.
    ///
    /// Default: 0.0
    pub vertical_scroll_margin: Option<f64>,

    /// Whether to scroll the editor to keep the cursor visible after a click.
    ///
    /// Default: true
    pub autoscroll_on_clicks: Option<bool>,

    /// The number of characters to keep on either side of the cursor when scrolling horizontally.
    ///
    /// Default: 0.0
    pub horizontal_scroll_margin: Option<f32>,

    /// Scroll sensitivity multiplier.
    ///
    /// Default: 1.0
    pub scroll_sensitivity: Option<f32>,

    /// Whether to zoom the buffer font when scrolling with a modifier held.
    ///
    /// Default: false
    pub mouse_wheel_zoom: Option<bool>,

    /// Scroll sensitivity multiplier applied while the fast-scroll modifier is held.
    ///
    /// Default: 2.0
    pub fast_scroll_sensitivity: Option<f32>,

    /// Sticky scroll related settings.
    pub sticky_scroll: Option<StickyScrollContent>,

    /// Whether line numbers are relative to the cursor line.
    ///
    /// Default: disabled
    pub relative_line_numbers: Option<RelativeLineNumbers>,

    /// When to populate a new search's query based on the text under the cursor.
    ///
    /// Default: none
    pub seed_search_query_from_cursor: Option<SeedQuerySetting>,

    /// Whether a search query with only lowercase characters matches case-insensitively.
    ///
    /// Default: true
    pub use_smartcase_search: Option<bool>,

    /// The modifier that adds a cursor when held during a click.
    ///
    /// Default: alt
    pub multi_cursor_modifier: Option<MultiCursorModifier>,

    /// Whether to hide the values of variables in private files.
    ///
    /// Default: false
    pub redact_private_values: Option<bool>,

    /// How many lines to expand an excerpt by when the expand action is used.
    ///
    /// Default: 3
    pub expand_excerpt_lines: Option<u32>,

    /// How many context lines an excerpt is created with.
    ///
    /// Default: 3
    pub excerpt_context_lines: Option<u32>,

    /// Whether a middle click pastes the primary selection.
    ///
    /// Default: false
    pub middle_click_paste: Option<bool>,

    /// What a double click in a multibuffer does.
    ///
    /// Default: select
    pub double_click_in_multibuffer: Option<DoubleClickInMultibuffer>,

    /// Whether a search wraps around after reaching the last match.
    ///
    /// Default: true
    pub search_wrap: Option<bool>,

    /// Default options for buffer and project search items.
    pub search: Option<SearchSettingsContent>,

    /// Whether to automatically show a signature help popover while typing.
    ///
    /// Default: true
    pub auto_signature_help: Option<bool>,

    /// Whether to show the signature help popover after completing an edit.
    ///
    /// Default: false
    pub show_signature_help_after_edits: Option<bool>,

    /// What to do when go-to-definition finds no definition.
    ///
    /// Default: lens
    pub go_to_definition_fallback: Option<GoToDefinitionFallback>,

    /// How to scroll the target into view after a go-to-definition.
    ///
    /// Default: center
    pub go_to_definition_scroll_strategy: Option<GoToDefinitionScrollStrategy>,

    /// Jupyter related settings.
    pub jupyter: Option<JupyterContent>,

    /// How to sort snippets against other completion entries.
    ///
    /// Default: relevance
    pub snippet_sort_order: Option<SnippetSortOrder>,

    /// The maximum severity of diagnostics to render in the editor.
    ///
    /// Default: none, which uses the editor's own limit.
    pub diagnostics_max_severity: Option<DiagnosticSeverityContent>,

    /// Whether to show code action indicators inline instead of in the gutter.
    ///
    /// Default: false
    pub inline_code_actions: Option<bool>,

    /// Drag and drop text selection related settings.
    pub drag_and_drop_selection: Option<DragAndDropSelectionContent>,

    /// Whether to show code lens.
    ///
    /// Default: on
    pub code_lens: Option<CodeLens>,

    /// How to render document colors reported by the language server.
    ///
    /// Default: inlay
    pub lsp_document_colors: Option<DocumentColorsRenderMode>,

    /// Whether to render document links reported by the language server.
    ///
    /// Default: true
    pub lsp_document_links: Option<bool>,

    /// The minimum contrast highlighted text must keep against its background.
    ///
    /// Default: 0.15
    pub minimum_contrast_for_highlights: Option<f32>,

    /// When to show the scrollbar in the completion menu.
    ///
    /// Default: auto
    pub completion_menu_scrollbar: Option<ShowScrollbar>,

    /// Which side of a completion entry the detail text is aligned to.
    ///
    /// Default: left
    pub completion_detail_alignment: Option<CompletionDetailAlignment>,

    /// Which kinds of entries the completion menu shows.
    ///
    /// Default: all
    pub completion_menu_item_kind: Option<CompletionMenuItemKind>,

    /// How to lay out a diff.
    ///
    /// Default: unified
    pub diff_view_style: Option<DiffViewStyle>,

    /// The minimum width in pixels at which a diff is laid out side by side.
    ///
    /// Default: 480.0
    pub minimum_split_diff_width: Option<f32>,
}

/// Default options for buffer and project search items.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct SearchSettingsContent {
    /// Whether to show the project search button in the status bar.
    ///
    /// Default: true
    pub button: Option<bool>,

    /// Whether to only match on whole words.
    ///
    /// Default: false
    pub whole_word: Option<bool>,

    /// Whether to match case sensitively.
    ///
    /// Default: false
    pub case_sensitive: Option<bool>,

    /// Whether to include gitignored files in search results.
    ///
    /// Default: false
    pub include_ignored: Option<bool>,

    /// Whether to interpret the search query as a regular expression.
    ///
    /// Default: false
    pub regex: Option<bool>,

    /// Whether to center the cursor on each search match when navigating.
    ///
    /// Default: true
    pub center_on_match: Option<bool>,
}

/// Toolbar related settings.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ToolbarContent {
    /// Whether to display breadcrumbs in the editor toolbar.
    ///
    /// Default: true
    pub breadcrumbs: Option<bool>,

    /// Whether to display quick action buttons in the editor toolbar.
    ///
    /// Default: true
    pub quick_actions: Option<bool>,

    /// Whether to display the selections menu in the editor toolbar.
    ///
    /// Default: true
    pub selections_menu: Option<bool>,

    /// Whether to display agent review buttons in the editor toolbar.
    ///
    /// Default: true
    pub agent_review: Option<bool>,

    /// Whether to display code action buttons in the editor toolbar.
    ///
    /// Default: true
    pub code_actions: Option<bool>,
}

/// Scrollbar related settings.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ScrollbarContent {
    /// When to show the scrollbar in the editor.
    ///
    /// Default: auto
    pub show: Option<ShowScrollbar>,

    /// Whether to show git diff indicators in the scrollbar.
    ///
    /// Default: true
    pub git_diff: Option<bool>,

    /// Whether to show buffer search result indicators in the scrollbar.
    ///
    /// Default: true
    pub search_results: Option<bool>,

    /// Whether to show selected text occurrences in the scrollbar.
    ///
    /// Default: true
    pub selected_text: Option<bool>,

    /// Whether to show selected symbol occurrences in the scrollbar.
    ///
    /// Default: true
    pub selected_symbol: Option<bool>,

    /// Which diagnostic indicators to show in the scrollbar.
    ///
    /// Default: none
    pub diagnostics: Option<ScrollbarDiagnostics>,

    /// Whether to show cursor positions in the scrollbar.
    ///
    /// Default: true
    pub cursors: Option<bool>,

    /// Forcefully enable or disable the scrollbar for each axis.
    pub axes: Option<ScrollbarAxesContent>,
}

/// Forcefully enable or disable the scrollbar for each axis.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ScrollbarAxesContent {
    /// When false, forcefully disables the horizontal scrollbar. Otherwise, obey other settings.
    ///
    /// Default: true
    pub horizontal: Option<bool>,

    /// When false, forcefully disables the vertical scrollbar. Otherwise, obey other settings.
    ///
    /// Default: true
    pub vertical: Option<bool>,
}

/// Minimap related settings.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct MinimapContent {
    /// When to show the minimap in the editor.
    ///
    /// Default: auto
    pub show: Option<ShowMinimap>,

    /// Where to show the minimap in the editor.
    ///
    /// Default: active_editor
    pub display_in: Option<DisplayIn>,

    /// When to show the minimap thumb.
    ///
    /// Default: always
    pub thumb: Option<MinimapThumb>,

    /// How the minimap thumb border is drawn.
    ///
    /// Default: full
    pub thumb_border: Option<MinimapThumbBorder>,

    /// How to highlight the current line in the minimap.
    ///
    /// Default: none, which inherits the editor's current line highlight.
    pub current_line_highlight: Option<CurrentLineHighlight>,

    /// Maximum number of columns to display in the minimap.
    ///
    /// Default: 128
    pub max_width_columns: Option<NonZeroU32>,
}

/// Gutter related settings.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct GutterContent {
    /// Minimum number of characters to reserve space for in the gutter.
    ///
    /// Default: 2
    pub min_line_number_digits: Option<usize>,

    /// Whether to show line numbers in the gutter.
    ///
    /// Default: true
    pub line_numbers: Option<bool>,

    /// Whether to show runnable buttons in the gutter.
    ///
    /// Default: true
    pub runnables: Option<bool>,

    /// Whether to show breakpoints in the gutter.
    ///
    /// Default: true
    pub breakpoints: Option<bool>,

    /// Whether to show bookmarks in the gutter.
    ///
    /// Default: true
    pub bookmarks: Option<bool>,

    /// Whether to show fold buttons in the gutter.
    ///
    /// Default: true
    pub folds: Option<bool>,
}

/// Sticky scroll related settings.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct StickyScrollContent {
    /// Whether to pin enclosing scopes to the top of the editor while scrolling.
    ///
    /// Default: false
    pub enabled: Option<bool>,
}

/// Jupyter related settings.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct JupyterContent {
    /// Whether the Jupyter feature is enabled.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Whether to allow drag and drop text selection in buffer.
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct DragAndDropSelectionContent {
    /// When true, enables drag and drop text selection in buffer.
    ///
    /// Default: true
    pub enabled: Option<bool>,

    /// The delay in milliseconds that must elapse before drag and drop is allowed.
    /// Otherwise, a new text selection is created.
    ///
    /// Default: 300
    pub delay: Option<u64>,
}

/// How to highlight the current line in the editor.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum CurrentLineHighlight {
    /// Highlight the gutter area of the current line.
    Gutter,
    /// Highlight the text of the current line.
    #[default]
    Line,
    /// Highlight the whole current line.
    All,
    /// Do not highlight the current line.
    None,
}

/// Whether the editor will scroll beyond the last line.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ScrollBeyondLastLine {
    /// The editor will scroll beyond the last line by up to one page.
    #[default]
    OnePage,
    /// The editor will not scroll beyond the last line.
    Off,
    /// The editor will scroll beyond the last line by the vertical scroll margin.
    VerticalScrollMargin,
}

/// Which diagnostic indicators to show in the scrollbar.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ScrollbarDiagnostics {
    /// Do not show any diagnostics.
    #[default]
    None,
    /// Show all diagnostics.
    All,
    /// Show only errors.
    Error,
    /// Show errors and warnings.
    Warning,
    /// Show errors, warnings and information.
    Information,
}

/// When to show the minimap in the editor.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ShowMinimap {
    /// Follow the editor's own heuristics.
    #[default]
    Auto,
    /// Always show the minimap.
    Always,
    /// Never show the minimap.
    Never,
}

/// Where to show the minimap in the editor.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DisplayIn {
    /// Show the minimap in the active editor only.
    #[default]
    ActiveEditor,
    /// Show the minimap in all editors.
    AllEditors,
}

/// When to show the minimap thumb.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum MinimapThumb {
    /// Always show the thumb.
    #[default]
    Always,
    /// Show the thumb while the mouse hovers the minimap.
    Hover,
}

/// How the minimap thumb border is drawn.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum MinimapThumbBorder {
    /// Draw a border on every side of the thumb.
    #[default]
    Full,
    /// Draw a border on the left side only.
    LeftOnly,
    /// Draw a border on every side except the left.
    LeftOpen,
    /// Draw a border on every side except the right.
    RightOpen,
    /// Draw no border.
    None,
    /// Draw a rounded border.
    Rounded,
    /// Draw a square border.
    Square,
}

/// Whether line numbers are relative to the cursor line.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum RelativeLineNumbers {
    /// Show absolute line numbers.
    #[default]
    Disabled,
    /// Show line numbers relative to the cursor's buffer row.
    Enabled,
    /// Show line numbers relative to the cursor's display row.
    Wrapped,
}

/// When to populate a new search's query based on the text under the cursor.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum SeedQuerySetting {
    /// Never populate the search query from the cursor.
    #[default]
    None,
    /// Populate the search query from a non-empty selection.
    Selection,
    /// Populate the search query from the cursor's line.
    Line,
    /// Populate the search query from the text surrounding the cursor.
    Surround,
}

/// The modifier that adds a cursor when held during a click.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum MultiCursorModifier {
    /// Use alt (option on macOS).
    #[default]
    Alt,
    /// Use cmd on macOS and ctrl elsewhere.
    CmdOrCtrl,
}

/// What a double click in a multibuffer does.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DoubleClickInMultibuffer {
    /// Select the word under the cursor.
    #[default]
    Select,
    /// Open the excerpt's buffer in its own tab.
    Open,
}

/// What to do when go-to-definition finds no definition.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum GoToDefinitionFallback {
    /// Fall back to the code lens.
    #[default]
    Lens,
    /// Fall back to a project search.
    Search,
    /// Do not fall back.
    Never,
    /// Do not fall back.
    None,
    /// Fall back to finding all references.
    FindAllReferences,
}

/// How to scroll the target into view after a go-to-definition.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum GoToDefinitionScrollStrategy {
    /// Center the target in the viewport.
    #[default]
    Center,
    /// Scroll the minimum amount needed to reveal the target.
    Minimum,
    /// Scroll the target to the top of the viewport.
    Top,
    /// Preserve the current scroll position.
    Preserve,
}

/// How to sort snippets against other completion entries.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum SnippetSortOrder {
    /// Sort by fuzzy match relevance.
    #[default]
    Relevance,
    /// Sort alphabetically.
    Alphabetical,
    /// Sort by how often the snippet was used.
    Frequency,
}

/// The maximum severity of diagnostics to render in the editor.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverityContent {
    /// Render no diagnostics.
    Off,
    /// Render errors only.
    Error,
    /// Render errors and warnings.
    Warning,
    /// Render errors, warnings and information.
    Info,
    /// Render every diagnostic, down to hints.
    Hint,
    /// Render every diagnostic.
    #[default]
    All,
}

/// Whether to show code lens.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum CodeLens {
    /// Show code lens inline.
    #[default]
    On,
    /// Do not show code lens.
    Off,
}

/// How to render document colors reported by the language server.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentColorsRenderMode {
    /// Render a color swatch as an inlay hint.
    #[default]
    Inlay,
    /// Render the color as the text's background.
    Background,
    /// Render the color as a border around the text.
    Border,
    /// Render the color as both background and border.
    Full,
    /// Do not render document colors.
    None,
}

/// Which side of a completion entry the detail text is aligned to.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDetailAlignment {
    /// Align the detail text to the left.
    #[default]
    Left,
    /// Align the detail text to the right.
    Right,
}

/// Which kinds of entries the completion menu shows.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum CompletionMenuItemKind {
    /// Show every completion kind.
    #[default]
    All,
    /// Show symbols only.
    Symbols,
    /// Show keywords only.
    Keywords,
    /// Show types only.
    Types,
}

/// How to lay out a diff.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum DiffViewStyle {
    /// Show deletions and insertions in a single column.
    #[default]
    Unified,
    /// Show the old and new text side by side.
    Split,
}
