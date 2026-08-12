use crate::MuxDomain;
use anyhow::{Context as _, Result};
use mux_protocol::{
    Cell, CommandRange, FetchGridUpdateResponse, FullGridSnapshot, MAX_FRAME_PAYLOAD,
    MAX_GRID_CELLS, checked_grid_cell_count, fetch_grid_update_response::Update as GridUpdateKind,
};

const MAX_COMMAND_CAPTURE_ATTEMPTS: usize = 3;
const MAX_COMMAND_CAPTURE_CELLS: usize = MAX_GRID_CELLS;
const MAX_COMMAND_CAPTURE_BYTES: usize = MAX_FRAME_PAYLOAD;

/// Selects a recorded command by its stable id or by an offset from the newest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSelector {
    Id(u64),
    Recent(u32),
}

/// The safely addressable output span for one OSC 133 command record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSpan {
    Located { start: i64, end: Option<i64> },
    Unaddressable,
    Incomplete,
}

/// Rendering options for one bounded command-output capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandCaptureOptions {
    pub join_wrapped: bool,
    pub preserve_ansi: bool,
}

pub fn select_command(
    commands: &[CommandRange],
    selector: CommandSelector,
) -> Result<&CommandRange> {
    anyhow::ensure!(
        !commands.is_empty(),
        "this pane has no recorded shell commands: the shell does not emit OSC 133 markers, or nothing has run since it started"
    );
    match selector {
        CommandSelector::Id(id) => commands
            .iter()
            .find(|command| command.id == id)
            .with_context(|| format!("no recorded command with id {id}")),
        CommandSelector::Recent(offset) => {
            let offset = usize::try_from(offset).context("command offset exceeds client limits")?;
            let index = commands
                .len()
                .checked_sub(offset.saturating_add(1))
                .with_context(|| {
                    format!(
                        "only {} recorded command(s), cannot go back {offset} from the newest",
                        commands.len()
                    )
                })?;
            commands
                .get(index)
                .context("recorded command index out of range")
        }
    }
}

/// Derive a command's output range without guessing invalidated marker rows.
pub fn command_output_span(command: &CommandRange) -> CommandSpan {
    let starts = [&command.output_start, &command.command, &command.prompt];
    let Some(start) = starts
        .iter()
        .find_map(|marker| marker.as_ref().and_then(|marker| marker.line))
    else {
        return if starts.iter().any(|marker| marker.is_some()) {
            CommandSpan::Unaddressable
        } else {
            CommandSpan::Incomplete
        };
    };

    let end = match &command.command_end {
        Some(marker) => match marker.line {
            Some(line) if marker.column == 0 => Some(line.saturating_sub(1)),
            Some(line) => Some(line),
            None => return CommandSpan::Unaddressable,
        },
        None => None,
    };

    CommandSpan::Located { start, end }
}

