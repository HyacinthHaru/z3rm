// capture-pane 实现: 从 server 拉取 grid 并转为文本
// 来源: spec §3.10 — capture-pane -p 输出 pane 可见内容

use anyhow::{Context, Result};
use mux_protocol::{MAX_GRID_CELLS, checked_grid_cell_count};
use mux::MuxDomain;
use mux::command_history::{CommandSpan, command_output_span};
use mux_protocol::proto::{
    Cell, FetchGridUpdateResponse, FullGridSnapshot, fetch_grid_update_response::Update as GridUpdateKind,
CommandRange,
};

/// `-S` / `-E` 接受的行号，遵循 tmux 的行号模型。
///
/// 可见区第一行是 `0`，往下递增；负数进入历史，`-1` 是紧贴可见区上方的
/// 那一行历史。字面量 `-` 表示这一侧的极端边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureLine {
    /// 字面量 `-`：`-S` 取历史最开头，`-E` 取可见区最后一行。
    Edge,
    Line(i32),
}

/// capture-pane 的取值范围与渲染选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureOptions {
    /// `-S`，缺省为可见区第一行。
    pub start: Option<CaptureLine>,
    /// `-E`，闭区间的结束行，缺省为可见区最后一行。
    pub end: Option<CaptureLine>,
    /// `-J`：把被终端折行的行重新拼回一行。
    pub join_wrapped: bool,
    /// `-e`：保留 ANSI 颜色/样式码。
    pub preserve_ansi: bool,
}

/// `capture-pane --command` / `--last-command` 选中的那条命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSelector {
    /// `list-commands` 第一列打印的那个 id。
    Id(u64),
    /// 相对最新一条命令往回数：`0` 是最新的一条，`1` 是上一条。
    Recent(u32),
}

/// 从 `list-commands` 的结果里挑出被选中的那条命令。
pub fn select_command(
    commands: &[CommandRange],
    selector: CommandSelector,
) -> Result<&CommandRange> {
    anyhow::ensure!(
        !commands.is_empty(),
        "this pane has no recorded shell commands: the shell does not emit OSC 133 \
         markers, or nothing has run since it started"
    );
    match selector {
        CommandSelector::Id(id) => commands
            .iter()
            .find(|command| command.id == id)
            .with_context(|| format!("no recorded command with id {id}")),
        CommandSelector::Recent(offset) => {
            let index = commands
                .len()
                .checked_sub(offset as usize + 1)
                .with_context(|| {
                    format!(
                        "only {} recorded command(s), cannot go back {offset} from the newest",
                        commands.len(),
                    )
                })?;
            commands
                .get(index)
                .context("recorded command index out of range")
        }
    }
}

/// 把一条命令的输出区间变成 `-S` / `-E`，行号不可用时给出说清原因的报错。
pub fn command_capture_lines(command: &CommandRange) -> Result<(CaptureLine, Option<CaptureLine>)> {
    match command_output_span(command) {
        CommandSpan::Located { start, end } => Ok((
            CaptureLine::Line(capture_line_number(start)?),
            end.map(capture_line_number)
                .transpose()?
                .map(CaptureLine::Line),
        )),
        CommandSpan::Unaddressable => anyhow::bail!(
            "command {} was recorded but its rows can no longer be addressed: they were \
             evicted from scrollback, or the line numbering was retired by a resize, a clear, \
             or scrollback reaching capacity. 'z3rm list-commands' still reports its exit status.",
            command.id,
        ),
        CommandSpan::Unmarked => anyhow::bail!(
            "command {} carries no marker saying where its output starts; this shell reports \
             only command ends",
            command.id,
        ),
    }
}

fn capture_line_number(line: i64) -> Result<i32> {
    i32::try_from(line).with_context(|| format!("line number {line} is out of range"))
}

