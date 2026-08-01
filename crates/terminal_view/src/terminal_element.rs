use editor::{CursorLayout, EditorSettings, HighlightedRange, HighlightedRangeLine};
use gpui::{
    AbsoluteLength, AnyElement, App, AvailableSpace, Bounds, ContentMask, Context, DispatchPhase,
    Element, ElementId, Entity, FocusHandle, Font, FontFeatures, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, Hitbox, Hsla, InputHandler, InteractiveElement, Interactivity,
    IntoElement, LayoutId, Length, ModifiersChangedEvent, MouseButton, MouseMoveEvent, Pixels,
    Point as GpuiPoint, Role, Stateful, StatefulInteractiveElement, StrikethroughStyle, Styled,
    TextRun, TextStyle, UTF16Selection, UnderlineStyle, WeakEntity, WhiteSpace, Window, div, fill,
    point, px, relative, size, A11ySubtreeBuilder,
};
use accesskit;

use itertools::Itertools;
use language::CursorShape as EditorCursorShape;
use settings::Settings;
use std::time::Instant;
use terminal::{
    Cell, Color, Content, CursorShape, IndexedCell, Modes, NamedColor, Point, Range, Terminal,
    TerminalBounds, VisibleImage, is_app_chosen_exact_color as terminal_is_app_chosen_exact_color,
    is_default_background_color, terminal_settings::TerminalSettings,
};
use theme::{ActiveTheme, Theme};
use theme_settings::ThemeSettings;
use ui::utils::ensure_minimum_contrast;
use ui::{ParentElement, Tooltip};
use util::ResultExt;
use workspace::Workspace;

use std::mem;
use std::{fmt::Debug, rc::Rc};

use crate::{BlockContext, BlockProperties, ContentMode, TerminalMode, TerminalView};

/// The information generated during layout that is necessary for painting.
pub struct LayoutState {
    hitbox: Hitbox,
    batched_text_runs: Vec<BatchedTextRun>,
    rects: Vec<LayoutRect>,
    relative_highlighted_ranges: Vec<(Range, Hsla)>,
    cursor: Option<CursorLayout>,
    ime_cursor_bounds: Option<Bounds<Pixels>>,
    background_color: Hsla,
    dimensions: TerminalBounds,
    mode: Modes,
    display_offset: usize,
    hyperlink_tooltip: Option<AnyElement>,
    block_below_cursor_element: Option<AnyElement>,
    base_text_style: TextStyle,
    content_mode: ContentMode,
    /// kitty graphics / OSC 1337 图像叠加层, 已按 z-index 排好绘制顺序。
    images: Vec<(VisibleImage, std::sync::Arc<gpui::RenderImage>)>,
}

/// Helper struct for converting terminal cursor points to displayed cursor points.
#[derive(Copy, Clone)]
struct DisplayCursor {
    line: i32,
    col: usize,
}

impl DisplayCursor {
    fn from(cursor_point: Point, display_offset: usize) -> Self {
        Self {
            line: cursor_point.line + display_offset as i32,
            col: cursor_point.column,
        }
    }

    pub fn line(&self) -> i32 {
        self.line
    }

    pub fn col(&self) -> usize {
        self.col
    }
}

/// Whether a standalone terminal's grid should be pinned to the bottom of its
/// available height.
///
/// Full-screen TUI apps (vim, opencode) run on the alternate screen and never
/// scroll, so they must anchor to the bottom regardless of the scroll state;
/// primary-screen content anchors only when it is scrolled to the bottom and
/// the bottom row is actually occupied (otherwise the leftover padding stays
/// below the grid and the prompt remains at the top).
fn should_anchor_to_bottom(content: &Content) -> bool {
    content.mode.contains(Modes::ALT_SCREEN)
        || (content.scrolled_to_bottom && content.bottom_row_occupied)
}

/// Converts a grid (line, col) to a pixel position, quantized to the
/// device-pixel grid.
///
/// Every terminal grid painter — glyph runs, background rects, kitty graphics
/// overlays, and the cursor — must use this same quantization so that a cell's
/// cursor lands on the exact pixel boundary as its glyph. Quantizing in device
/// space (rounding the device-pixel position, then converting back to logical
/// pixels) keeps the grid stable across fractional cell metrics and HiDPI
/// scale factors. A plain `.floor()` in logical pixels (the historical cursor
/// behavior) quantizes to a different boundary than the window's device-pixel
/// snapping and can leave the cursor up to a pixel away from the character it
/// covers.
fn snapped_cell_point(
    origin: GpuiPoint<Pixels>,
    line: i32,
    col: usize,
    dimensions: &TerminalBounds,
    scale_factor: f32,
) -> GpuiPoint<Pixels> {
    let snap = |value: Pixels| {
        Pixels::from((f32::from(value) * scale_factor).round() / scale_factor)
    };
    point(
        snap(origin.x + col as f32 * dimensions.cell_width()),
        snap(origin.y + line as f32 * dimensions.line_height()),
    )
}

fn terminal_paint_origin(
    bounds_origin: GpuiPoint<Pixels>,
    scroll_top: Pixels,
    scale_factor: f32,
) -> GpuiPoint<Pixels> {
    let snap = |value: Pixels| {
        Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
    };
    point(snap(bounds_origin.x), snap(bounds_origin.y - scroll_top))
}


#[derive(Copy, Clone, Debug, Default)]
pub struct LayoutPoint {
    line: i32,
    column: i32,
}

impl LayoutPoint {
    fn new(line: i32, column: i32) -> Self {
        Self { line, column }
    }

    pub fn line(&self) -> i32 {
        self.line
    }

    pub fn column(&self) -> i32 {
        self.column
    }
}

/// A batched text run that combines multiple adjacent cells with the same style
#[derive(Debug)]
pub struct BatchedTextRun {
    pub start_point: LayoutPoint,
    pub text: String,
    pub cell_count: usize,
    pub style: TextRun,
    pub font_size: AbsoluteLength,
}

impl BatchedTextRun {
    fn new_from_char(
        start_point: LayoutPoint,
        c: char,
        style: TextRun,
        font_size: AbsoluteLength,
    ) -> Self {
        let mut text = String::with_capacity(100); // Pre-allocate for typical line length
        text.push(c);
        BatchedTextRun {
            start_point,
            text,
            cell_count: 1,
            style,
            font_size,
        }
    }

    fn can_append(&self, other_style: &TextRun) -> bool {
        self.style.font == other_style.font
            && self.style.color == other_style.color
            && self.style.background_color == other_style.background_color
            && self.style.underline == other_style.underline
            && self.style.strikethrough == other_style.strikethrough
    }

    fn append_char(&mut self, c: char) {
        self.append_char_internal(c, true);
    }

    fn append_zero_width_chars(&mut self, chars: &[char]) {
        for &c in chars {
            self.append_char_internal(c, false);
        }
    }

    fn append_char_internal(&mut self, c: char, counts_cell: bool) {
        self.text.push(c);
        if counts_cell {
            self.cell_count += 1;
        }
        self.style.len += c.len_utf8();
    }

    pub fn paint(
        &self,
        origin: GpuiPoint<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let pos = snapped_cell_point(
            origin,
            self.start_point.line,
            self.start_point.column.max(0) as usize,
            dimensions,
            window.scale_factor(),
        );

        window
            .text_system()
            .shape_line(
                self.text.clone().into(),
                self.font_size.to_pixels(window.rem_size()),
                std::slice::from_ref(&self.style),
                Some(dimensions.cell_width),
            )
            .paint(
                pos,
                dimensions.line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .log_err();
    }
}

#[derive(Clone, Debug, Default)]
pub struct LayoutRect {
    point: LayoutPoint,
    num_of_cells: usize,
    color: Hsla,
}

impl LayoutRect {
    fn new(point: LayoutPoint, num_of_cells: usize, color: Hsla) -> LayoutRect {
        LayoutRect {
            point,
            num_of_cells,
            color,
        }
    }

    pub fn paint(
        &self,
        origin: GpuiPoint<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
    ) {
        let scale_factor = window.scale_factor();
        let col = self.point.column.max(0) as usize;
        let start = snapped_cell_point(origin, self.point.line, col, dimensions, scale_factor);
        let end = snapped_cell_point(
            origin,
            self.point.line,
            col + self.num_of_cells,
            dimensions,
            scale_factor,
        );
        let bottom = snapped_cell_point(
            origin,
            self.point.line + 1,
            col,
            dimensions,
            scale_factor,
        );
        let size = point(end.x - start.x, bottom.y - start.y).into();

        window.paint_quad(fill(Bounds::new(start, size), self.color));
    }
}

/// Represents a rectangular region with a specific background color
#[derive(Debug, Clone)]
struct BackgroundRegion {
    start_line: i32,
    start_col: i32,
    end_line: i32,
    end_col: i32,
    color: Hsla,
}

impl BackgroundRegion {
    fn new(line: i32, col: i32, color: Hsla) -> Self {
        BackgroundRegion {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col,
            color,
        }
    }

    /// Check if this region can be merged with another region
    fn can_merge_with(&self, other: &BackgroundRegion) -> bool {
        if self.color != other.color {
            return false;
        }

        // Check if regions are adjacent horizontally
        if self.start_line == other.start_line && self.end_line == other.end_line {
            return self.end_col + 1 == other.start_col || other.end_col + 1 == self.start_col;
        }

        // Check if regions are adjacent vertically with same column span
        if self.start_col == other.start_col && self.end_col == other.end_col {
            return self.end_line + 1 == other.start_line || other.end_line + 1 == self.start_line;
        }

        false
    }

    /// Merge this region with another region
    fn merge_with(&mut self, other: &BackgroundRegion) {
        self.start_line = self.start_line.min(other.start_line);
        self.start_col = self.start_col.min(other.start_col);
        self.end_line = self.end_line.max(other.end_line);
        self.end_col = self.end_col.max(other.end_col);
    }
}

pub trait TerminalLayoutCell {
    fn point(&self) -> Point;
    fn cell(&self) -> &Cell;
}

impl TerminalLayoutCell for IndexedCell {
    fn point(&self) -> Point {
        self.point
    }

    fn cell(&self) -> &Cell {
        &self.cell
    }
}

impl TerminalLayoutCell for &IndexedCell {
    fn point(&self) -> Point {
        self.point
    }