/// Capture exactly one recorded command's output from one stable server checkpoint.
///
/// The grid, history pages, and command metadata must agree on generation and
/// history version. A final no-change grid fetch closes the race before bytes
/// are returned. Checkpoint races retry without exposing a partial capture.
pub async fn capture_command_output(
    domain: &MuxDomain,
    pane_id: &str,
    selector: CommandSelector,
    options: CommandCaptureOptions,
) -> Result<String> {
    for _ in 0..MAX_COMMAND_CAPTURE_ATTEMPTS {
        let grid = domain
            .fetch_grid_update(pane_id, 0)
            .await
            .context("failed to fetch command grid checkpoint")?;
        let Some(GridUpdateKind::FullSnapshot(snapshot)) = grid.update.as_ref() else {
            anyhow::bail!("command capture expected a full grid snapshot");
        };
        validate_snapshot(snapshot)?;

        let listed = domain
            .list_commands(pane_id, 0)
            .await
            .context("failed to list shell commands")?;
        if listed.generation != grid.to_generation
            || listed.history_version != snapshot.history_version
        {
            continue;
        }

        let command = select_command(&listed.commands, selector)?;
        let command_id = command.id;
        let range = command_capture_range(command, snapshot)?;
        let expected_rows = range.row_count()?;
        let columns =
            usize::try_from(snapshot.cols).context("grid columns exceed client limits")?;
        let expected_cells = expected_rows
            .checked_mul(columns)
            .context("command capture cell count overflow")?;
        anyhow::ensure!(
            expected_cells <= MAX_COMMAND_CAPTURE_CELLS,
            "command {command_id} output has {expected_cells} cells, exceeding capture limit {MAX_COMMAND_CAPTURE_CELLS}"
        );

        let mut rows = Vec::with_capacity(expected_rows);
        if let Some((from, count)) = range.history {
            let mut next = from;
            let mut remaining = count;
            let page_rows = history_page_rows(snapshot.cols)?;
            let mut checkpoint_changed = false;
            while remaining > 0 {
                let page_count = remaining.min(page_rows);
                let page = domain
                    .fetch_scrollback(pane_id, next, 1, page_count)
                    .await
                    .context("failed to fetch command scrollback")?;
                if !scrollback_matches_snapshot(
                    &page,
                    snapshot.history_version,
                    snapshot.history_size,
                    snapshot.cols,
                    next,
                    page_count,
                ) {
                    checkpoint_changed = true;
                    break;
                }
                rows.extend(page.lines.into_iter().map(|row| row.cells));
                next = next
                    .checked_add(page_count)
                    .context("command scrollback row range overflow")?;
                remaining -= page_count;
            }
            if checkpoint_changed || remaining != 0 {
                continue;
            }
        }
        if let Some((first, last)) = range.visible {
            rows.extend(visible_rows(snapshot, first, last)?);
        }
        anyhow::ensure!(
            rows.len() == expected_rows,
            "command capture assembled {} rows, expected {expected_rows}",
            rows.len()
        );

        let checkpoint = domain
            .fetch_grid_update(pane_id, grid.to_generation)
            .await
            .context("failed to validate command capture checkpoint")?;
        if !grid_checkpoint_is_stable(grid.to_generation, &checkpoint) {
            continue;
        }

        return render_capture(&rows, options);
    }

    anyhow::bail!("terminal command checkpoint changed while its output was being captured")
}