/// 捕获 pane 的内容，转换为文本。
pub async fn capture_pane(
    domain: &MuxDomain,
    pane_id: &str,
    options: CaptureOptions,
) -> Result<String> {
    const MAX_CAPTURE_ATTEMPTS: usize = 3;

    for _ in 0..MAX_CAPTURE_ATTEMPTS {
        let grid = domain
            .fetch_grid_update(pane_id, 0)
            .await
            .context("failed to fetch grid update")?;
        let Some(GridUpdateKind::FullSnapshot(snapshot)) = grid.update.as_ref() else {
            anyhow::bail!("capture-pane expected a full grid snapshot");
        };
        validate_snapshot(snapshot)?;

        let span = capture_span(
            snapshot.history_size,
            snapshot.rows,
            options.start,
            options.end,
        );

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        if let Some((from, count)) = span.history {
            let page_rows = history_page_rows(snapshot.cols)?;
            let mut next = from;
            let mut remaining = count;
            while remaining > 0 {
                let page_count = remaining.min(page_rows);
                let scrollback = domain
                    .fetch_scrollback(pane_id, next, 1, page_count)
                    .await
                    .context("failed to fetch scrollback")?;
                if !scrollback_matches_snapshot(
                    &scrollback,
                    snapshot.history_version,
                    snapshot.history_size,
                    snapshot.cols,
                    next,
                    page_count,
                ) {
                    rows.clear();
                    break;
                }
                rows.extend(scrollback.lines.into_iter().map(|row| row.cells));
                next = next
                    .checked_add(page_count)
                    .context("scrollback row range overflow")?;
                remaining -= page_count;
            }
            if remaining != 0 {
                continue;
            }
        }

        let checkpoint = domain
            .fetch_grid_update(pane_id, grid.to_generation)
            .await
            .context("failed to validate capture grid checkpoint")?;
        if !grid_checkpoint_is_stable(grid.to_generation, &checkpoint) {
            continue;
        }
        let checkpoint = domain
            .fetch_grid_update(pane_id, grid.to_generation)
            .await
            .context("failed to validate capture grid checkpoint")?;
        if !grid_checkpoint_is_stable(grid.to_generation, &checkpoint) {
            continue;
        }
        if let Some((first, last)) = span.visible {
            rows.extend(visible_rows(snapshot, first, last)?);
        }

        return Ok(render_capture(
            &rows,
            options.join_wrapped,
            options.preserve_ansi,
        ));
    }

    anyhow::bail!("terminal history changed while capture-pane was reading it")
}
fn validate_snapshot(snapshot: &FullGridSnapshot) -> Result<()> {
    let cols = usize::try_from(snapshot.cols).context("grid columns exceed client limits")?;
    let rows = usize::try_from(snapshot.rows).context("grid rows exceed client limits")?;
    let expected = checked_grid_cell_count(cols, rows)
        .map_err(|error| anyhow::anyhow!("invalid grid snapshot: {error}"))?;
    if snapshot.cells.len() != expected {
        anyhow::bail!(
            "invalid grid snapshot: expected {expected} cells, got {}",
            snapshot.cells.len()
        );
    }
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

/// 一次 capture 要读取的行，拆成"历史"和"可见区"两段。
#[derive(Debug, Default, PartialEq, Eq)]
struct CaptureSpan {
    /// `(from_line, count)`，直接喂给 `fetch_scrollback`。
    history: Option<(u32, u32)>,
    /// 可见区的闭区间 `[first_row, last_row]`。
    visible: Option<(u32, u32)>,
}

/// 把 tmux 行号区间夹到 pane 实际拥有的范围内，再拆成历史段和可见段。
fn capture_span(
    history_size: u32,
    rows: u32,
    start: Option<CaptureLine>,
    end: Option<CaptureLine>,
) -> CaptureSpan {
    let oldest = -clamp_to_i32(history_size);
    let newest = clamp_to_i32(rows) - 1;

    let start = match start {
        Some(CaptureLine::Edge) => oldest,
        Some(CaptureLine::Line(line)) => line,
        None => 0,
    }
    .max(oldest);
    let end = match end {
        Some(CaptureLine::Edge) | None => newest,
        Some(CaptureLine::Line(line)) => line,
    }
    .min(newest);

    if end < start {
        return CaptureSpan::default();
    }

    let history = (start < 0).then(|| {
        let last_history_line = end.min(-1);
        let from = (history_size as i64 + start as i64).max(0) as u32;
        let count = (last_history_line - start + 1) as u32;
        (from, count)
    });
    let visible = (end >= 0).then(|| (start.max(0) as u32, end as u32));

    CaptureSpan { history, visible }
}

fn clamp_to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn visible_rows(snapshot: &FullGridSnapshot, first: u32, last: u32) -> Result<Vec<Vec<Cell>>> {
    if first > last || last >= snapshot.rows {
        anyhow::bail!(
            "visible capture range {first}..={last} exceeds {} rows",
            snapshot.rows
        );
    }
    let cols = usize::try_from(snapshot.cols).context("grid columns exceed client limits")?;
    let mut rows = Vec::with_capacity((last - first + 1) as usize);
    for row in first..=last {
        let offset = (row as usize)
            .checked_mul(cols)
            .context("visible grid row offset overflow")?;
        let end = offset
            .checked_add(cols)
            .context("visible grid row end overflow")?;
        let cells = snapshot
            .cells
            .get(offset..end)
            .context("visible grid snapshot is missing cells")?;
        rows.push(cells.to_vec());
    }
    Ok(rows)
}

fn scrollback_matches_snapshot(
    scrollback: &mux_protocol::proto::FetchScrollbackResponse,
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
            from.checked_add(index as u32)
                .is_some_and(|expected| row.row == expected)
                && row.cells.len() == columns as usize
        })
}