    fn cell(&self) -> &Cell {
        &self.cell
    }
}

/// Merge background regions to minimize the number of rectangles
fn merge_background_regions(regions: Vec<BackgroundRegion>) -> Vec<BackgroundRegion> {
    if regions.is_empty() {
        return regions;
    }

    let mut merged = regions;
    let mut changed = true;

    // Keep merging until no more merges are possible
    while changed {
        changed = false;
        let mut i = 0;

        while i < merged.len() {
            let mut j = i + 1;
            while j < merged.len() {
                if merged[i].can_merge_with(&merged[j]) {
                    let other = merged.remove(j);
                    merged[i].merge_with(&other);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    merged
}

/// The GPUI element that paints the terminal.
/// We need to keep a reference to the model for mouse events, do we need it for any other terminal stuff, or can we move that to connection?
pub struct TerminalElement {
    terminal: Entity<Terminal>,
    terminal_view: Entity<TerminalView>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    interactivity: Interactivity,
    mode: TerminalMode,
    block_below_cursor: Option<Rc<BlockProperties>>,
}

impl InteractiveElement for TerminalElement {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

impl StatefulInteractiveElement for TerminalElement {}

impl TerminalElement {
    pub fn new(
        terminal: Entity<Terminal>,
        terminal_view: Entity<TerminalView>,
        workspace: WeakEntity<Workspace>,
        focus: FocusHandle,
        focused: bool,
        cursor_visible: bool,
        block_below_cursor: Option<Rc<BlockProperties>>,
        mode: TerminalMode,
    ) -> Stateful<TerminalElement> {
        // A stable id is required for the element to participate in the
        // accessibility tree (Element::a11y_role / a11y_synthetic_children
        // are only consulted for elements with an id). The id is unique within
        // its parent wrapper (mux_pane / terminal_view render one element each).
        TerminalElement {
            terminal,
            terminal_view,
            workspace,
            focused,
            focus: focus.clone(),
            cursor_visible,
            block_below_cursor,
            mode,
            interactivity: Default::default(),
        }
        .id("terminal-element")
        .track_focus(&focus)
    }

    pub fn layout_grid<T: TerminalLayoutCell>(
        grid: impl Iterator<Item = T>,
        start_line_offset: i32,
        text_style: &TextStyle,
        hyperlink: Option<(HighlightStyle, &Range)>,
        minimum_contrast: f32,
        cx: &App,
    ) -> (Vec<LayoutRect>, Vec<BatchedTextRun>) {
        let start_time = Instant::now();
        let theme = cx.theme();

        // Pre-allocate with estimated capacity to reduce reallocations
        let estimated_cells = grid.size_hint().0;
        let estimated_runs = estimated_cells / 10; // Estimate ~10 cells per run
        let estimated_regions = estimated_cells / 20; // Estimate ~20 cells per background region

        let mut batched_runs = Vec::with_capacity(estimated_runs);
        let mut cell_count = 0;

        // Collect background regions for efficient merging
        let mut background_regions: Vec<BackgroundRegion> = Vec::with_capacity(estimated_regions);
        let mut current_batch: Option<BatchedTextRun> = None;

        // First pass: collect all cells and their backgrounds
        let linegroups = grid.into_iter().chunk_by(|cell| cell.point().line);
        for (line_index, (_, line)) in linegroups.into_iter().enumerate() {
            let display_line = start_line_offset + line_index as i32;

            // Flush any existing batch at line boundaries
            if let Some(batch) = current_batch.take() {
                batched_runs.push(batch);
            }

            let mut previous_cell_had_extras = false;

            for cell in line {
                let point = cell.point();
                let cell = cell.cell();
                let mut fg = cell.foreground();
                let mut bg = cell.background();
                if cell.is_inverse() {
                    mem::swap(&mut fg, &mut bg);
                }

                // Collect background regions (skip default background)
                if !is_default_background_color(bg) {
                    let color = convert_color(&bg, theme);
                    let col = point.column as i32;

                    // Try to extend the last region if it's on the same line with the same color
                    if let Some(last_region) = background_regions.last_mut()
                        && last_region.color == color
                        && last_region.start_line == display_line
                        && last_region.end_line == display_line
                        && last_region.end_col + 1 == col
                    {
                        last_region.end_col = col;
                    } else {
                        background_regions.push(BackgroundRegion::new(display_line, col, color));
                    }
                }
                // Skip wide character spacers - they're just placeholders for the second cell of wide characters
                if cell.is_wide_char_spacer() {
                    continue;
                }

                // Skip spaces that follow cells with extras (emoji variation sequences)
                if cell.character() == ' ' && previous_cell_had_extras {
                    previous_cell_had_extras = false;
                    continue;
                }
                // Update tracking for next iteration
                previous_cell_had_extras =
                    matches!(cell.zerowidth(), Some(chars) if !chars.is_empty());

                //Layout current cell text
                {
                    if !is_blank(cell) {
                        cell_count += 1;
                        let cell_style = TerminalElement::cell_style(
                            point,
                            cell,
                            fg,
                            bg,
                            theme,
                            text_style,
                            hyperlink,
                            minimum_contrast,
                        );

                        let cell_point = LayoutPoint::new(display_line, point.column as i32);
                        let zero_width_chars = cell.zerowidth();

                        // Try to batch with existing run
                        if let Some(ref mut batch) = current_batch {
                            if batch.can_append(&cell_style)
                                && batch.start_point.line == cell_point.line
                                && batch.start_point.column + batch.cell_count as i32
                                    == cell_point.column
                            {
                                batch.append_char(cell.character());
                                if let Some(chars) = zero_width_chars {
                                    batch.append_zero_width_chars(chars);
                                }
                            } else {
                                // Flush current batch and start new one
                                let old_batch = current_batch.take().unwrap();
                                batched_runs.push(old_batch);
                                let mut new_batch = BatchedTextRun::new_from_char(
                                    cell_point,
                                    cell.character(),
                                    cell_style,
                                    text_style.font_size,
                                );
                                if let Some(chars) = zero_width_chars {
                                    new_batch.append_zero_width_chars(chars);
                                }
                                current_batch = Some(new_batch);
                            }
                        } else {
                            // Start new batch
                            let mut new_batch = BatchedTextRun::new_from_char(
                                cell_point,
                                cell.character(),
                                cell_style,
                                text_style.font_size,
                            );
                            if let Some(chars) = zero_width_chars {
                                new_batch.append_zero_width_chars(chars);
                            }
                            current_batch = Some(new_batch);
                        }
                    };
                }
            }
        }

        // Flush any remaining batch
        if let Some(batch) = current_batch {
            batched_runs.push(batch);
        }

        // Second pass: merge background regions and convert to layout rects
        let region_count = background_regions.len();
        let merged_regions = merge_background_regions(background_regions);
        let mut rects = Vec::with_capacity(merged_regions.len() * 2); // Estimate 2 rects per merged region

        // Convert merged regions to layout rects
        // Since LayoutRect only supports single-line rectangles, we need to split multi-line regions
        for region in merged_regions {
            for line in region.start_line..=region.end_line {
                rects.push(LayoutRect::new(
                    LayoutPoint::new(line, region.start_col),
                    (region.end_col - region.start_col + 1) as usize,
                    region.color,
                ));
            }
        }

        let layout_time = start_time.elapsed();

        log::debug!(
            "Terminal layout_grid: {} cells processed, \
            {} batched runs created, {} rects (from {} merged regions), \
            layout took {:?}",
            cell_count,
            batched_runs.len(),
            rects.len(),
            region_count,
            layout_time
        );

        (rects, batched_runs)
    }

    /// Computes the cursor position relative to the paint origin.
    ///
    /// The position is quantized to device pixels via [`snapped_cell_point`]
    /// so the cursor shares the exact pixel boundary with the glyph in the
    /// same cell, including when the terminal is partially scrolled.
    fn cursor_position(
        cursor_point: DisplayCursor,
        size: TerminalBounds,
        paint_origin: GpuiPoint<Pixels>,
        scale_factor: f32,
    ) -> Option<GpuiPoint<Pixels>> {
        if cursor_point.line() < size.num_lines() as i32 {
            let snapped = snapped_cell_point(
                paint_origin,
                cursor_point.line(),
                cursor_point.col(),
                &size,
                scale_factor,
            );
            Some(point(
                snapped.x - paint_origin.x,
                snapped.y - paint_origin.y,
            ))
        } else {
            None
        }
    }

    /// Checks if a character is a decorative block/box-like character that should
    /// preserve its exact colors without contrast adjustment.
    ///
    /// This specifically targets characters used as visual connectors, separators,
    /// and borders where color matching with adjacent backgrounds is critical.
    /// Regular icons (git, folders, etc.) are excluded as they need to remain readable.
    ///
    /// Fixes https://github.com/zed-industries/zed/issues/34234
    fn is_decorative_character(ch: char) -> bool {
        matches!(
            ch as u32,
            // Unicode Box Drawing and Block Elements
            0x2500..=0x257F // Box Drawing (└ ┐ ─ │ etc.)
            | 0x2580..=0x259F // Block Elements (▀ ▄ █ ░ ▒ ▓ etc.)
            | 0x25A0..=0x25FF // Geometric Shapes (■ ▶ ● etc. - includes triangular/circular separators)

            // Private Use Area - Powerline separator symbols only
            | 0xE0B0..=0xE0B7 // Powerline separators: triangles (E0B0-E0B3) and half circles (E0B4-E0B7)
            | 0xE0B8..=0xE0BF // Powerline separators: corner triangles
            | 0xE0C0..=0xE0CA // Powerline separators: flames (E0C0-E0C3), pixelated (E0C4-E0C7), and ice (E0C8 & E0CA)
            | 0xE0CC..=0xE0D1 // Powerline separators: honeycombs (E0CC-E0CD) and lego (E0CE-E0D1)
            | 0xE0D2..=0xE0D7 // Powerline separators: trapezoid (E0D2 & E0D4) and inverted triangles (E0D6-E0D7)
        )
    }

    /// Whether the application explicitly picked this foreground color and does not
    /// want it adjusted for contrast: 24-bit true color (`\e[38;2;R;G;Bm`) or a
    /// specific entry in the 256-color palette (`\e[38;5;Nm`) where N >= 16 (the
    /// 6x6x6 cube at 16..=231 and the 24-step grayscale ramp at 232..=255).
    /// Indices 0..=15 still go through contrast adjustment since those map to
    /// theme-defined ANSI colors that can clash with the theme background.
    fn is_app_chosen_exact_color(fg: &Color) -> bool {
        terminal_is_app_chosen_exact_color(*fg)
    }

    /// Converts terminal cell styles to GPUI text styles and background color.
    fn cell_style(
        point: Point,
        cell: &Cell,
        fg: Color,
        bg: Color,
        colors: &Theme,
        text_style: &TextStyle,
        hyperlink: Option<(HighlightStyle, &Range)>,
        minimum_contrast: f32,
    ) -> TextRun {
        let skip_contrast = Self::is_app_chosen_exact_color(&fg);
        let mut fg = convert_color(&fg, colors);
        let bg = convert_color(&bg, colors);

        if !skip_contrast && !Self::is_decorative_character(cell.character()) {
            fg = ensure_minimum_contrast(fg, bg, minimum_contrast);
        }

        // Use a dim multiplier that stays close to the existing Alacritty look.
        if cell.is_dim() {
            fg.a *= 0.7;
        }

        let underline =
            (cell.has_underline() || cell.hyperlink().is_some()).then(|| UnderlineStyle {
                color: Some(fg),
                thickness: Pixels::from(1.0),
                wavy: cell.has_undercurl(),
            });

        let strikethrough = cell.has_strikeout().then(|| StrikethroughStyle {
            color: Some(fg),
            thickness: Pixels::from(1.0),
        });

        let weight = if cell.is_bold() {
            FontWeight::BOLD
        } else {
            text_style.font_weight
        };

        let style = if cell.is_italic() {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };

        let mut result = TextRun {
            len: cell.character().len_utf8(),
            color: fg,
            background_color: None,
            font: Font {
                weight,
                style,
                ..text_style.font()
            },
            underline,
            strikethrough,
        };

        if let Some((style, range)) = hyperlink
            && range.contains(point)
        {
            if let Some(underline) = style.underline {
                result.underline = Some(underline);
            }

            if let Some(color) = style.color {
                result.color = color;
            }
        }

        result
    }

    fn generic_button_handler<E>(
        connection: Entity<Terminal>,
        focus_handle: FocusHandle,
        steal_focus: bool,
        f: impl Fn(&mut Terminal, &E, &mut Context<Terminal>),
    ) -> impl Fn(&E, &mut Window, &mut App) {
        move |event, window, cx| {
            if steal_focus {
                window.focus(&focus_handle, cx);
            } else if !focus_handle.is_focused(window) {
                return;
            }
            connection.update(cx, |terminal, cx| {
                f(terminal, event, cx);

                cx.notify();
            })
        }
    }

    fn register_mouse_listeners(
        &mut self,
        mode: Modes,
        hitbox: &Hitbox,
        content_mode: &ContentMode,
        window: &mut Window,
    ) {
        let focus = self.focus.clone();
        let terminal = self.terminal.clone();
        let terminal_view = self.terminal_view.clone();

        self.interactivity.on_mouse_down(MouseButton::Left, {
            let terminal = terminal.clone();
            let focus = focus.clone();
            let terminal_view = terminal_view.clone();

            move |e, window, cx| {
                window.focus(&focus, cx);

                let scroll_top = terminal_view.read(cx).scroll_top;
                terminal.update(cx, |terminal, cx| {
                    let mut adjusted_event = e.clone();
                    if scroll_top > Pixels::ZERO {
                        adjusted_event.position.y += scroll_top;
                    }
                    terminal.mouse_down(&adjusted_event, cx);
                    cx.notify();
                })
            }
        });

        window.on_mouse_event({
            let terminal = self.terminal.clone();
            let hitbox = hitbox.clone();
            let focus = focus.clone();
            let terminal_view = terminal_view;
            move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                if e.pressed_button.is_some() && !cx.has_active_drag() && focus.is_focused(window) {
                    let hovered = hitbox.is_hovered(window);

                    let scroll_top = terminal_view.read(cx).scroll_top;
                    terminal.update(cx, |terminal, cx| {
                        if terminal.selection_started() || hovered {
                            let mut adjusted_event = e.clone();
                            if scroll_top > Pixels::ZERO {
                                adjusted_event.position.y += scroll_top;
                            }
                            terminal.mouse_drag(&adjusted_event, hitbox.bounds, cx);
                            cx.notify();
                        }
                    })
                }

                if hitbox.is_hovered(window) {
                    terminal.update(cx, |terminal, cx| {
                        terminal.mouse_move(e, cx);
                    })
                }
            }
        });

        self.interactivity.on_mouse_up(
            MouseButton::Left,
            TerminalElement::generic_button_handler(
                terminal.clone(),
                focus.clone(),
                false,
                move |terminal, e, cx| {
                    terminal.mouse_up(e, cx);
                },
            ),
        );
        self.interactivity.on_mouse_down(
            MouseButton::Middle,
            TerminalElement::generic_button_handler(
                terminal.clone(),
                focus.clone(),
                true,
                move |terminal, e, cx| {
                    terminal.mouse_down(e, cx);
                },
            ),
        );

        if content_mode.is_scrollable() {
            self.interactivity.on_scroll_wheel({
                let terminal_view = self.terminal_view.downgrade();
                move |e, window, cx| {
                    terminal_view
                        .update(cx, |terminal_view, cx| {
                            if matches!(terminal_view.mode, TerminalMode::Standalone)
                                || terminal_view.focus_handle.is_focused(window)
                            {
                                terminal_view.scroll_wheel(e, cx);
                                cx.notify();
                            }
                        })
                        .ok();
                }
            });
        }

        // Mouse mode handlers:
        // All mouse modes need the extra click handlers
        if mode.intersects(Modes::MOUSE_MODE) {
            self.interactivity.on_mouse_down(
                MouseButton::Right,
                TerminalElement::generic_button_handler(
                    terminal.clone(),
                    focus.clone(),
                    true,
                    move |terminal, e, cx| {
                        terminal.mouse_down(e, cx);
                    },
                ),
            );
            self.interactivity.on_mouse_up(
                MouseButton::Right,
                TerminalElement::generic_button_handler(
                    terminal.clone(),
                    focus.clone(),
                    false,
                    move |terminal, e, cx| {
                        terminal.mouse_up(e, cx);
                    },
                ),
            );
            self.interactivity.on_mouse_up(
                MouseButton::Middle,
                TerminalElement::generic_button_handler(
                    terminal,
                    focus,
                    false,
                    move |terminal, e, cx| {
                        terminal.mouse_up(e, cx);
                    },
                ),
            );
        }
    }

    fn rem_size(&self, cx: &mut App) -> Option<Pixels> {
        let settings = ThemeSettings::get_global(cx).clone();
        let buffer_font_size = settings.buffer_font_size(cx);
        let rem_size_scale = {
            // Our default UI font size is 14px on a 16px base scale.
            // This means the default UI font size is 0.875rems.
            let default_font_size_scale = 14. / ui::BASE_REM_SIZE_IN_PX;

            // We then determine the delta between a single rem and the default font
            // size scale.
            let default_font_size_delta = 1. - default_font_size_scale;

            // Finally, we add this delta to 1rem to get the scale factor that
            // should be used to scale up the UI.
            1. + default_font_size_delta
        };

        Some(buffer_font_size * rem_size_scale)
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Expose TerminalElement to the accessibility tree as the live terminal
    /// surface. The labelled Terminal node that announces the pane lives on
    /// the wrapper (mux_pane / terminal_view root); this element is its single
    /// content child and supplies the readable line text via
    /// [`Element::a11y_synthetic_children`].
    fn a11y_role(&self) -> Option<accesskit::Role> {
        Some(Role::Terminal)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        // Stable, translatable-ish label so the surface is not nameless. The
        // pane title is announced by the parent; this only names the content.
        node.set_label("terminal output".to_string());
    }

    /// Synthesize one [`Role::TextRun`] node per visible terminal line, derived
    /// from the same `batched_text_runs` the element paints. Runs already flush
    /// at line boundaries (see [`TerminalElement::layout_grid`]), so grouping by
    /// `start_point.line` reconstructs the on-screen rows. Each line is chunked
    /// to fit AccessKit's `u8`-indexed `word_starts` (max 255 chars/run) and
    /// supplies per-run `character_lengths` / `word_starts` so platform text
    /// patterns (caret tracking, review, typed-character echo) work.
    ///
    /// Synthetic ids key off the parent node id + a `(line, chunk)` key, so
    /// stable frame-to-frame while the pane layout is stable.
    fn a11y_synthetic_children(
        &mut self,
        prepaint: &mut Self::PrepaintState,
        builder: &mut A11ySubtreeBuilder,
    ) {
        push_terminal_line_text_runs(builder, &prepaint.batched_text_runs);
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let height: Length = match self.terminal_view.read(cx).content_mode(window, cx) {
            ContentMode::Inline {
                displayed_lines,
                total_lines: _,
            } => {
                let rem_size = window.rem_size();
                let line_height = f32::from(window.text_style().font_size.to_pixels(rem_size))
                    * TerminalSettings::get_global(cx).line_height.value();
                px(displayed_lines as f32 * line_height).into()
            }
            ContentMode::Scrollable => {
                if let TerminalMode::Embedded { .. } = &self.mode {
                    let term = self.terminal.read(cx);
                    if !term.scrolled_to_top() && !term.scrolled_to_bottom() && self.focused {
                        self.interactivity.occlude_mouse();
                    }
                }

                relative(1.).into()
            }
        };

        let layout_id = self.interactivity.request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |mut style, window, cx| {
                style.size.width = relative(1.).into();
                style.size.height = height;

                window.request_layout(style, None, cx)
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let rem_size = self.rem_size(cx);
        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            bounds.size,
            window,
            cx,
            |_, _, hitbox, window, cx| {
                let hitbox = hitbox.unwrap();
                let settings = ThemeSettings::get_global(cx).clone();

                let buffer_font_size = settings.buffer_font_size(cx);

                let terminal_settings = TerminalSettings::get_global(cx);
                let minimum_contrast = terminal_settings.minimum_contrast;

                let font_family = terminal_settings.font_family.as_ref().map_or_else(
                    || settings.buffer_font.family.clone(),
                    |font_family| font_family.0.clone().into(),
                );

                let font_fallbacks = terminal_settings
                    .font_fallbacks
                    .as_ref()
                    .or(settings.buffer_font.fallbacks.as_ref())
                    .cloned();

                let font_features = terminal_settings
                    .font_features
                    .as_ref()
                    .unwrap_or(&FontFeatures::disable_ligatures())
                    .clone();

                let font_weight = terminal_settings.font_weight.unwrap_or_default();

                let line_height = terminal_settings.line_height.value();

                let font_size = match &self.mode {
                    TerminalMode::Embedded { .. } => {
                        window.text_style().font_size.to_pixels(window.rem_size())
                    }
                    TerminalMode::Standalone => terminal_settings
                        .font_size
                        .map_or(buffer_font_size, |size| {
                            theme_settings::adjusted_font_size(size, cx)
                        }),
                };

                let theme = cx.theme().clone();

                let link_style = HighlightStyle {
                    color: Some(theme.colors().link_text_hover),
                    font_weight: Some(font_weight),
                    font_style: None,
                    background_color: None,
                    underline: Some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(theme.colors().link_text_hover),
                        wavy: false,
                    }),
                    strikethrough: None,
                    fade_out: None,
                };

                let text_style = TextStyle {
                    font_family,
                    font_features,
                    font_weight,
                    font_fallbacks,
                    font_size: font_size.into(),
                    font_style: FontStyle::Normal,
                    line_height: px(line_height).into(),
                    background_color: Some(theme.colors().terminal_ansi_background),
                    white_space: WhiteSpace::Normal,
                    // These are going to be overridden per-cell
                    color: theme.colors().terminal_foreground,
                    ..Default::default()
                };

                let text_system = cx.text_system();
                let player_color = theme.players().local();
                let match_color = theme.colors().search_match_background;
                let gutter;
                let (dimensions, line_height_px) = {
                    let rem_size = window.rem_size();
                    let font_pixels = text_style.font_size.to_pixels(rem_size);
                    let line_height = f32::from(font_pixels) * line_height;
                    let font_id = cx.text_system().resolve_font(&text_style.font());

                    let cell_width = text_system
                        .advance(font_id, font_pixels, 'm')
                        .unwrap()
                        .width;
                    gutter = cell_width;

                    let mut size = bounds.size;
                    size.width -= gutter;
                    let available_height = size.height;

                    // https://github.com/zed-industries/zed/issues/2750
                    // if the terminal is one column wide, rendering 🦀
                    // causes alacritty to misbehave.
                    if size.width < cell_width * 2.0 {
                        size.width = cell_width * 2.0;
                    }

                    let mut origin = bounds.origin;
                    origin.x += gutter;

                    if matches!(self.terminal_view.read(cx).mode, TerminalMode::Standalone) {
                        let should_anchor_to_bottom = {
                            let content = self.terminal.read(cx).last_content();
                            should_anchor_to_bottom(content)
                        };
                        let scale_factor = window.scale_factor();
                        let line_height_pixels = px(line_height);
                        let line_height_device_px = (f32::from(line_height_pixels) * scale_factor)
                            .round()
                            .max(1.0) as i32;
                        let available_height_device_px =
                            (f32::from(available_height) * scale_factor)
                                .floor()
                                .max(0.0) as i32;

                        let rows =
                            ((available_height_device_px / line_height_device_px) as usize).max(1);
                        let snapped_height_device_px = (rows as i32) * line_height_device_px;
                        let padding_device_px =
                            (available_height_device_px - snapped_height_device_px).max(0);

                        let snapped_height =
                            px(snapped_height_device_px as f32 / scale_factor.max(1.0));
                        let padding = px(padding_device_px as f32 / scale_factor.max(1.0));

                        size.height = snapped_height;
                        if should_anchor_to_bottom {
                            origin.y += padding;
                        }
                    }

                    // Snap to device pixels to avoid subpixel jitter while resizing.
                    // Terminal rendering is grid-based; allowing fractional origins can cause the
                    // glyph rasterization to shift between frames, which looks like flicker.
                    let scale_factor = window.scale_factor();
                    let snap_px = |value: Pixels| {
                        Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
                    };
                    origin.x = snap_px(origin.x);
                    origin.y = snap_px(origin.y);

                    (
                        TerminalBounds::new(px(line_height), cell_width, Bounds { origin, size }),
                        line_height,
                    )
                };

                let search_matches = self.terminal.read(cx).matches.clone();

                let background_color = theme.colors().terminal_background;

                let (last_hovered_word, hover_tooltip) =
                    self.terminal.update(cx, |terminal, cx| {
                        terminal.set_size(dimensions);
                        terminal.sync(window, cx);

                        if window.modifiers().secondary()
                            && bounds.contains(&window.mouse_position())
                            && self.terminal_view.read(cx).hover.is_some()
                        {
                            let registered_hover = self.terminal_view.read(cx).hover.as_ref();
                            if terminal.last_content.last_hovered_word.as_ref()
                                == registered_hover.map(|hover| &hover.hovered_word)
                            {
                                (
                                    terminal.last_content.last_hovered_word.clone(),
                                    registered_hover.map(|hover| hover.tooltip.clone()),
                                )
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        }
                    });

                let scroll_top = self.terminal_view.read(cx).scroll_top;
                let paint_origin = terminal_paint_origin(
                    dimensions.bounds.origin,
                    scroll_top,
                    window.scale_factor(),
                );

                let hyperlink_tooltip = hover_tooltip.map(|hover_tooltip| {
                    let offset = dimensions.bounds.origin - point(px(0.), scroll_top);
                    let mut element = div()
                        .size_full()
                        .id("terminal-element")
                        .tooltip(Tooltip::text(hover_tooltip))
                        .into_any_element();
                    element.prepaint_as_root(offset, bounds.size.into(), window, cx);
                    element
                });

                let Content {
                    cells,
                    mode,
                    display_offset,
                    cursor_char,
                    selection,
                    cursor,
                    ..
                } = &self.terminal.read(cx).last_content;
                let mode = *mode;
                let display_offset = *display_offset;

                // searches, highlights to a single range representations
                let mut relative_highlighted_ranges = Vec::new();
                for search_match in search_matches {
                    relative_highlighted_ranges.push((search_match, match_color))
                }
                if let Some(selection) = selection {
                    relative_highlighted_ranges
                        .push((selection.point_range(), player_color.selection));
                }

                // then have that representation be converted to the appropriate highlight data structure

                let content_mode = self.terminal_view.read(cx).content_mode(window, cx);

                // Calculate the intersection of the terminal's bounds with the current
                // content mask (the visible viewport after all parent clipping).
                // This allows us to only render cells that are actually visible, which is
                // critical for performance when terminals are inside scrollable containers
                // like the Agent Panel thread view.
                //
                // This optimization is analogous to the editor optimization in PR #45077
                // which fixed performance issues with large AutoHeight editors inside Lists.
                let content_bounds = dimensions.bounds;
                let visible_bounds = window.content_mask().bounds;
                let intersection = visible_bounds.intersect(&content_bounds);

                // If the terminal is entirely outside the viewport, skip all cell processing.
                // This handles the case where the terminal has been scrolled past (above or
                // below the viewport), similar to the editor fix in PR #45077 where start_row
                // could exceed max_row when the editor was positioned above the viewport.
                let (rects, batched_text_runs) = if intersection.size.height <= px(0.)
                    || intersection.size.width <= px(0.)
                {
                    (Vec::new(), Vec::new())
                } else if intersection == content_bounds {
                    // Fast path: terminal fully visible, no clipping needed.
                    // Avoid grouping/allocation overhead by streaming cells directly.
                    TerminalElement::layout_grid(
                        cells.iter(),
                        0,
                        &text_style,
                        last_hovered_word
                            .as_ref()
                            .map(|last_hovered_word| (link_style, &last_hovered_word.word_match)),
                        minimum_contrast,
                        cx,
                    )
                } else {
                    // Calculate which screen rows are visible based on pixel positions.
                    // This works for both Scrollable and Inline modes because we filter
                    // by screen position (enumerated line group index), not by the cell's
                    // internal line number (which can be negative in Scrollable mode for
                    // scrollback history).
                    let rows_above_viewport = f32::from(
                        (intersection.top() - content_bounds.top()).max(px(0.)) / line_height_px,
                    ) as usize;
                    let visible_row_count =
                        f32::from((intersection.size.height / line_height_px).ceil()) as usize + 1;

                    TerminalElement::layout_grid(
                        // Group cells by line and filter to only the visible screen rows.
                        // skip() and take() work on enumerated line groups (screen position),
                        // making this work regardless of the actual cell.point.line values.
                        cells
                            .iter()
                            .chunk_by(|c| c.point.line)
                            .into_iter()
                            .skip(rows_above_viewport)
                            .take(visible_row_count)
                            .flat_map(|(_, line_cells)| line_cells),
                        rows_above_viewport as i32,
                        &text_style,
                        last_hovered_word
                            .as_ref()
                            .map(|last_hovered_word| (link_style, &last_hovered_word.word_match)),
                        minimum_contrast,
                        cx,
                    )
                };

                // Layout cursor. Rectangle is used for IME, so we should lay it out even
                // if we don't end up showing it.
                let cursor_point = DisplayCursor::from(cursor.point, display_offset);
                let cursor_text = {
                    let str_trxt = cursor_char.to_string();
                    let len = str_trxt.len();
                    window.text_system().shape_line(
                        str_trxt.into(),
                        text_style.font_size.to_pixels(window.rem_size()),
                        &[TextRun {
                            len,
                            font: text_style.font(),
                            color: theme.colors().terminal_ansi_background,
                            ..Default::default()
                        }],
                        None,
                    )
                };

                // For whitespace, use cell width to avoid cursor stretching.
                // For other characters, use the larger of shaped width and cell width
                // to properly cover wide characters like emojis.
                let cursor_width = if cursor_char.is_whitespace() {
                    dimensions.cell_width()
                } else {
                    cursor_text.width.max(dimensions.cell_width())
                };
                let ime_cursor_bounds = TerminalElement::cursor_position(
                    cursor_point,
                    dimensions,
                    paint_origin,
                    window.scale_factor(),
                )
                .map(|cursor_position| Bounds {
                    origin: cursor_position,
                    size: size(cursor_width.ceil(), dimensions.line_height),
                });

                let cursor = if let CursorShape::Hidden = cursor.shape {
                    None
                } else {
                    let focused = self.focused;
                    ime_cursor_bounds.map(move |bounds| {
                        let (shape, text) = match cursor.shape {
                            CursorShape::Block if !focused => (EditorCursorShape::Hollow, None),
                            CursorShape::Block => (EditorCursorShape::Block, Some(cursor_text)),
                            CursorShape::Underline if !focused => (EditorCursorShape::Hollow, None),
                            CursorShape::Underline => (EditorCursorShape::Underline, None),
                            CursorShape::Bar if !focused => (EditorCursorShape::Hollow, None),
                            CursorShape::Bar => (EditorCursorShape::Bar, None),
                            CursorShape::HollowBlock => (EditorCursorShape::Hollow, None),
                            CursorShape::Hidden => unreachable!(),
                        };

                        CursorLayout::new(
                            bounds.origin,
                            bounds.size.width,
                            bounds.size.height,
                            theme.players().local().cursor,
                            shape,
                            text,
                        )
                    })
                };

                let block_below_cursor_element = if let Some(block) = &self.block_below_cursor {
                    let terminal = self.terminal.read(cx);
                    if terminal.last_content.display_offset == 0 {
                        let target_line = terminal.last_content.cursor.point.line + 1;
                        let render = &block.render;
                        let mut block_cx = BlockContext {
                            window,
                            context: cx,
                            dimensions,
                        };
                        let element = render(&mut block_cx);
                        let mut element = div().occlude().child(element).into_any_element();
                        let available_space = size(
                            AvailableSpace::Definite(dimensions.width() + gutter),
                            AvailableSpace::Definite(
                                block.height as f32 * dimensions.line_height(),
                            ),
                        );
                        let origin = GpuiPoint::new(bounds.origin.x, dimensions.bounds.origin.y)
                            + point(px(0.), target_line as f32 * dimensions.line_height())
                            - point(px(0.), scroll_top);
                        window.with_rem_size(rem_size, |window| {
                            element.prepaint_as_root(origin, available_space, window, cx);
                        });
                        Some(element)
                    } else {
                        None
                    }
                } else {
                    None
                };

                LayoutState {
                    hitbox,
                    batched_text_runs,
                    cursor,
                    ime_cursor_bounds,
                    background_color,
                    dimensions,
                    rects,
                    relative_highlighted_ranges,
                    mode,
                    display_offset,
                    hyperlink_tooltip,
                    block_below_cursor_element,
                    base_text_style: text_style,
                    content_mode,
                    images: resolve_terminal_images(&self.terminal, cx),
                }
            },
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let paint_start = Instant::now();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            let scroll_top = self.terminal_view.read(cx).scroll_top;
            let scale_factor = window.scale_factor();
            let origin = terminal_paint_origin(
                layout.dimensions.bounds.origin,
                scroll_top,
                scale_factor,
            );

            window.paint_quad(fill(bounds, layout.background_color));

            let marked_text_cloned: Option<String> = {
                let ime_state = &self.terminal_view.read(cx).ime_state;
                ime_state.as_ref().map(|state| state.marked_text.clone())
            };

            let terminal_input_handler = TerminalInputHandler {
                terminal_view: self.terminal_view.clone(),
                cursor_bounds: layout.ime_cursor_bounds.map(|bounds| bounds + origin),
                workspace: self.workspace.clone(),
            };

            self.register_mouse_listeners(
                layout.mode,
                &layout.hitbox,
                &layout.content_mode,
                window,
            );
            if window.modifiers().secondary()
                && bounds.contains(&window.mouse_position())
                && self.terminal_view.read(cx).hover.is_some()
            {
                window.set_cursor_style(gpui::CursorStyle::PointingHand, &layout.hitbox);
            } else {
                window.set_cursor_style(gpui::CursorStyle::IBeam, &layout.hitbox);
            }

            let original_cursor = layout.cursor.take();
            let hyperlink_tooltip = layout.hyperlink_tooltip.take();
            let block_below_cursor_element = layout.block_below_cursor_element.take();
            self.interactivity.paint(
                global_id,
                inspector_id,
                bounds,
                Some(&layout.hitbox),
                window,
                cx,
                |_, window, cx| {
                    window.handle_input(&self.focus, terminal_input_handler, cx);

                    window.on_key_event({
                        let this = self.terminal.clone();
                        move |event: &ModifiersChangedEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }

                            this.update(cx, |term, cx| {
                                term.try_modifiers_change(&event.modifiers, window, cx)
                            });
                        }
                    });

                    for rect in &layout.rects {
                        rect.paint(origin, &layout.dimensions, window);
                    }

                    for (relative_highlighted_range, color) in &layout.relative_highlighted_ranges {
                        if let Some((start_y, highlighted_range_lines)) =
                            to_highlighted_range_lines(relative_highlighted_range, layout, origin)
                        {
                            let corner_radius = if EditorSettings::get_global(cx).rounded_selection
                            {
                                0.15 * layout.dimensions.line_height
                            } else {
                                Pixels::ZERO
                            };
                            let hr = HighlightedRange {
                                start_y,
                                line_height: layout.dimensions.line_height,
                                lines: highlighted_range_lines,
                                color: *color,
                                corner_radius: corner_radius,
                            };
                            hr.paint(true, bounds, window);
                        }
                    }

                    // Paint batched text runs instead of individual cells
                    let text_paint_start = Instant::now();
                    for batch in &layout.batched_text_runs {
                        batch.paint(origin, &layout.dimensions, window, cx);
                    }
                    let text_paint_time = text_paint_start.elapsed();

                    // §11.2 渲染 kitty graphics / OSC 1337 图像
                    for (visible, render_image) in &layout.images {
                        let cell_width = layout.dimensions.cell_width;
                        let line_height = layout.dimensions.line_height;
                        let image_bounds = Bounds {
                            origin: snapped_cell_point(
                                origin,
                                visible.row,
                                visible.column,
                                &layout.dimensions,
                                scale_factor,
                            ),
                            size: size(
                                visible.columns as f32 * cell_width,
                                visible.rows as f32 * line_height,
                            ),
                        };
                        window
                            .paint_image(
                                image_bounds,
                                gpui::Corners::all(px(0.)),
                                render_image.clone(),
                                0,
                                false,
                            )
                            .log_err();
                    }

                    if let Some(text_to_mark) = &marked_text_cloned
                        && !text_to_mark.is_empty()
                        && let Some(ime_bounds) = layout.ime_cursor_bounds
                    {
                        let ime_position = (ime_bounds + origin).origin;
                        let mut ime_style = layout.base_text_style.clone();
                        ime_style.underline = Some(UnderlineStyle {
                            color: Some(ime_style.color),
                            thickness: px(1.0),
                            wavy: false,
                        });

                        let shaped_line = window.text_system().shape_line(
                            text_to_mark.clone().into(),
                            ime_style.font_size.to_pixels(window.rem_size()),
                            &[TextRun {
                                len: text_to_mark.len(),
                                font: ime_style.font(),
                                color: ime_style.color,
                                underline: ime_style.underline,
                                ..Default::default()
                            }],
                            None,
                        );

                        // Paint background to cover terminal text behind marked text
                        let ime_background_bounds = Bounds::new(
                            ime_position,
                            size(shaped_line.width, layout.dimensions.line_height),
                        );
                        window.paint_quad(fill(ime_background_bounds, layout.background_color));

                        shaped_line
                            .paint(
                                ime_position,
                                layout.dimensions.line_height,
                                gpui::TextAlign::Left,
                                None,
                                window,
                                cx,
                            )
                            .log_err();
                    }

                    if self.cursor_visible
                        && marked_text_cloned.is_none()
                        && let Some(mut cursor) = original_cursor
                    {
                        cursor.paint(origin, window, cx);
                    }

                    if let Some(mut element) = block_below_cursor_element {
                        element.paint(window, cx);
                    }

                    if let Some(mut element) = hyperlink_tooltip {
                        element.paint(window, cx);
                    }

                    log::debug!(
                        "Terminal paint: {} text runs, {} rects, \
                        text paint took {:?}, total paint took {total_paint_time:?}",
                        layout.batched_text_runs.len(),
                        layout.rects.len(),
                        text_paint_time,
                        total_paint_time = paint_start.elapsed()
                    );
                },
            );
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct TerminalInputHandler {
    terminal_view: Entity<TerminalView>,
    workspace: WeakEntity<Workspace>,
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        // Always return a valid selection for IME positioning,
        // even in ALT_SCREEN mode (fullscreen TUI apps like opencode, vim, etc.)
        // The terminal still has a cursor position that should be used for IME candidate window placement.
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        self.terminal_view.read(cx).marked_text_range()
    }

    fn text_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.terminal_view.update(cx, |view, view_cx| {
            view.clear_marked_text(view_cx);
            view.commit_text(text, view_cx);
        });

        self.workspace
            .update(cx, |this, cx| {
                window.invalidate_character_coordinates();
                let project = this.project().read(cx);
                let telemetry = project.client().telemetry().clone();
                telemetry.log_edit_event("terminal", project.is_via_remote_server());
            })
            .ok();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_marked_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.terminal_view.update(cx, |view, view_cx| {
            view.set_marked_text(new_text.to_string(), view_cx);
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.terminal_view.update(cx, |view, view_cx| {
            view.clear_marked_text(view_cx);
        });
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let term_bounds = self.terminal_view.read(cx).terminal_bounds(cx);

        let mut bounds = self.cursor_bounds?;
        let offset_x = term_bounds.cell_width * range_utf16.start as f32;
        bounds.origin.x += offset_x;

        Some(bounds)
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }

    fn character_index_for_point(
        &mut self,
        _point: GpuiPoint<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }
}

pub fn is_blank(cell: &Cell) -> bool {
    if cell.character() != ' ' {
        return false;
    }

    if !is_default_background_color(cell.background()) {
        return false;
    }

    if cell.hyperlink().is_some() {
        return false;
    }

    if cell.has_visible_style_modifier() {
        return false;
    }

    true
}

// ---- Accessibility: synthetic line text runs --------------------------------

/// AccessKit's `word_starts` is `u8`-indexed, so a single text run cannot
/// exceed this many characters. Longer lines are split into multiple runs
/// (mirroring `settings_ui::input_field`).
const MAX_CHARS_PER_A11Y_RUN: usize = 255;

/// A line reconstructed from `batched_text_runs` for accessibility purposes.
/// `text` is the concatenation of every run on `line`, trailing blanks
/// right-trimmed so screen readers do not announce a wall of spaces.
struct A11yTerminalLine {
    line: i32,
    text: String,
}

/// Group the element's painted [`BatchedTextRun`]s into per-line strings,
/// ordered by screen row. `batched_text_runs` flush at line boundaries, so
/// every run belongs to exactly one line.
fn collect_terminal_lines(runs: &[BatchedTextRun]) -> Vec<A11yTerminalLine> {
    use std::collections::BTreeMap;
    let mut by_line: BTreeMap<i32, String> = BTreeMap::new();
    for run in runs {
        by_line
            .entry(run.start_point.line)
            .or_default()
            .push_str(&run.text);
    }
    by_line
        .into_iter()
        .map(|(line, mut text)| {
            // Trailing whitespace carries no semantic content and triggers
            // verbose "space space space …" announcements on some platforms.
            let trimmed = text.trim_end_matches(' ');
            // Keep an emptied line as an empty run so the row structure and
            // line counts remain intact (AT can still report blank lines).
            if trimmed.is_empty() {
                text.clear();
            } else if trimmed.len() < text.len() {
                text.truncate(trimmed.len());
            }
            A11yTerminalLine { line, text }
        })
        .collect()
}

/// `true` for the "word" character class AccessKit expects for `word_starts`.
/// Matches the input_field convention (`[A-Za-z0-9_]`).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Build per-line `Role::TextRun` synthetic nodes without the
/// [`A11ySubtreeBuilder`], so the line/text-run assembly can be unit-tested
/// against arbitrary painted runs (mirroring `settings_ui::input_field`).
///
/// `synthetic_id` maps `(line, chunk_index)` to a stable [`accesskit::NodeId`];
/// in production this is `A11ySubtreeBuilder::synthetic_node_id`.
fn build_terminal_line_runs(
    runs: &[BatchedTextRun],
    synthetic_id: impl Fn(u64, u64) -> accesskit::NodeId,
) -> Vec<(accesskit::NodeId, accesskit::Node)> {
    let mut out = Vec::new();
    for line in collect_terminal_lines(runs) {
        let chars: Vec<char> = line.text.chars().collect();
        let total = chars.len();
        let num_chunks = total.div_ceil(MAX_CHARS_PER_A11Y_RUN).max(1);

        // Word boundaries, retained as character offsets into the whole line.
        let mut word_starts: Vec<usize> = Vec::new();
        let mut was_word = false;
        for (ix, c) in chars.iter().enumerate() {
            let is_word = is_word_char(*c);
            if is_word && !was_word {
                word_starts.push(ix);
            }
            was_word = is_word;
        }

        for chunk_index in 0..num_chunks {
            let char_start = chunk_index * MAX_CHARS_PER_A11Y_RUN;
            let char_end = (char_start + MAX_CHARS_PER_A11Y_RUN).min(total);
            let chunk: String = chars[char_start..char_end].iter().collect();

            let mut node = accesskit::Node::new(accesskit::Role::TextRun);
            node.set_text_direction(accesskit::TextDirection::LeftToRight);
            node.set_value(chunk);
            node.set_character_lengths(
                chars[char_start..char_end]
                    .iter()
                    .map(|c| c.len_utf8() as u8)
                    .collect::<Vec<u8>>(),
            );
            node.set_word_starts(
                word_starts
                    .iter()
                    .filter(|&&ws| ws >= char_start && ws < char_end)
                    .map(|&ws| (ws - char_start) as u8)
                    .collect::<Vec<u8>>(),
            );
            // Stable id per (line, chunk): the line is the primary key and the
            // chunk index distinguishes splits for very long lines.
            let node_id = synthetic_id(line.line as u64, chunk_index as u64);
            if chunk_index > 0 {
                node.set_previous_on_line(synthetic_id(line.line as u64, (chunk_index - 1) as u64));
            }
            if chunk_index + 1 < num_chunks {
                node.set_next_on_line(synthetic_id(line.line as u64, (chunk_index + 1) as u64));
            }
            out.push((node_id, node));
        }
    }
    out
}

/// Push per-line `Role::TextRun` synthetic children onto `builder`, derived
/// from the same `batched_text_runs` the element paints. Each run carries
/// `value`, `character_lengths` and `word_starts` so platform text patterns
/// can drive caret/review. Synthetic ids key off the parent node id + a
/// `(line, chunk)` key, so they are stable frame-to-frame.
fn push_terminal_line_text_runs(
    builder: &mut A11ySubtreeBuilder,
    runs: &[BatchedTextRun],
) {
    let runs = build_terminal_line_runs(runs, |line, chunk| {
        builder.synthetic_node_id((line, chunk))
    });
    for (id, node) in runs {
        builder.push_child(id, node);
    }
}

fn to_highlighted_range_lines(
    range: &Range,
    layout: &LayoutState,
    origin: GpuiPoint<Pixels>,
) -> Option<(Pixels, Vec<HighlightedRangeLine>)> {
    // Step 1. Normalize the points to be viewport relative.
    // When display_offset = 1, here's how the grid is arranged:
    //-2,0 -2,1...
    //--- Viewport top
    //-1,0 -1,1...
    //--------- Terminal Top
    // 0,0  0,1...
    // 1,0  1,1...
    //--- Viewport Bottom
    // 2,0  2,1...
    //--------- Terminal Bottom

    // Normalize to viewport relative, from terminal relative.
    // lines are i32s, which are negative above the top left corner of the terminal
    // If the user has scrolled, we use the display_offset to tell us which offset
    // of the grid data we should be looking at. But for the rendering step, we don't
    // want negatives. We want things relative to the 'viewport' (the area of the grid
    // which is currently shown according to the display offset)
    let display_offset = i32::try_from(layout.display_offset).unwrap_or(i32::MAX);
    let unclamped_start_line = range.start().line.saturating_add(display_offset);
    let unclamped_start_column = range.start().column;
    let unclamped_end_line = range.end().line.saturating_add(display_offset);
    let unclamped_end_column = range.end().column;

    // Step 2. Clamp range to viewport, and return None if it doesn't overlap
    if unclamped_end_line < 0 || unclamped_start_line > layout.dimensions.num_lines() as i32 {
        return None;
    }

    let clamped_start_line = unclamped_start_line.max(0) as usize;

    let clamped_end_line = unclamped_end_line.min(layout.dimensions.num_lines() as i32) as usize;

    // Convert the start of the range to pixels
    let start_y = origin.y + clamped_start_line as f32 * layout.dimensions.line_height;

    // Step 3. Expand ranges that cross lines into a collection of single-line ranges.
    //  (also convert to pixels)
    let mut highlighted_range_lines = Vec::new();
    for line in clamped_start_line..=clamped_end_line {
        let mut line_start = 0;
        let mut line_end = layout.dimensions.num_columns();

        if line == clamped_start_line && unclamped_start_line >= 0 {
            line_start = unclamped_start_column;
        }
        if line == clamped_end_line && unclamped_end_line <= layout.dimensions.num_lines() as i32 {
            line_end = unclamped_end_column + 1; // +1 for inclusive
        }

        highlighted_range_lines.push(HighlightedRangeLine {
            start_x: origin.x + line_start as f32 * layout.dimensions.cell_width,
            end_x: origin.x + line_end as f32 * layout.dimensions.cell_width,
        });
    }

    Some((start_y, highlighted_range_lines))
}

/// Converts a 2, 8, or 24 bit color ANSI color to the GPUI equivalent.
pub fn convert_color(fg: &Color, theme: &Theme) -> Hsla {
    let colors = theme.colors();
    match fg {
        // Named and theme defined colors
        Color::Named(color) => match color {
            NamedColor::Black => colors.terminal_ansi_black,
            NamedColor::Red => colors.terminal_ansi_red,
            NamedColor::Green => colors.terminal_ansi_green,
            NamedColor::Yellow => colors.terminal_ansi_yellow,
            NamedColor::Blue => colors.terminal_ansi_blue,
            NamedColor::Magenta => colors.terminal_ansi_magenta,
            NamedColor::Cyan => colors.terminal_ansi_cyan,
            NamedColor::White => colors.terminal_ansi_white,
            NamedColor::BrightBlack => colors.terminal_ansi_bright_black,
            NamedColor::BrightRed => colors.terminal_ansi_bright_red,
            NamedColor::BrightGreen => colors.terminal_ansi_bright_green,
            NamedColor::BrightYellow => colors.terminal_ansi_bright_yellow,
            NamedColor::BrightBlue => colors.terminal_ansi_bright_blue,
            NamedColor::BrightMagenta => colors.terminal_ansi_bright_magenta,
            NamedColor::BrightCyan => colors.terminal_ansi_bright_cyan,
            NamedColor::BrightWhite => colors.terminal_ansi_bright_white,
            NamedColor::Foreground => colors.terminal_foreground,
            NamedColor::Background => colors.terminal_ansi_background,
            NamedColor::Cursor => theme.players().local().cursor,
            NamedColor::DimBlack => colors.terminal_ansi_dim_black,
            NamedColor::DimRed => colors.terminal_ansi_dim_red,
            NamedColor::DimGreen => colors.terminal_ansi_dim_green,
            NamedColor::DimYellow => colors.terminal_ansi_dim_yellow,
            NamedColor::DimBlue => colors.terminal_ansi_dim_blue,
            NamedColor::DimMagenta => colors.terminal_ansi_dim_magenta,
            NamedColor::DimCyan => colors.terminal_ansi_dim_cyan,
            NamedColor::DimWhite => colors.terminal_ansi_dim_white,
            NamedColor::BrightForeground => colors.terminal_bright_foreground,
            NamedColor::DimForeground => colors.terminal_dim_foreground,
        },
        // 'True' colors
        Color::Spec(rgb) => terminal::rgba_color(rgb.r, rgb.g, rgb.b),
        // 8 bit, indexed colors
        Color::Indexed(i) => terminal::get_color_at_index(*i as usize, theme),
    }
}

/// §11.2 把 `Content` 里的图像引用换成缓存中已解码好的 `RenderImage`。
///
/// 可见性和视口行号在 `Terminal::sync` 里就算好了, 这里只做查表, 所以绘制
/// 一帧不会触发任何图像解码。
fn resolve_terminal_images(
    terminal: &Entity<Terminal>,
    cx: &mut App,
) -> Vec<(VisibleImage, std::sync::Arc<gpui::RenderImage>)> {
    let terminal = terminal.read(cx);
    let cache = terminal.image_cache();

    let mut images: Vec<(VisibleImage, std::sync::Arc<gpui::RenderImage>)> = terminal
        .last_content
        .images
        .iter()
        .filter_map(|visible| {
            let cached = cache.get(visible.id)?;
            Some((*visible, cached.image.render_image.clone()))
        })
        .collect();
    images.sort_by_key(|(visible, _)| visible.z_index);
    images
}

#[cfg(all(test, feature = "z3rm-migration"))]
mod tests {
    use super::*;
    use gpui::{AbsoluteLength, Hsla, font};
    use ui::utils::apca_contrast;

    #[test]
    fn test_is_decorative_character() {
        // Box Drawing characters (U+2500 to U+257F)
        assert!(TerminalElement::is_decorative_character('─')); // U+2500
        assert!(TerminalElement::is_decorative_character('│')); // U+2502
        assert!(TerminalElement::is_decorative_character('┌')); // U+250C
        assert!(TerminalElement::is_decorative_character('┐')); // U+2510
        assert!(TerminalElement::is_decorative_character('└')); // U+2514
        assert!(TerminalElement::is_decorative_character('┘')); // U+2518
        assert!(TerminalElement::is_decorative_character('┼')); // U+253C

        // Block Elements (U+2580 to U+259F)
        assert!(TerminalElement::is_decorative_character('▀')); // U+2580
        assert!(TerminalElement::is_decorative_character('▄')); // U+2584
        assert!(TerminalElement::is_decorative_character('█')); // U+2588
        assert!(TerminalElement::is_decorative_character('░')); // U+2591
        assert!(TerminalElement::is_decorative_character('▒')); // U+2592
        assert!(TerminalElement::is_decorative_character('▓')); // U+2593

        // Geometric Shapes - block/box-like subset (U+25A0 to U+25D7)
        assert!(TerminalElement::is_decorative_character('■')); // U+25A0
        assert!(TerminalElement::is_decorative_character('□')); // U+25A1
        assert!(TerminalElement::is_decorative_character('▲')); // U+25B2
        assert!(TerminalElement::is_decorative_character('▼')); // U+25BC
        assert!(TerminalElement::is_decorative_character('◆')); // U+25C6
        assert!(TerminalElement::is_decorative_character('●')); // U+25CF

        // The specific character from the issue
        assert!(TerminalElement::is_decorative_character('◗')); // U+25D7
        assert!(TerminalElement::is_decorative_character('◘')); // U+25D8 (now included in Geometric Shapes)
        assert!(TerminalElement::is_decorative_character('◙')); // U+25D9 (now included in Geometric Shapes)

        // Powerline symbols (Private Use Area)
        assert!(TerminalElement::is_decorative_character('\u{E0B0}')); // Powerline right triangle
        assert!(TerminalElement::is_decorative_character('\u{E0B2}')); // Powerline left triangle
        assert!(TerminalElement::is_decorative_character('\u{E0B4}')); // Powerline right half circle (the actual issue!)
        assert!(TerminalElement::is_decorative_character('\u{E0B6}')); // Powerline left half circle
        assert!(TerminalElement::is_decorative_character('\u{E0CA}')); // Powerline mirrored ice waveform
        assert!(TerminalElement::is_decorative_character('\u{E0D7}')); // Powerline left triangle inverted

        // Characters that should NOT be considered decorative
        assert!(!TerminalElement::is_decorative_character('A')); // Regular letter
        assert!(!TerminalElement::is_decorative_character('$')); // Symbol
        assert!(!TerminalElement::is_decorative_character(' ')); // Space
        assert!(!TerminalElement::is_decorative_character('←')); // U+2190 (Arrow, not in our ranges)
        assert!(!TerminalElement::is_decorative_character('→')); // U+2192 (Arrow, not in our ranges)
        assert!(!TerminalElement::is_decorative_character('\u{F00C}')); // Font Awesome check (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{E711}')); // Devicons (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{EA71}')); // Codicons folder (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{F401}')); // Octicons (icon, needs contrast)
        assert!(!TerminalElement::is_decorative_character('\u{1F600}')); // Emoji (not in our ranges)
    }

    #[test]
    fn test_decorative_character_boundary_cases() {
        // Test exact boundaries of our ranges
        // Box Drawing range boundaries
        assert!(TerminalElement::is_decorative_character('\u{2500}')); // First char
        assert!(TerminalElement::is_decorative_character('\u{257F}')); // Last char
        assert!(!TerminalElement::is_decorative_character('\u{24FF}')); // Just before

        // Block Elements range boundaries
        assert!(TerminalElement::is_decorative_character('\u{2580}')); // First char
        assert!(TerminalElement::is_decorative_character('\u{259F}')); // Last char

        // Geometric Shapes subset boundaries
        assert!(TerminalElement::is_decorative_character('\u{25A0}')); // First char
        assert!(TerminalElement::is_decorative_character('\u{25FF}')); // Last char
        assert!(!TerminalElement::is_decorative_character('\u{2600}')); // Just after
    }

    #[test]
    fn test_decorative_characters_bypass_contrast_adjustment() {
        // Decorative characters should not be affected by contrast adjustment

        // The specific character from issue #34234
        let problematic_char = '◗'; // U+25D7
        assert!(
            TerminalElement::is_decorative_character(problematic_char),
            "Character ◗ (U+25D7) should be recognized as decorative"
        );

        // Verify some other commonly used decorative characters
        assert!(TerminalElement::is_decorative_character('│')); // Vertical line
        assert!(TerminalElement::is_decorative_character('─')); // Horizontal line
        assert!(TerminalElement::is_decorative_character('█')); // Full block
        assert!(TerminalElement::is_decorative_character('▓')); // Dark shade
        assert!(TerminalElement::is_decorative_character('■')); // Black square
        assert!(TerminalElement::is_decorative_character('●')); // Black circle

        // Verify normal text characters are NOT decorative
        assert!(!TerminalElement::is_decorative_character('A'));
        assert!(!TerminalElement::is_decorative_character('1'));
        assert!(!TerminalElement::is_decorative_character('$'));
        assert!(!TerminalElement::is_decorative_character(' '));
    }

    #[test]
    fn test_is_app_chosen_exact_color() {
        use terminal::{Color, NamedColor, Rgb};

        // Indices 0..=15 are theme-overridable ANSI colors; contrast adjustment must still apply.
        assert!(!TerminalElement::is_app_chosen_exact_color(
            &Color::Indexed(0)
        ));
        assert!(!TerminalElement::is_app_chosen_exact_color(
            &Color::Indexed(15)
        ));

        // Boundary: index 16 is the first entry of the 6x6x6 cube — application-chosen.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            16
        )));
        // Interior of the cube.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            17
        )));
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            231
        )));
        // Grayscale ramp boundaries.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            232
        )));
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Indexed(
            255
        )));

        // 24-bit true color is always application-chosen.
        assert!(TerminalElement::is_app_chosen_exact_color(&Color::Spec(
            Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        )));

        // Named colors are theme-defined and must go through contrast adjustment.
        assert!(!TerminalElement::is_app_chosen_exact_color(&Color::Named(
            NamedColor::Red
        )));
        assert!(!TerminalElement::is_app_chosen_exact_color(&Color::Named(
            NamedColor::Foreground
        )));
    }

    #[test]
    fn test_contrast_adjustment_logic() {
        // Test the core contrast adjustment logic without needing full app context

        // Test case 1: Light colors (poor contrast)
        let white_fg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 1.0,
            a: 1.0,
        };
        let light_gray_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.95,
            a: 1.0,
        };

        // Should have poor contrast
        let actual_contrast = apca_contrast(white_fg, light_gray_bg).abs();
        assert!(
            actual_contrast < 30.0,
            "White on light gray should have poor APCA contrast: {}",
            actual_contrast
        );

        // After adjustment with minimum APCA contrast of 45, should be darker
        let adjusted = ensure_minimum_contrast(white_fg, light_gray_bg, 45.0);
        assert!(
            adjusted.l < white_fg.l,
            "Adjusted color should be darker than original"
        );
        let adjusted_contrast = apca_contrast(adjusted, light_gray_bg).abs();
        assert!(adjusted_contrast >= 45.0, "Should meet minimum contrast");

        // Test case 2: Dark colors (poor contrast)
        let black_fg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 1.0,
        };
        let dark_gray_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.05,
            a: 1.0,
        };

        // Should have poor contrast
        let actual_contrast = apca_contrast(black_fg, dark_gray_bg).abs();
        assert!(
            actual_contrast < 30.0,
            "Black on dark gray should have poor APCA contrast: {}",
            actual_contrast
        );

        // After adjustment with minimum APCA contrast of 45, should be lighter
        let adjusted = ensure_minimum_contrast(black_fg, dark_gray_bg, 45.0);
        assert!(
            adjusted.l > black_fg.l,
            "Adjusted color should be lighter than original"
        );
        let adjusted_contrast = apca_contrast(adjusted, dark_gray_bg).abs();
        assert!(adjusted_contrast >= 45.0, "Should meet minimum contrast");

        // Test case 3: Already good contrast
        let good_contrast = ensure_minimum_contrast(black_fg, white_fg, 45.0);
        assert_eq!(
            good_contrast, black_fg,
            "Good contrast should not be adjusted"
        );
    }

    #[test]
    fn test_true_color_red_blue_not_washed_out_on_dark_bg() {
        // Red and blue have inherently low perceptual luminance in APCA.
        // Pure #ff0000 only achieves Lc ~35 against #1e1e1e — below the
        // default Lc 45 threshold. ensure_minimum_contrast would lighten
        // them, washing out the color. This is why cell_style skips the
        // adjustment for Color::Spec (24-bit true color).
        let dark_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.05,
            a: 1.0,
        };

        for (name, r, g, b) in [
            ("red", 225, 80, 80),
            ("blue", 80, 80, 225),
            ("pure red", 255, 0, 0),
        ] {
            let color = terminal::rgba_color(r, g, b);
            let contrast = apca_contrast(color, dark_bg).abs();
            assert!(
                contrast < 45.0,
                "{name} should have APCA < 45 on dark bg, got {contrast}",
            );

            let adjusted = ensure_minimum_contrast(color, dark_bg, 45.0);
            assert!(
                adjusted.l > color.l,
                "{name} would be lightened by contrast adjustment (l: {} -> {})",
                color.l,
                adjusted.l,
            );
        }
    }

    #[test]
    fn test_white_on_white_contrast_issue() {
        // This test reproduces the exact issue from the bug report
        // where white ANSI text on white background should be adjusted

        // Simulate One Light theme colors
        let white_fg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.98, // #fafafaff is approximately 98% lightness
            a: 1.0,
        };
        let white_bg = gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.98, // Same as foreground - this is the problem!
            a: 1.0,
        };

        // With minimum contrast of 0.0, no adjustment should happen
        let no_adjust = ensure_minimum_contrast(white_fg, white_bg, 0.0);
        assert_eq!(no_adjust, white_fg, "No adjustment with min_contrast 0.0");

        // With minimum APCA contrast of 15, it should adjust to a darker color
        let adjusted = ensure_minimum_contrast(white_fg, white_bg, 15.0);
        assert!(
            adjusted.l < white_fg.l,
            "White on white should become darker, got l={}",
            adjusted.l
        );

        // Verify the contrast is now acceptable
        let new_contrast = apca_contrast(adjusted, white_bg).abs();
        assert!(
            new_contrast >= 15.0,
            "Adjusted APCA contrast {} should be >= 15.0",
            new_contrast
        );
    }

    #[test]
    fn test_batched_text_run_can_append() {
        let style1 = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let style2 = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let style3 = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::blue(), // Different color
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let batch = BatchedTextRun::new_from_char(LayoutPoint::new(0, 0), 'a', style1, font_size);

        // Should be able to append same style
        assert!(batch.can_append(&style2));

        // Should not be able to append different style
        assert!(!batch.can_append(&style3));
    }

    #[test]
    fn test_batched_text_run_append() {
        let style = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let mut batch =
            BatchedTextRun::new_from_char(LayoutPoint::new(0, 0), 'a', style, font_size);

        assert_eq!(batch.text, "a");
        assert_eq!(batch.cell_count, 1);
        assert_eq!(batch.style.len, 1);

        batch.append_char('b');

        assert_eq!(batch.text, "ab");
        assert_eq!(batch.cell_count, 2);
        assert_eq!(batch.style.len, 2);

        batch.append_char('c');

        assert_eq!(batch.text, "abc");
        assert_eq!(batch.cell_count, 3);
        assert_eq!(batch.style.len, 3);
    }

    #[test]
    fn test_batched_text_run_append_char() {
        let style = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let mut batch =
            BatchedTextRun::new_from_char(LayoutPoint::new(0, 0), 'x', style, font_size);

        assert_eq!(batch.text, "x");
        assert_eq!(batch.cell_count, 1);
        assert_eq!(batch.style.len, 1);

        batch.append_char('y');

        assert_eq!(batch.text, "xy");
        assert_eq!(batch.cell_count, 2);
        assert_eq!(batch.style.len, 2);

        // Test with multi-byte character
        batch.append_char('😀');

        assert_eq!(batch.text, "xy😀");
        assert_eq!(batch.cell_count, 3);
        assert_eq!(batch.style.len, 6); // 1 + 1 + 4 bytes for emoji
    }

    #[test]
    fn test_batched_text_run_append_zero_width_char() {
        let style = TextRun {
            len: 1,
            font: font("Helvetica"),
            color: Hsla::red(),
            ..Default::default()
        };

        let font_size = AbsoluteLength::Pixels(px(12.0));
        let mut batch =
            BatchedTextRun::new_from_char(LayoutPoint::new(0, 0), 'x', style, font_size);

        let combining = '\u{0301}';
        batch.append_zero_width_chars(&[combining]);

        assert_eq!(batch.text, format!("x{}", combining));
        assert_eq!(batch.cell_count, 1);
        assert_eq!(batch.style.len, 1 + combining.len_utf8());
    }

    #[test]
    fn test_background_region_can_merge() {
        let color1 = Hsla::red();
        let color2 = Hsla::blue();

        // Test horizontal merging
        let mut region1 = BackgroundRegion::new(0, 0, color1);
        region1.end_col = 5;
        let region2 = BackgroundRegion::new(0, 6, color1);
        assert!(region1.can_merge_with(&region2));

        // Test vertical merging with same column span
        let mut region3 = BackgroundRegion::new(0, 0, color1);
        region3.end_col = 5;
        let mut region4 = BackgroundRegion::new(1, 0, color1);
        region4.end_col = 5;
        assert!(region3.can_merge_with(&region4));

        // Test cannot merge different colors
        let region5 = BackgroundRegion::new(0, 0, color1);
        let region6 = BackgroundRegion::new(0, 1, color2);
        assert!(!region5.can_merge_with(&region6));

        // Test cannot merge non-adjacent regions
        let region7 = BackgroundRegion::new(0, 0, color1);
        let region8 = BackgroundRegion::new(0, 2, color1);
        assert!(!region7.can_merge_with(&region8));

        // Test cannot merge vertical regions with different column spans
        let mut region9 = BackgroundRegion::new(0, 0, color1);
        region9.end_col = 5;
        let mut region10 = BackgroundRegion::new(1, 0, color1);
        region10.end_col = 6;
        assert!(!region9.can_merge_with(&region10));
    }

    #[test]
    fn test_background_region_merge() {
        let color = Hsla::red();

        // Test horizontal merge
        let mut region1 = BackgroundRegion::new(0, 0, color);
        region1.end_col = 5;
        let mut region2 = BackgroundRegion::new(0, 6, color);
        region2.end_col = 10;
        region1.merge_with(&region2);
        assert_eq!(region1.start_col, 0);
        assert_eq!(region1.end_col, 10);
        assert_eq!(region1.start_line, 0);
        assert_eq!(region1.end_line, 0);

        // Test vertical merge
        let mut region3 = BackgroundRegion::new(0, 0, color);
        region3.end_col = 5;
        let mut region4 = BackgroundRegion::new(1, 0, color);
        region4.end_col = 5;
        region3.merge_with(&region4);
        assert_eq!(region3.start_col, 0);
        assert_eq!(region3.end_col, 5);
        assert_eq!(region3.start_line, 0);
        assert_eq!(region3.end_line, 1);
    }

    #[test]
    fn test_merge_background_regions() {
        let color = Hsla::red();

        // Test merging multiple adjacent regions
        let regions = vec![
            BackgroundRegion::new(0, 0, color),
            BackgroundRegion::new(0, 1, color),
            BackgroundRegion::new(0, 2, color),
            BackgroundRegion::new(1, 0, color),
            BackgroundRegion::new(1, 1, color),
            BackgroundRegion::new(1, 2, color),
        ];

        let merged = merge_background_regions(regions);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_line, 0);
        assert_eq!(merged[0].end_line, 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 2);

        // Test with non-mergeable regions
        let color2 = Hsla::blue();
        let regions2 = vec![
            BackgroundRegion::new(0, 0, color),
            BackgroundRegion::new(0, 2, color),  // Gap at column 1
            BackgroundRegion::new(1, 0, color2), // Different color
        ];

        let merged2 = merge_background_regions(regions2);
        assert_eq!(merged2.len(), 3);
    }

    #[test]
    fn test_screen_position_filtering_with_positive_lines() {
        // Test the unified screen-position-based filtering approach.
        // This works for both Scrollable and Inline modes because we filter
        // by enumerated line group index, not by cell.point.line values.
        use itertools::Itertools;
        use terminal::{Cell, IndexedCell, Point};

        // Create mock cells for lines 0-23 (typical terminal with 24 visible lines)
        let mut cells = Vec::new();
        for line in 0..24i32 {
            for col in 0..3i32 {
                cells.push(IndexedCell {
                    point: Point::new(line, col as usize),
                    cell: Cell::default(),
                });
            }
        }

        // Scenario: Terminal partially scrolled above viewport
        // First 5 lines (0-4) are clipped, lines 5-15 should be visible
        let rows_above_viewport = 5usize;
        let visible_row_count = 11usize;

        // Apply the same filtering logic as in the render code
        let filtered: Vec<_> = cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_above_viewport)
            .take(visible_row_count)
            .flat_map(|(_, line_cells)| line_cells)
            .collect();

        // Should have lines 5-15 (11 lines * 3 cells each = 33 cells)
        assert_eq!(filtered.len(), 11 * 3, "Should have 33 cells for 11 lines");

        // First filtered cell should be line 5
        assert_eq!(
            filtered.first().unwrap().point.line,
            5,
            "First cell should be on line 5"
        );

        // Last filtered cell should be line 15
        assert_eq!(
            filtered.last().unwrap().point.line,
            15,
            "Last cell should be on line 15"
        );
    }

    #[test]
    fn test_screen_position_filtering_with_negative_lines() {
        // This is the key test! In Scrollable mode, cells have NEGATIVE line numbers
        // for scrollback history. The screen-position filtering approach works because
        // we filter by enumerated line group index, not by cell.point.line values.
        use itertools::Itertools;
        use terminal::{Cell, IndexedCell, Point};

        // Simulate cells from a scrolled terminal with scrollback
        // These have negative line numbers representing scrollback history
        let mut scrollback_cells = Vec::new();
        for line in -588i32..=-578i32 {
            for col in 0..80i32 {
                scrollback_cells.push(IndexedCell {
                    point: Point::new(line, col as usize),
                    cell: Cell::default(),
                });
            }
        }

        // Scenario: First 3 screen rows clipped, show next 5 rows
        let rows_above_viewport = 3usize;
        let visible_row_count = 5usize;

        // Apply the same filtering logic as in the render code
        let filtered: Vec<_> = scrollback_cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_above_viewport)
            .take(visible_row_count)
            .flat_map(|(_, line_cells)| line_cells)
            .collect();

        // Should have 5 lines * 80 cells = 400 cells
        assert_eq!(filtered.len(), 5 * 80, "Should have 400 cells for 5 lines");

        // First filtered cell should be line -585 (skipped 3 lines from -588)
        assert_eq!(
            filtered.first().unwrap().point.line,
            -585,
            "First cell should be on line -585"
        );

        // Last filtered cell should be line -581 (5 lines: -585, -584, -583, -582, -581)
        assert_eq!(
            filtered.last().unwrap().point.line,
            -581,
            "Last cell should be on line -581"
        );
    }

    #[test]
    fn test_screen_position_filtering_skip_all() {
        // Test what happens when we skip more rows than exist
        use itertools::Itertools;
        use terminal::{Cell, IndexedCell, Point};

        let mut cells = Vec::new();
        for line in 0..10i32 {
            cells.push(IndexedCell {
                point: Point::new(line, 0),
                cell: Cell::default(),
            });
        }

        // Skip more rows than exist
        let rows_above_viewport = 100usize;
        let visible_row_count = 5usize;

        let filtered: Vec<_> = cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_above_viewport)
            .take(visible_row_count)
            .flat_map(|(_, line_cells)| line_cells)
            .collect();

        assert_eq!(
            filtered.len(),
            0,
            "Should have no cells when all are skipped"
        );
    }

    #[test]
    fn test_layout_grid_positioning_math() {
        // Test the math that layout_grid uses for positioning.
        // When we skip N rows, we pass N as start_line_offset to layout_grid,
        // which positions the first visible line at screen row N.

        // Scenario: Terminal at y=-100px, line_height=20px
        // First 5 screen rows are above viewport (clipped)
        // So we skip 5 rows and pass offset=5 to layout_grid

        let terminal_origin_y = -100.0f32;
        let line_height = 20.0f32;
        let rows_skipped = 5;

        // The first visible line (at offset 5) renders at:
        // y = terminal_origin + offset * line_height = -100 + 5*20 = 0
        let first_visible_y = terminal_origin_y + rows_skipped as f32 * line_height;
        assert_eq!(
            first_visible_y, 0.0,
            "First visible line should be at viewport top (y=0)"
        );

        // The 6th visible line (at offset 10) renders at:
        let sixth_visible_y = terminal_origin_y + (rows_skipped + 5) as f32 * line_height;
        assert_eq!(
            sixth_visible_y, 100.0,
            "6th visible line should be at y=100"
        );
    }

    #[test]
    fn display_cursor_maps_screen_and_history_lines_to_viewport_rows() {
        use terminal::Point;

        // Normal screen (display_offset = 0): the absolute grid line is the
        // viewport row, so the cursor sits on the row that contains its cell.
        let cursor = DisplayCursor::from(Point::new(0, 3), 0);
        assert_eq!((cursor.line(), cursor.col()), (0, 3));
        let cursor = DisplayCursor::from(Point::new(9, 0), 0);
        assert_eq!((cursor.line(), cursor.col()), (9, 0));

        // Scrolled back 2 rows: the viewport starts at absolute line -2, so a
        // history row and a screen row map to distinct viewport rows, and the
        // cursor lands on the same row as the cell it covers. The display
        // offset is applied exactly once — a second application would shift
        // the cursor off its cell by the scroll amount.
        let history_cursor = DisplayCursor::from(Point::new(-2, 1), 2);
        assert_eq!((history_cursor.line(), history_cursor.col()), (0, 1));
        let screen_cursor = DisplayCursor::from(Point::new(0, 1), 2);
        assert_eq!((screen_cursor.line(), screen_cursor.col()), (2, 1));

        // A cursor on a screen row scrolled out of view maps past the viewport
        // bottom; cursor_position must reject it so history never renders a
        // cursor over its own scrollback.
        let below_viewport = DisplayCursor::from(Point::new(1, 0), 2);
        assert_eq!(below_viewport.line(), 3);
    }

    #[test]
    fn cursor_position_hides_cursor_scrolled_out_of_viewport() {
        let line_height = px(18.2);
        let cell_width = px(8.4);
        let dimensions = TerminalBounds::new(
            line_height,
            cell_width,
            Bounds {
                origin: point(px(10.0), px(10.0)),
                size: size(px(84.0), px(36.4)), // 2 rows
            },
        );
        assert_eq!(dimensions.num_lines(), 2);

        // Screen row 0 while scrolled back 2: viewport row 2 is past the
        // 2-row viewport, so the cursor is hidden.
        let scrolled_out =
            DisplayCursor::from(Point::new(0, 0), 2);
        assert!(
            TerminalElement::cursor_position(scrolled_out, dimensions, dimensions.bounds.origin, 1.0)
                .is_none(),
            "cursor below the viewport must be hidden"
        );

        // History row -2 while scrolled back 2 is viewport row 0: visible.
        let visible_history = DisplayCursor::from(Point::new(-2, 0), 2);
        let position =
            TerminalElement::cursor_position(visible_history, dimensions, dimensions.bounds.origin, 1.0);
        assert!(position.is_some(), "cursor inside the viewport must render");
        assert_eq!(position.unwrap().y, px(0.0));
    }

    #[test]
    fn cursor_and_glyphs_share_device_pixel_boundaries() {
        // Fractional cell metrics at 1.25x scale: col*cell_width and
        // line*line_height are not whole device pixels, so a logical-pixel
        // floor (the historical cursor quantization) lands the cursor on a
        // different boundary than the glyphs.
        let line_height = px(18.2);
        let cell_width = px(8.4);
        let origin = point(px(10.0), px(10.0)); // already device-snapped
        let scale_factor = 1.25;
        let dimensions = TerminalBounds::new(
            line_height,
            cell_width,
            Bounds {
                origin,
                size: size(px(84.0), px(182.0)),
            },
        );

        // col=3, line=2: abs x = 10 + 3*8.4 = 35.2 -> 44.0 device px
        // (integral); abs y = 10 + 2*18.2 = 46.4 -> 58.0 device px (integral).
        let snapped = snapped_cell_point(origin, 2, 3, &dimensions, scale_factor);
        assert_eq!(snapped, point(px(35.2), px(46.4)));
        assert_eq!(f32::from(snapped.x * scale_factor), 44.0);
        assert_eq!(f32::from(snapped.y * scale_factor), 58.0);

        // The cursor uses the same quantization as the glyph run for its cell.
        let cursor = TerminalElement::cursor_position(
            DisplayCursor::from(Point::new(2, 3), 0),
            dimensions,
            origin,
            scale_factor,
        )
        .unwrap();
        assert_eq!(
            cursor,
            point(snapped.x - origin.x, snapped.y - origin.y),
            "cursor must match the glyph boundary"
        );

        // Regression: the old code floored in logical pixels
        // (floor(3*8.4)=25.0, floor(2*18.2)=36.0), which is a different
        // boundary than the device grid.
        assert_ne!(cursor, point(px(25.0), px(36.0)));

        // col=2, line=0: abs x = 10 + 2*8.4 = 26.8 -> 33.5 device px rounds up
        // to 34, so the snapped position is 27.2 logical px, not the raw 26.8.
        let snapped = snapped_cell_point(origin, 0, 2, &dimensions, scale_factor);
        assert_eq!(snapped.x, px(27.2));
        assert_eq!(f32::from(snapped.x * scale_factor), 34.0);

        // Every cell boundary in a row lands on a device pixel, so no column
        // (or the cursor on it) jitters between two pixels at this scale.
        for col in 0..10 {
            let snapped = snapped_cell_point(origin, 1, col, &dimensions, scale_factor);
            let device_x = f32::from(snapped.x * scale_factor);
            let boundary_x = snapped.x;
            assert!(
                (device_x - device_x.round()).abs() < 1e-3,
                "col {col} boundary {boundary_x:?} is not device-integral"
            );
        }
    }

    #[test]
    fn cursor_and_glyphs_share_boundaries_after_fractional_scroll() {
        let line_height = px(18.2);
        let cell_width = px(8.4);
        let dimensions = TerminalBounds::new(
            line_height,
            cell_width,
            Bounds {
                origin: point(px(10.0), px(10.0)),
                size: size(px(84.0), px(182.0)),
            },
        );
        let scale_factor = 1.25;
        let scroll_top = px(0.4);
        let snap_origin = |value: Pixels| {
            Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
        };
        let paint_origin = point(
            snap_origin(dimensions.bounds.origin.x),
            snap_origin(dimensions.bounds.origin.y - scroll_top),
        );
        let glyph = snapped_cell_point(paint_origin, 2, 3, &dimensions, scale_factor);
        let cursor = TerminalElement::cursor_position(
            DisplayCursor::from(Point::new(2, 3), 0),
            dimensions,
            paint_origin,
            scale_factor,
        )
        .unwrap();
        let cursor = paint_origin + cursor;

        assert_eq!(
            cursor, glyph,
            "cursor and glyph must share a device-pixel boundary after scrolling"
        );
    }

    #[test]
    fn standalone_anchor_keeps_alternate_screen_pinned_to_bottom() {
        // Full-screen TUI apps run on the alternate screen: they never scroll
        // and may not occupy the bottom row, but the grid must still be pinned
        // to the bottom of the available height (upstream condition restored).
        let alt_screen = Content {
            mode: Modes::ALT_SCREEN,
            ..Content::default()
        };
        assert!(should_anchor_to_bottom(&alt_screen));

        // Primary screen: anchor only when scrolled to bottom AND the bottom
        // row is occupied.
        let scrolled_with_content = Content {
            scrolled_to_bottom: true,
            bottom_row_occupied: true,
            ..Content::default()
        };
        assert!(should_anchor_to_bottom(&scrolled_with_content));

        // Scrolled to bottom but the last row is empty: keep the padding below
        // the grid so the prompt stays at the top.
        let scrolled_with_empty_bottom = Content {
            scrolled_to_bottom: true,
            bottom_row_occupied: false,
            ..Content::default()
        };
        assert!(!should_anchor_to_bottom(&scrolled_with_empty_bottom));

        // Scrolled back into history: never anchor.
        let scrolled_up = Content {
            scrolled_to_bottom: false,
            bottom_row_occupied: true,
            ..Content::default()
        };
        assert!(!should_anchor_to_bottom(&scrolled_up));
    }

    #[test]
    fn test_unified_filtering_works_for_both_modes() {
        // This test proves that the unified screen-position filtering approach
        // works for BOTH positive line numbers (Inline mode) and negative line
        // numbers (Scrollable mode with scrollback).
        //
        // The key insight: we filter by enumerated line group index (screen position),
        // not by cell.point.line values. This makes the filtering agnostic to the
        // actual line numbers in the cells.
        use itertools::Itertools;
        use terminal::Point;
        use terminal::{Cell, IndexedCell};

        // Test with positive line numbers (Inline mode style)
        let positive_cells: Vec<_> = (0..10i32)
            .flat_map(|line| {
                (0..3i32).map(move |col| IndexedCell {
                    point: Point::new(line, col as usize),
                    cell: Cell::default(),
                })
            })
            .collect();

        // Test with negative line numbers (Scrollable mode with scrollback)
        let negative_cells: Vec<_> = (-10i32..0i32)
            .flat_map(|line| {
                (0..3i32).map(move |col| IndexedCell {
                    point: Point::new(line, col as usize),
                    cell: Cell::default(),
                })
            })
            .collect();

        let rows_to_skip = 3usize;
        let rows_to_take = 4usize;

        // Filter positive cells
        let positive_filtered: Vec<_> = positive_cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_to_skip)
            .take(rows_to_take)
            .flat_map(|(_, cells)| cells)
            .collect();

        // Filter negative cells
        let negative_filtered: Vec<_> = negative_cells
            .iter()
            .chunk_by(|c| c.point.line)
            .into_iter()
            .skip(rows_to_skip)
            .take(rows_to_take)
            .flat_map(|(_, cells)| cells)
            .collect();

        // Both should have same count: 4 lines * 3 cells = 12
        assert_eq!(positive_filtered.len(), 12);
        assert_eq!(negative_filtered.len(), 12);

        // Positive: lines 3, 4, 5, 6
        assert_eq!(positive_filtered.first().unwrap().point.line, 3);
        assert_eq!(positive_filtered.last().unwrap().point.line, 6);

        // Negative: lines -7, -6, -5, -4
        assert_eq!(negative_filtered.first().unwrap().point.line, -7);
        assert_eq!(negative_filtered.last().unwrap().point.line, -4);
    }

    // ---- Accessibility: synthetic terminal line text runs ------------------

    /// Build a [`BatchedTextRun`] with a stable default style for testing.
    fn a11y_run(line: i32, text: &str) -> BatchedTextRun {
        BatchedTextRun {
            start_point: LayoutPoint::new(line, 0),
            text: text.into(),
            cell_count: text.chars().count(),
            style: TextRun {
                len: text.len(),
                font: font("Helvetica"),
                color: Hsla::red(),
                ..Default::default()
            },
            font_size: AbsoluteLength::Pixels(px(14.)),
        }
    }

    /// A synthetic id function for tests: encodes `(line << 16) | chunk` so
    /// ids are deterministic and unique. Mirrors how a real
    /// `A11ySubtreeBuilder::synthetic_node_id` derives ids from a key.
    fn fake_id(line: u64, chunk: u64) -> accesskit::NodeId {
        accesskit::NodeId((line << 16) | chunk)
    }

    /// `value` on a TextRun node is what screen readers announce. The synthetic
    /// runs must expose exactly the visible line text — one node per row — so
    /// review commands read the terminal surface.
    #[test]
    fn a11y_synthetic_runs_expose_visible_line_text() {
        let runs = vec![
            a11y_run(0, "hello "),
            a11y_run(0, "world"),
            a11y_run(1, "git status"),
            // A fully-blank row must still produce a (empty) run so row counts
            // are preserved and AT can report the blank line.
            a11y_run(2, "        "),
        ];
        let nodes = build_terminal_line_runs(&runs, fake_id);
        let values: Vec<&str> = nodes.iter().filter_map(|(_, n)| n.value()).collect();
        assert_eq!(
            values,
            vec!["hello world", "git status", ""],
            "each visible line becomes one TextRun, trailing blanks trimmed",
        );
        for (_, node) in &nodes {
            assert_eq!(node.role(), accesskit::Role::TextRun);
        }
    }

    /// Runs on the same painted line are concatenated in order, regardless of
    /// how [`layout_grid`] batched them by style. Without this, a recolored
    /// word would split a single terminal row into multiple a11y nodes and
    /// screen readers would announce "hello _world_" as separate fragments.
    #[test]
    fn a11y_concatenates_multicolored_runs_on_same_line() {
        let runs = vec![
            a11y_run(3, "foo"),
            a11y_run(3, "-"),
            a11y_run(3, "bar"),
            a11y_run(4, "baz"),
        ];
        let nodes = build_terminal_line_runs(&runs, fake_id);
        let values: Vec<String> = nodes
            .iter()
            .map(|(_, n)| n.value().unwrap_or_default().to_string())
            .collect();
        assert_eq!(values, vec!["foo-bar", "baz"]);
        // Three runs on line 3 collapse to a single TextRun node.
        assert_eq!(nodes.iter().filter(|(_, n)| n.value() == Some("foo-bar")).count(), 1);
    }

    /// A single line longer than [`MAX_CHARS_PER_A11Y_RUN`] characters must
    /// split into chunks — AccessKit's `word_starts` is `u8`-indexed, so a run
    /// with more than 255 chars would silently corrupt word boundaries. The
    /// chunks must carry `previous_on_line` / `next_on_line` so review can walk
    /// the line as one logical run.
    #[test]
    fn a11y_long_line_splits_across_chunks_with_line_links() {
        let long: String = "a".repeat(MAX_CHARS_PER_A11Y_RUN * 2 + 7);
        let runs = vec![a11y_run(0, &long)];
        let nodes = build_terminal_line_runs(&runs, fake_id);

        // 2*255 + 7 chars ⇒ ceil(517 / 255) = 3 chunks.
        assert_eq!(nodes.len(), 3);
        let total: usize = nodes
            .iter()
            .map(|(_, n)| n.value().map(str::len).unwrap_or(0))
            .sum();
        assert_eq!(total, long.len(), "chunk values concatenate back to the line");

        // Chunk 0 has `next_on_line`, chunk 2 has `previous_on_line`, chunk 1
        // has both. The middle chunk must not be orphaned: it links both ways.
        let has_next = |i: usize| {
            nodes[i].1.next_on_line().is_some()
        };
        let has_prev = |i: usize| nodes[i].1.previous_on_line().is_some();
        assert!(has_next(0) && !has_prev(0), "first chunk links forward only");
        assert!(has_next(1) && has_prev(1), "middle chunk links both ways");
        assert!(!has_next(2) && has_prev(2), "last chunk links backward only");

        // Chunks link to the neighbouring chunks' ids.
        assert_eq!(
            nodes[0].1.next_on_line(),
            Some(fake_id(0, 1)),
        );
        assert_eq!(
            nodes[2].1.previous_on_line(),
            Some(fake_id(0, 1)),
        );
    }

    /// `character_lengths` and `word_starts` are the inputs the platform text
    /// pattern uses for caret navigation and by-word review. A multi-byte
    /// glyph (é = 2 UTF-8 bytes) and two words ("foo bar") must yield the
    /// expected layouts: one length per char, a word boundary at "bar".
    #[test]
    fn a11y_run_carries_character_lengths_and_word_starts() {
        let runs = vec![a11y_run(0, "café bar")]; // c a f é [space] b a r
        let nodes = build_terminal_line_runs(&runs, fake_id);
        assert_eq!(nodes.len(), 1);
        let node = &nodes[0].1;
        assert_eq!(node.character_lengths(), &[1u8, 1, 1, 2, 1, 1, 1, 1][..]);
        // Words: "café" (start 0), "bar" (start 5). é is alphanumeric ⇒ part
        // of "café".
        assert_eq!(node.word_starts(), &[0u8, 5][..]);
    }

    /// Trailing-blank rows, and rows that are entirely whitespace, must not
    /// collapse screen-reader "wall of spaces". The blank row keeps an empty
    /// value (so the row count is stable) instead of retaining the spaces.
    #[test]
    fn a11y_trailing_blanks_are_trimmed_not_announced() {
        let runs = vec![
            a11y_run(0, "done.   "), // trailing spaces within a non-empty line
            a11y_run(1, "   "), // entirely blank row
        ];
        let nodes = build_terminal_line_runs(&runs, fake_id);
        let values: Vec<&str> = nodes.iter().map(|(_, n)| n.value().unwrap_or_default()).collect();
        assert_eq!(values, vec!["done.", ""]);
    }

    /// §16.4 / goal a11y stress: a full 80x24 terminal screen of mixed
    /// content (colored output, blank lines, long lines, unicode) must
    /// produce a bounded, correct set of TextRun nodes without panicking.
    /// This exercises the a11y transformation under realistic load.
    #[test]
    fn a11y_stress_full_screen_mixed_content() {
        let mut runs = Vec::new();
        for line in 0..24i32 {
            match line % 4 {
                0 => {
                    // Normal colored output — two style runs per line.
                    runs.push(a11y_run(line, "\\x1b[32mPASS\\x1b[0m"));
                    runs.push(a11y_run(line, "  test_module::case_"));
                }
                1 => {
                    // Long line near the chunk boundary.
                    let text = "x".repeat(MAX_CHARS_PER_A11Y_RUN + 10);
                    runs.push(a11y_run(line, &text));
                }
                2 => {
                    // Blank line.
                    runs.push(a11y_run(line, ""));
                }
                _ => {
                    // Unicode: multi-byte chars.
                    runs.push(a11y_run(line, "日本語テスト"));
                }
            }
        }

        let nodes = build_terminal_line_runs(&runs, fake_id);

        // 24 lines must produce at least 24 nodes (long line splits → more).
        assert!(
            nodes.len() >= 24,
            "expected ≥24 nodes for 24 lines, got {}",
            nodes.len()
        );

        // Node count must be bounded: each line produces at most
        // ceil(len / MAX_CHARS_PER_A11Y_RUN) + 1 chunks.
        let max_expected = 24 * 4;
        assert!(
            nodes.len() <= max_expected,
            "node count {} exceeds reasonable bound {} for 24 lines",
            nodes.len(),
            max_expected
        );

        // All nodes are TextRun role.
        for (_, node) in &nodes {
            assert_eq!(node.role(), accesskit::Role::TextRun);
        }

        // PASS text must be present in the first non-blank line's value.
        let first_value = nodes
            .iter()
            .filter_map(|(_, n)| n.value())
            .find(|v| v.contains("PASS"));
        assert!(first_value.is_some(), "PASS text must appear in a11y nodes");

        // Unicode content must survive.
        let has_unicode = nodes
            .iter()
            .filter_map(|(_, n)| n.value())
            .any(|v| v.contains("日本"));
        assert!(has_unicode, "unicode text must appear in a11y nodes");
    }
}
#[cfg(test)]
mod cursor_geometry_tests {
    use super::*;

    #[test]
    fn cursor_and_glyphs_share_boundaries_after_fractional_scroll() {
        let line_height = px(18.2);
        let cell_width = px(8.4);
        let dimensions = TerminalBounds::new(
            line_height,
            cell_width,
            Bounds {
                origin: point(px(10.0), px(10.0)),
                size: size(px(84.0), px(182.0)),
            },
        );
        let scale_factor = 1.25;
        let scroll_top = px(0.4);
        let snap_origin = |value: Pixels| {
            Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
        };
        let paint_origin = point(
            snap_origin(dimensions.bounds.origin.x),
            snap_origin(dimensions.bounds.origin.y - scroll_top),
        );
        let glyph = snapped_cell_point(paint_origin, 2, 3, &dimensions, scale_factor);
        let cursor = TerminalElement::cursor_position(
            DisplayCursor::from(Point::new(2, 3), 0),
            dimensions,
            paint_origin,
            scale_factor,
        )
        .unwrap();
        let cursor = paint_origin + cursor;

        assert_eq!(
            cursor, glyph,
            "cursor and glyph must share a device-pixel boundary after scrolling"
        );
    }
}
