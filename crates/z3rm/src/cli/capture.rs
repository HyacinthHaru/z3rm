// capture-pane 实现: 从 server 拉取 grid 并转为文本
// 来源: spec §3.10 — capture-pane -p 输出 pane 可见内容

use anyhow::{Context, Result};
use mux::MuxDomain;
use mux_protocol::proto::{
    Cell, FullGridSnapshot, fetch_grid_update_response::Update as GridUpdateKind,
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

        let span = capture_span(
            snapshot.history_size,
            snapshot.rows,
            options.start,
            options.end,
        );

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        if let Some((from, count)) = span.history {
            let scrollback = domain
                .fetch_scrollback(pane_id, from, 1, count)
                .await
                .context("failed to fetch scrollback")?;
            if !scrollback_matches_snapshot(
                &scrollback,
                snapshot.history_version,
                snapshot.history_size,
                snapshot.cols,
                from,
                count,
            ) {
                continue;
            }
            rows.extend(scrollback.lines.iter().map(|row| row.cells.clone()));
        }
        if let Some((first, last)) = span.visible {
            rows.extend(visible_rows(snapshot, first, last));
        }

        return Ok(render_capture(
            &rows,
            options.join_wrapped,
            options.preserve_ansi,
        ));
    }

    anyhow::bail!("terminal history changed while capture-pane was reading it")
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

fn visible_rows(snapshot: &FullGridSnapshot, first: u32, last: u32) -> Vec<Vec<Cell>> {
    let cols = snapshot.cols as usize;
    (first..=last)
        .map(|row| {
            let offset = row as usize * cols;
            (0..cols)
                .filter_map(|col| snapshot.cells.get(offset + col).cloned())
                .collect()
        })
        .collect()
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
            row.row == from + index as u32 && row.cells.len() == columns as usize
        })
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
    for cell in cells {
        if cell
            .style
            .as_ref()
            .is_some_and(|style| style.wide_char_spacer)
        {
            continue;
        }
        let next = SgrState::from_cell(cell);
        if current.as_ref() != Some(&next) {
            output.push_str(&next.to_sgr());
            current = Some(next);
        }
        output.push_str(&cell_text(cell));
    }
    if current.is_some() {
        output.push_str("\x1b[0m");
    }
    output
}

fn cell_text(cell: &Cell) -> String {
    if cell
        .style
        .as_ref()
        .is_some_and(|style| style.wide_char_spacer)
    {
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
    strikethrough: bool,
    dim: bool,
    reverse: bool,
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
            strikethrough: style.strikethrough,
            dim: style.dim,
            reverse: style.reverse,
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
        if self.underline {
            parts.push("4".into());
        }
        if self.reverse {
            parts.push("7".into());
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
        format!("\x1b[{}m", parts.join(";"))
    }
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
    use mux_protocol::proto::CellStyle;

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
            render_capture(&visible_rows(&snapshot, 1, 2), false, false),
            "cd\nef\n"
        );
        assert_eq!(
            render_capture(&visible_rows(&snapshot, 0, 0), false, false),
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
}