fn command_capture_range(
    command: &CommandRange,
    snapshot: &FullGridSnapshot,
) -> Result<CaptureRange> {
    let (start, end) = match command_output_span(command) {
        CommandSpan::Located { start, end } => (start, end),
        CommandSpan::Unaddressable => anyhow::bail!(
            "command {} was recorded but its output is no longer addressable",
            command.id
        ),
        CommandSpan::Incomplete => {
            anyhow::bail!("command {} has no usable OSC 133 start marker", command.id)
        }
    };

    let history_size = i64::from(snapshot.history_size);
    let rows = i64::from(snapshot.rows);
    anyhow::ensure!(rows > 0, "command capture grid has no visible rows");
    let oldest = -history_size;
    let newest = rows - 1;
    let end = end.unwrap_or(newest);
    anyhow::ensure!(
        (oldest..=newest).contains(&start),
        "command {} start row {start} lies outside {oldest}..={newest}",
        command.id
    );
    anyhow::ensure!(
        (oldest..=newest).contains(&end),
        "command {} end row {end} lies outside {oldest}..={newest}",
        command.id
    );
    anyhow::ensure!(
        end >= start,
        "command {} has inverted output rows {start}..={end}",
        command.id
    );

    let history = if start < 0 {
        let last = end.min(-1);
        let from = u32::try_from(history_size + start)
            .context("command history start exceeds protocol limits")?;
        let count = u32::try_from(last - start + 1)
            .context("command history row count exceeds protocol limits")?;
        Some((from, count))
    } else {
        None
    };
    let visible = if end >= 0 {
        Some((
            u32::try_from(start.max(0)).context("command visible start exceeds protocol limits")?,
            u32::try_from(end).context("command visible end exceeds protocol limits")?,
        ))
    } else {
        None
    };

    Ok(CaptureRange { history, visible })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CaptureRange {
    history: Option<(u32, u32)>,
    visible: Option<(u32, u32)>,
}

impl CaptureRange {
    fn row_count(&self) -> Result<usize> {
        let history_rows = self
            .history
            .map(|(_, count)| usize::try_from(count))
            .transpose()
            .context("command history row count exceeds client limits")?
            .unwrap_or(0);
        let visible_rows = self
            .visible
            .map(|(first, last)| {
                last.checked_sub(first)
                    .and_then(|distance| distance.checked_add(1))
                    .context("command visible row range overflow")
                    .and_then(|count| {
                        usize::try_from(count)
                            .context("command visible row count exceeds client limits")
                    })
            })
            .transpose()?
            .unwrap_or(0);
        history_rows
            .checked_add(visible_rows)
            .context("command capture row count overflow")
    }
}

fn validate_snapshot(snapshot: &FullGridSnapshot) -> Result<()> {
    let columns = usize::try_from(snapshot.cols).context("grid columns exceed client limits")?;
    let rows = usize::try_from(snapshot.rows).context("grid rows exceed client limits")?;
    let expected = checked_grid_cell_count(columns, rows)
        .map_err(|error| anyhow::anyhow!("invalid grid snapshot: {error}"))?;
    anyhow::ensure!(
        snapshot.cells.len() == expected,
        "invalid grid snapshot: expected {expected} cells, got {}",
        snapshot.cells.len()
    );
    Ok(())
}

fn history_page_rows(columns: u32) -> Result<u32> {
    let columns = usize::try_from(columns).context("scrollback columns exceed client limits")?;
    let rows = MAX_GRID_CELLS
        .checked_div(columns.max(1))
        .filter(|rows| *rows > 0)
        .context("scrollback columns exceed protocol cell limit")?;
    Ok(u32::try_from(rows).unwrap_or(u32::MAX))
}

fn scrollback_matches_snapshot(
    scrollback: &mux_protocol::FetchScrollbackResponse,
    history_version: u64,
    history_size: u32,
    columns: u32,
    from: u32,
    count: u32,
) -> bool {
    scrollback.scrollback_version == history_version
        && scrollback.total_lines == history_size
        && scrollback.lines.len() == count as usize
        && scrollback.lines.iter().enumerate().all(|(index, row)| {
            u32::try_from(index)
                .ok()
                .and_then(|index| from.checked_add(index))
                .is_some_and(|expected| row.row == expected)
                && row.cells.len() == columns as usize
        })
}

fn visible_rows(snapshot: &FullGridSnapshot, first: u32, last: u32) -> Result<Vec<Vec<Cell>>> {
    anyhow::ensure!(
        first <= last && last < snapshot.rows,
        "visible command range {first}..={last} exceeds {} rows",
        snapshot.rows
    );
    let columns = usize::try_from(snapshot.cols).context("grid columns exceed client limits")?;
    let capacity = last
        .checked_sub(first)
        .and_then(|distance| distance.checked_add(1))
        .and_then(|count| usize::try_from(count).ok())
        .context("visible command row count exceeds client limits")?;
    let mut rows = Vec::with_capacity(capacity);
    for row in first..=last {
        let offset = usize::try_from(row)
            .ok()
            .and_then(|row| row.checked_mul(columns))
            .context("visible command row offset overflow")?;
        let end = offset
            .checked_add(columns)
            .context("visible command row end overflow")?;
        let cells = snapshot
            .cells
            .get(offset..end)
            .context("visible command grid is missing cells")?;
        rows.push(cells.to_vec());
    }
    Ok(rows)
}

fn grid_checkpoint_is_stable(generation: u64, response: &FetchGridUpdateResponse) -> bool {
    response.from_generation == generation
        && response.to_generation == generation
        && response.update.is_none()
}

fn render_capture(rows: &[Vec<Cell>], options: CommandCaptureOptions) -> Result<String> {
    let mut output = String::new();
    let mut index = 0usize;
    while index < rows.len() {
        let mut line = Vec::new();
        while let Some(row) = rows.get(index) {
            line.extend(row.iter());
            index += 1;
            if !options.join_wrapped || !row_wraps(row) {
                break;
            }
        }
        let rendered = render_cells(line.into_iter(), options.preserve_ansi);
        let next_length = output
            .len()
            .checked_add(rendered.len())
            .and_then(|length| length.checked_add(1))
            .context("command output byte count overflow")?;
        anyhow::ensure!(
            next_length <= MAX_COMMAND_CAPTURE_BYTES,
            "command output exceeds capture byte limit {MAX_COMMAND_CAPTURE_BYTES}"
        );
        output.push_str(&rendered);
        output.push('\n');
    }
    Ok(output)
}

fn row_wraps(row: &[Cell]) -> bool {
    row.last()
        .is_some_and(|cell| cell.style.as_ref().is_some_and(|style| style.wrapline))
}

fn render_cells<'a>(cells: impl IntoIterator<Item = &'a Cell>, preserve_ansi: bool) -> String {
    if !preserve_ansi {
        return cells.into_iter().map(cell_text).collect();
    }

    let mut output = String::new();
    let mut current = None;
    let mut hyperlink: Option<(String, String)> = None;
    for cell in cells {
        if is_cell_spacer(cell) {
            continue;
        }
        let next_hyperlink = cell
            .hyperlink
            .as_ref()
            .map(|link| (link.id.clone(), link.uri.clone()));
        if hyperlink != next_hyperlink {
            if hyperlink.is_some() {
                output.push_str("\x1b]8;;\x1b\\");
            }
            if let Some((id, uri)) = &next_hyperlink {
                let parameters = if id.is_empty() {
                    String::new()
                } else {
                    format!("id={id}")
                };
                output.push_str(&format!("\x1b]8;{parameters};{uri}\x1b\\"));
            }
            hyperlink = next_hyperlink;
        }
        let next = SgrState::from_cell(cell);
        if current.as_ref() != Some(&next) {
            output.push_str(&next.to_sgr());
            current = Some(next);
        }
        output.push_str(&cell_text(cell));
    }
    if hyperlink.is_some() {
        output.push_str("\x1b]8;;\x1b\\");
    }
    if current.is_some() {
        output.push_str("\x1b[0m");
    }
    output
}