fn grid_checkpoint_is_stable(
    generation: u64,
    response: &FetchGridUpdateResponse,
) -> bool {
    response.from_generation == generation
        && response.to_generation == generation
        && response.update.is_none()
}

fn render_capture(rows: &[Vec<Cell>], join_wrapped: bool, preserve_ansi: bool) -> String {
    let mut output = String::new();
    let mut index = 0usize;
    while index < rows.len() {
        let mut line: Vec<&Cell> = Vec::new();
        while let Some(row) = rows.get(index) {
            line.extend(row.iter());
            index += 1;
            if !join_wrapped || !row_wraps(row) {
                break;
            }
        }
        output.push_str(&render_cells(line.into_iter(), preserve_ansi));
        output.push('\n');
    }
    output
}

/// alacritty 在折行时给该行最后一个 cell 打上 `WRAPLINE`，这是"下一行是本行
/// 续行"的权威信号 —— 比"行尾是否填满"这种启发式可靠。
fn row_wraps(row: &[Cell]) -> bool {
    row.last()
        .is_some_and(|cell| cell.style.as_ref().is_some_and(|style| style.wrapline))
}

pub(super) fn render_cells<'a>(
    cells: impl IntoIterator<Item = &'a Cell>,
    preserve_ansi: bool,
) -> String {
    if !preserve_ansi {
        return cells.into_iter().map(cell_text).collect();
    }

    let mut output = String::new();
    let mut current: Option<SgrState> = None;
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
                let params = if id.is_empty() {
                    String::new()
                } else {
                    format!("id={id}")
                };
                output.push_str(&format!("\x1b]8;{params};{uri}\x1b\\"));
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
    cell.style.as_ref().is_some_and(|style| {
        style.wide_char_spacer || style.leading_wide_char_spacer
    })
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
    let r = (color >> 16) & 0xff;
    let g = (color >> 8) & 0xff;
    let b = color & 0xff;
    format!("{code};2;{r};{g};{b}")
}