fn is_cell_spacer(cell: &Cell) -> bool {
    cell.style
        .as_ref()
        .is_some_and(|style| style.wide_char_spacer || style.leading_wide_char_spacer)
}

fn cell_text(cell: &Cell) -> String {
    if is_cell_spacer(cell) {
        return String::new();
    }
    let mut text = cell.char.clone();
    text.push_str(&cell.zerowidth);
    text
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SgrState {
    bold: bool,
    italic: bool,
    underline: bool,
    underline_style: i32,
    underline_color: Option<u32>,
    strikethrough: bool,
    dim: bool,
    reverse: bool,
    hidden: bool,
    foreground: u32,
    background: u32,
}

impl SgrState {
    fn from_cell(cell: &Cell) -> Self {
        let style = cell.style.as_ref().cloned().unwrap_or_default();
        Self {
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            underline_style: style.underline_style,
            underline_color: style.underline_color,
            strikethrough: style.strikethrough,
            dim: style.dim,
            reverse: style.reverse,
            hidden: style.hidden,
            foreground: cell.foreground,
            background: cell.background,
        }
    }

    fn to_sgr(self) -> String {
        let mut parts = vec!["0".to_string()];
        if self.bold {
            parts.push("1".into());
        }
        if self.dim {
            parts.push("2".into());
        }
        if self.italic {
            parts.push("3".into());
        }
        match self.underline_style {
            2 => parts.push("4:1".into()),
            3 => parts.push("4:2".into()),
            4 => parts.push("4:3".into()),
            5 => parts.push("4:4".into()),
            6 => parts.push("4:5".into()),
            0 if self.underline => parts.push("4".into()),
            _ => {}
        }
        if self.reverse {
            parts.push("7".into());
        }
        if self.hidden {
            parts.push("8".into());
        }
        if self.strikethrough {
            parts.push("9".into());
        }
        if self.foreground != 0 {
            parts.push(color_sgr(true, self.foreground));
        }
        if self.background != 0 {
            parts.push(color_sgr(false, self.background));
        }
        if let Some(color) = self.underline_color {
            parts.push(color_sgr_code(58, color));
        }
        format!("\x1b[{}m", parts.join(";"))
    }
}

fn color_sgr_code(code: u8, color: u32) -> String {
    let red = (color >> 16) & 0xff;
    let green = (color >> 8) & 0xff;
    let blue = color & 0xff;
    format!("{code};2;{red};{green};{blue}")
}

fn color_sgr(foreground: bool, color: u32) -> String {
    let red = ((color >> 16) & 0xff) as i32;
    let green = ((color >> 8) & 0xff) as i32;
    let blue = (color & 0xff) as i32;
    const PALETTE: [(i32, i32, i32, u8); 17] = [
        (0, 0, 0, 0),
        (205, 0, 0, 1),
        (204, 85, 85, 1),
        (0, 205, 0, 2),
        (205, 205, 0, 3),
        (0, 0, 238, 4),
        (205, 0, 205, 5),
        (0, 205, 205, 6),
        (229, 229, 229, 7),
        (127, 127, 127, 8),
        (255, 0, 0, 9),
        (0, 255, 0, 10),
        (255, 255, 0, 11),
        (92, 92, 255, 12),
        (255, 0, 255, 13),
        (0, 255, 255, 14),
        (255, 255, 255, 15),
    ];
    let mut best = None;
    let mut best_distance = i32::MAX;
    for (palette_red, palette_green, palette_blue, index) in PALETTE {
        let distance = (red - palette_red).pow(2)
            + (green - palette_green).pow(2)
            + (blue - palette_blue).pow(2);
        if distance < best_distance {
            best_distance = distance;
            best = Some(index);
        }
    }
    if let Some(index) = best.filter(|_| best_distance <= 25_000) {
        if foreground {
            if index < 8 {
                format!("{}", 30 + index)
            } else {
                format!("{}", 90 + (index - 8))
            }
        } else if index < 8 {
            format!("{}", 40 + index)
        } else {
            format!("{}", 100 + (index - 8))
        }
    } else if foreground {
        format!("38;2;{red};{green};{blue}")
    } else {
        format!("48;2;{red};{green};{blue}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::{CellStyle, CommandMarker};

    fn marker(line: Option<i64>, column: u32) -> Option<CommandMarker> {
        Some(CommandMarker { line, column })
    }

    fn command(id: u64) -> CommandRange {
        CommandRange {
            id,
            ..Default::default()
        }
    }

    #[test]
    fn output_span_uses_safe_marker_fallback_and_column_zero_end() {
        let mut range = command(1);
        range.prompt = marker(Some(-40), 0);
        range.command = marker(Some(-40), 8);
        range.output_start = marker(Some(-39), 0);
        range.command_end = marker(Some(-30), 0);
        assert_eq!(
            command_output_span(&range),
            CommandSpan::Located {
                start: -39,
                end: Some(-31),
            }
        );

        range.output_start = None;
        assert_eq!(
            command_output_span(&range),
            CommandSpan::Located {
                start: -40,
                end: Some(-31),
            }
        );
    }

    #[test]
    fn output_span_never_guesses_invalidated_or_missing_rows() {
        let mut invalidated = command(2);
        invalidated.output_start = marker(None, 0);
        invalidated.command_end = marker(None, 0);
        assert_eq!(
            command_output_span(&invalidated),
            CommandSpan::Unaddressable
        );

        let mut incomplete = command(3);
        incomplete.command_end = marker(Some(-1), 0);
        assert_eq!(command_output_span(&incomplete), CommandSpan::Incomplete);
    }

    #[test]
    fn selection_uses_stable_ids_and_newest_offsets() {
        let commands = [command(10), command(20), command(30)];
        assert_eq!(
            select_command(&commands, CommandSelector::Recent(0))
                .expect("newest command")
                .id,
            30
        );
        assert_eq!(
            select_command(&commands, CommandSelector::Id(20))
                .expect("stable command id")
                .id,
            20
        );
        assert!(select_command(&commands, CommandSelector::Recent(3)).is_err());
    }

    #[test]
    fn command_range_rejects_rows_outside_the_snapshot() {
        let snapshot = FullGridSnapshot {
            cols: 2,
            rows: 2,
            history_size: 3,
            cells: vec![Cell::default(); 4],
            ..Default::default()
        };
        let mut valid = command(4);
        valid.output_start = marker(Some(-2), 0);
        valid.command_end = marker(Some(1), 1);
        assert_eq!(
            command_capture_range(&valid, &snapshot).expect("valid range"),
            CaptureRange {
                history: Some((1, 2)),
                visible: Some((0, 1)),
            }
        );

        valid.output_start = marker(Some(-4), 0);
        assert!(command_capture_range(&valid, &snapshot).is_err());
    }

    #[test]
    fn rendering_preserves_combining_marks_and_skips_wide_spacers() {
        let mut combined = Cell {
            char: "e".into(),
            zerowidth: "\u{301}\u{323}".into(),
            ..Default::default()
        };
        combined.style = Some(CellStyle::default());
        let mut spacer = Cell {
            char: " ".into(),
            style: Some(CellStyle::default()),
            ..Default::default()
        };
        spacer.style.as_mut().expect("cell style").wide_char_spacer = true;
        let rendered = render_capture(&[vec![combined, spacer]], CommandCaptureOptions::default())
            .expect("bounded render");
        assert_eq!(rendered, "e\u{301}\u{323}\n");
    }
}