/// Prefer classic 16-color SGR when the RGB is near the XTerm palette so
/// capture-pane -e stays tmux-compatible for ordinary ANSI sequences. Fall
/// back to truecolor for arbitrary RGB.
fn color_sgr(foreground: bool, color: u32) -> String {
    let r = ((color >> 16) & 0xff) as i32;
    let g = ((color >> 8) & 0xff) as i32;
    let b = (color & 0xff) as i32;
    // XTerm default 16-color palette (approx).
    const PALETTE: [(i32, i32, i32, u8); 17] = [
        (0, 0, 0, 0),
        (205, 0, 0, 1),
        (204, 85, 85, 1), // common theme bright-dark red
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
    let mut best_dist = i32::MAX;
    for (pr, pg, pb, index) in PALETTE {
        let dist = (r - pr).pow(2) + (g - pg).pow(2) + (b - pb).pow(2);
        if dist < best_dist {
            best_dist = dist;
            best = Some(index);
        }
    }
    // Threshold ~50 units per channel squared sum ~ 3*50^2 = 7500.
    if let Some(index) = best.filter(|_| best_dist <= 25000) {
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
        format!("38;2;{r};{g};{b}")
    } else {
        format!("48;2;{r};{g};{b}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::proto::{CellStyle, CommandMarker};

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
    fn output_span_runs_from_the_output_marker_to_the_row_before_the_end_marker() {
        let mut range = command(1);
        range.prompt = marker(Some(-40), 0);
        range.command = marker(Some(-40), 8);
        range.output_start = marker(Some(-39), 0);
        // D 在第 0 列 = shell 已经换到新行才报告结束, 那一行不属于输出。
        range.command_end = marker(Some(-30), 0);
        assert_eq!(
            command_output_span(&range),
            CommandSpan::Located {
                start: -39,
                end: Some(-31),
            }
        );

        // D 落在行中间说明它还在输出的最后一行上, 那一行要留下。
        range.command_end = marker(Some(-30), 12);
        assert_eq!(
            command_output_span(&range),
            CommandSpan::Located {
                start: -39,
                end: Some(-30),
            }
        );
    }

    #[test]
    fn a_running_command_has_no_end_bound() {
        let mut range = command(2);
        range.prompt = marker(Some(-3), 0);
        range.command = marker(Some(-3), 5);
        range.output_start = marker(Some(-2), 0);
        assert_eq!(
            command_output_span(&range),
            CommandSpan::Located {
                start: -2,
                end: None,
            }
        );
        // -E 缺省就是可见区末尾, 正是"跑到现在为止的全部输出"。
        let (start, end) = command_capture_lines(&range).expect("a located span");
        assert_eq!(start, CaptureLine::Line(-2));
        assert_eq!(end, None);
    }

    /// 有的 shell 只发 A 和 D。缺 C 时退到更靠前的 marker: 多带上命令行甚至
    /// 提示符, 好过漏掉真正的输出。
    #[test]
    fn a_missing_output_marker_falls_back_to_the_earlier_ones() {
        let mut range = command(3);
        range.command = marker(Some(-9), 6);
        range.command_end = marker(Some(-4), 0);
        assert_eq!(
            command_output_span(&range),
            CommandSpan::Located {
                start: -9,
                end: Some(-5),
            }
        );

        let mut only_prompt_and_end = command(4);
        only_prompt_and_end.prompt = marker(Some(-9), 0);
        only_prompt_and_end.command_end = marker(Some(-4), 0);
        assert_eq!(
            command_output_span(&only_prompt_and_end),
            CommandSpan::Located {
                start: -9,
                end: Some(-5),
            }
        );
    }

    /// 行号不可用时绝不猜 —— 错的行号比查不到糟得多。
    #[test]
    fn evicted_rows_are_reported_rather_than_guessed() {
        let mut range = command(5);
        range.prompt = marker(None, 0);
        range.command = marker(None, 4);
        range.output_start = marker(None, 0);
        range.command_end = marker(None, 0);
        range.exit_code = Some(3);
        assert_eq!(command_output_span(&range), CommandSpan::Unaddressable);

        let error = command_capture_lines(&range).expect_err("an unaddressable span must fail");
        let message = format!("{error:#}");
        assert!(message.contains("scrollback"), "{message}");
        assert!(
            message.contains("exit status"),
            "the error must point at the exit status that still works: {message}"
        );

        // 起点找得到而终点找不到, 这一对就配不上了: 取到可见区末尾会把后面
        // 无关的输出一起带上。
        let mut half_located = command(6);
        half_located.output_start = marker(Some(-8), 0);
        half_located.command_end = marker(None, 0);
        assert_eq!(
            command_output_span(&half_located),
            CommandSpan::Unaddressable
        );
    }

    #[test]
    fn a_command_with_no_start_marker_at_all_is_distinguished_from_evicted_rows() {
        let mut range = command(7);
        range.command_end = marker(Some(-4), 0);
        range.exit_code = Some(0);
        assert_eq!(command_output_span(&range), CommandSpan::Unmarked);

        let error = command_capture_lines(&range).expect_err("an unmarked span must fail");
        assert!(
            format!("{error:#}").contains("only command ends"),
            "{error}"
        );
    }

    #[test]
    fn command_selection_addresses_ids_and_offsets_from_the_newest() {
        let commands: Vec<CommandRange> = [10u64, 20, 30].into_iter().map(command).collect();
        assert_eq!(
            select_command(&commands, CommandSelector::Recent(0))
                .expect("newest")
                .id,
            30
        );
        assert_eq!(
            select_command(&commands, CommandSelector::Recent(2))
                .expect("oldest")
                .id,
            10
        );
        assert_eq!(
            select_command(&commands, CommandSelector::Id(20))
                .expect("by id")
                .id,
            20
        );

        let error = select_command(&commands, CommandSelector::Recent(3))
            .expect_err("walking past the oldest command must fail");
        assert!(format!("{error:#}").contains("only 3"), "{error}");
        let error = select_command(&commands, CommandSelector::Id(11))
            .expect_err("an unknown id must fail");
        assert!(format!("{error:#}").contains("11"), "{error}");
        let error = select_command(&[], CommandSelector::Recent(0))
            .expect_err("a pane with no commands must fail");
        assert!(format!("{error:#}").contains("OSC 133"), "{error}");
    }

    fn cell(ch: &str, fg: u32, bold: bool) -> Cell {
        Cell {
            char: ch.into(),
            style: Some(CellStyle {
                bold,
                ..Default::default()
            }),
            foreground: fg,
            background: 0,
            ..Default::default()
        }
    }

    #[test]
    fn plain_capture_omits_sgr() {
        let cells = vec![cell("a", 0xff0000, true)];
        assert_eq!(render_cells(&cells, false), "a");
    }
    #[test]
    fn ansi_capture_emits_sgr_and_reset() {
        // Near-palette red (205,0,0) maps to classic \x1b[31m.
        let cells = vec![cell("x", 0xcd0000, true)];
        let text = render_cells(&cells, true);
        assert!(text.starts_with("\x1b[0;1;31m"), "{text:?}");
        assert!(text.ends_with("x\x1b[0m"), "{text:?}");
    }
    #[test]
    fn ansi_capture_coalesces_identical_runs() {
        let cells = vec![cell("a", 0xff0000, true), cell("b", 0xff0000, true)];
        let text = render_cells(&cells, true);
        assert_eq!(
            text.matches("\x1b[").count(),
            2,
            "one open SGR + one reset: {text:?}"
        );
        assert!(text.contains("ab"));
    }

    #[test]
    fn capture_preserves_combining_marks_and_skips_wide_spacers() {
        let mut combined = cell("e", 0, false);
        combined.zerowidth = "\u{301}\u{323}".to_string();
        let mut spacer = cell(" ", 0, false);
        spacer.style.as_mut().expect("cell style").wide_char_spacer = true;

        assert_eq!(
            render_cells(&[combined.clone(), spacer.clone()], false),
            "e\u{301}\u{323}"
        );
        let escaped = render_cells(&[combined, spacer], true);
        assert!(escaped.contains("e\u{301}\u{323}"), "{escaped:?}");
        assert!(!escaped.contains("e\u{301}\u{323} "), "{escaped:?}");
    }
    #[test]
    fn capture_with_scrollback_appends_visible_grid() {
        let rows = vec![vec![cell("h", 0, false)], vec![cell("v", 0, false)]];
        assert_eq!(render_capture(&rows, false, false), "h\nv\n");
    }

    #[test]
    fn visible_rows_slices_the_requested_window() {
        let snapshot = FullGridSnapshot {
            cols: 2,
            rows: 3,
            cells: vec![
                cell("a", 0, false),
                cell("b", 0, false),
                cell("c", 0, false),
                cell("d", 0, false),
                cell("e", 0, false),
                cell("f", 0, false),
            ],
            ..Default::default()
        };
        assert_eq!(
            render_capture(&visible_rows(&snapshot, 1, 2).unwrap(), false, false),
            "cd\nef\n"
        );
        assert_eq!(
            render_capture(&visible_rows(&snapshot, 0, 0).unwrap(), false, false),
            "ab\n"
        );
    }

    #[test]
    fn capture_requests_only_the_latest_scrollback_rows() {
        // `-S -2` 只拉紧贴可见区上方的两行历史，再接整个可见区。
        assert_eq!(
            capture_span(10_000, 24, Some(CaptureLine::Line(-2)), None),
            CaptureSpan {
                history: Some((9_998, 2)),
                visible: Some((0, 23)),
            }
        );
        // 请求超过实际历史时夹到历史起点，而不是发出越界请求。
        assert_eq!(
            capture_span(3, 24, Some(CaptureLine::Line(-10)), None),
            CaptureSpan {
                history: Some((0, 3)),
                visible: Some((0, 23)),
            }
        );
        assert_eq!(
            capture_span(0, 24, Some(CaptureLine::Line(-10)), None),
            CaptureSpan {
                history: None,
                visible: Some((0, 23)),
            }
        );
    }

    #[test]
    fn capture_span_honors_start_and_end_line_numbers() {
        // 默认：只有可见区。
        assert_eq!(
            capture_span(50, 24, None, None),
            CaptureSpan {
                history: None,
                visible: Some((0, 23)),
            }
        );
        // `-S -` / `-E -` 是两端的极值。
        assert_eq!(
            capture_span(50, 24, Some(CaptureLine::Edge), Some(CaptureLine::Edge)),
            CaptureSpan {
                history: Some((0, 50)),
                visible: Some((0, 23)),
            }
        );
        // 完全落在可见区内的闭区间。
        assert_eq!(
            capture_span(
                50,
                24,
                Some(CaptureLine::Line(2)),
                Some(CaptureLine::Line(4))
            ),
            CaptureSpan {
                history: None,
                visible: Some((2, 4)),
            }
        );
        // 完全落在历史里的闭区间，不碰可见区。
        assert_eq!(
            capture_span(
                50,
                24,
                Some(CaptureLine::Line(-5)),
                Some(CaptureLine::Line(-3))
            ),
            CaptureSpan {
                history: Some((45, 3)),
                visible: None,
            }
        );
        // `-E` 超过可见区末行时夹住。
        assert_eq!(
            capture_span(
                50,
                24,
                Some(CaptureLine::Line(0)),
                Some(CaptureLine::Line(999))
            ),
            CaptureSpan {
                history: None,
                visible: Some((0, 23)),
            }
        );
        // 空区间。
        assert_eq!(
            capture_span(
                50,
                24,
                Some(CaptureLine::Line(5)),
                Some(CaptureLine::Line(4))
            ),
            CaptureSpan::default()
        );
    }

    #[test]
    fn join_merges_only_rows_flagged_as_wrapped() {
        let wrapped = |ch: &str| {
            let mut cell = cell(ch, 0, false);
            cell.style.as_mut().expect("cell style").wrapline = true;
            cell
        };
        // 第一行以 wrapline 结尾 -> 与第二行合并;第二行没有 -> 断行。
        let rows = vec![
            vec![cell("a", 0, false), wrapped("b")],
            vec![cell("c", 0, false), cell("d", 0, false)],
            vec![cell("e", 0, false), cell("f", 0, false)],
        ];
        assert_eq!(render_capture(&rows, true, false), "abcd\nef\n");
        assert_eq!(render_capture(&rows, false, false), "ab\ncd\nef\n");
    }

    #[test]
    fn join_follows_a_chain_of_wrapped_rows() {
        let wrapped = |ch: &str| {
            let mut cell = cell(ch, 0, false);
            cell.style.as_mut().expect("cell style").wrapline = true;
            cell
        };
        let rows = vec![
            vec![wrapped("a")],
            vec![wrapped("b")],
            vec![cell("c", 0, false)],
        ];
        assert_eq!(render_capture(&rows, true, false), "abc\n");
    }

    #[test]
    fn join_emits_one_sgr_run_across_the_merged_line() {
        let mut first = cell("a", 0xcd0000, true);
        first.style.as_mut().expect("cell style").wrapline = true;
        let rows = vec![vec![first], vec![cell("b", 0xcd0000, true)]];
        let text = render_capture(&rows, true, true);
        assert_eq!(
            text.matches("\x1b[").count(),
            2,
            "joined line should open once and reset once: {text:?}"
        );
        assert!(text.contains("ab"), "{text:?}");
    }

    #[test]
    fn capture_rejects_mixed_or_malformed_scrollback_pages() {
        let row = |index| mux_protocol::proto::RowChange {
            row: index,
            cells: vec![cell("x", 0, false), cell("y", 0, false)],
        };
        let valid = mux_protocol::proto::FetchScrollbackResponse {
            lines: vec![row(8), row(9)],
            total_lines: 10,
            scrollback_version: 7,
        };
        assert!(scrollback_matches_snapshot(&valid, 7, 10, 2, 8, 2));

        let mut changed = valid.clone();
        changed.scrollback_version = 8;
        assert!(!scrollback_matches_snapshot(&changed, 7, 10, 2, 8, 2));

        let mut missing = valid.clone();
        missing.lines[1].row = 10;
        assert!(!scrollback_matches_snapshot(&missing, 7, 10, 2, 8, 2));

        let mut narrow = valid;
        narrow.lines[1].cells.pop();
        assert!(!scrollback_matches_snapshot(&narrow, 7, 10, 2, 8, 2));
    }

    #[test]
    fn capture_requires_an_unchanged_grid_checkpoint() {
        let stable = FetchGridUpdateResponse {
            from_generation: 9,
            to_generation: 9,
            output_sequence: 0,
            update: None,
        };
        assert!(grid_checkpoint_is_stable(9, &stable));

        let changed = FetchGridUpdateResponse {
            to_generation: 10,
            update: Some(GridUpdateKind::Diff(Default::default())),
            ..stable
        };
        assert!(!grid_checkpoint_is_stable(9, &changed));
    }
}
