// capture-pane 实现: 从 server 拉取 grid 并转为文本
// 来源: spec §3.10 — capture-pane -p 输出 pane 可见内容

use anyhow::{Context, Result};
use mux::MuxDomain;
use mux_protocol::proto::{Cell, CellStyle, fetch_grid_update_response::Update as GridUpdateKind};

/// 捕获 pane 的可见网格内容，转换为文本。
///
/// - `pane_id`: pane 的唯一标识
/// - `scrollback_lines`: 可选的历史行数。负值表示包含 scrollback（最新 N 行）。
/// - `preserve_ansi`: 是否保留 ANSI 颜色/样式码
pub async fn capture_pane(
    domain: &MuxDomain,
    pane_id: &str,
    scrollback_lines: Option<i32>,
    preserve_ansi: bool,
) -> Result<String> {
    const MAX_CAPTURE_ATTEMPTS: usize = 3;
    let requested_history = scrollback_lines
        .filter(|lines| *lines < 0)
        .map(i32::unsigned_abs);

    for _ in 0..MAX_CAPTURE_ATTEMPTS {
        let grid = domain
            .fetch_grid_update(pane_id, 0)
            .await
            .context("failed to fetch grid update")?;
        let Some(GridUpdateKind::FullSnapshot(snapshot)) = grid.update.as_ref() else {
            anyhow::bail!("capture-pane expected a full grid snapshot");
        };

        let mut scrollback_rows = Vec::new();
        if let Some(requested) = requested_history {
            if let Some((from, count)) = scrollback_tail(snapshot.history_size, requested) {
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
                scrollback_rows.extend(scrollback.lines.iter().map(|row| row.cells.clone()));
            }
        }

        let scrollback_row_slices = scrollback_rows
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        return Ok(render_capture(
            &scrollback_row_slices,
            grid.update.as_ref(),
            preserve_ansi,
        ));
    }

    anyhow::bail!("terminal history changed while capture-pane was reading it")
}

fn scrollback_tail(history_size: u32, requested: u32) -> Option<(u32, u32)> {
    let count = requested.min(history_size);
    (count != 0).then_some((history_size - count, count))
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

fn render_capture(
    scrollback_rows: &[&[Cell]],
    grid_update: Option<&GridUpdateKind>,
    preserve_ansi: bool,
) -> String {
    let mut output = String::new();
    for row in scrollback_rows {
        output.push_str(&render_cells(row, preserve_ansi));
        output.push('\n');
    }

    if let Some(update) = grid_update {
        match update {
            GridUpdateKind::FullSnapshot(snapshot) => {
                for row in 0..snapshot.rows {
                    let mut row_cells = Vec::with_capacity(snapshot.cols as usize);
                    for col in 0..snapshot.cols {
                        let idx = (row * snapshot.cols + col) as usize;
                        if let Some(cell) = snapshot.cells.get(idx) {
                            row_cells.push(cell.clone());
                        }
                    }
                    output.push_str(&render_cells(&row_cells, preserve_ansi));
                    output.push('\n');
                }
            }
            GridUpdateKind::Diff(diff) => {
                for row_change in &diff.rows {
                    output.push_str(&render_cells(&row_change.cells, preserve_ansi));
                    output.push('\n');
                }
            }
        }
    }

    output
}

fn render_cells(cells: &[Cell], preserve_ansi: bool) -> String {
    if !preserve_ansi {
        return cells.iter().map(|c| c.char.as_str()).collect();
    }

    let mut output = String::new();
    let mut current: Option<SgrState> = None;
    for cell in cells {
        let next = SgrState::from_cell(cell);
        if current.as_ref() != Some(&next) {
            output.push_str(&next.to_sgr());
            current = Some(next);
        }
        output.push_str(&cell.char);
    }
    if current.is_some() {
        output.push_str("\x1b[0m");
    }
    output
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
    fn capture_with_scrollback_appends_visible_grid() {
        let scrollback = vec![cell("h", 0, false)];
        let visible = vec![cell("v", 0, false)];
        let update = GridUpdateKind::FullSnapshot(mux_protocol::proto::FullGridSnapshot {
            cols: 1,
            rows: 1,
            cells: visible,
            ..Default::default()
        });

        let text = render_capture(&[scrollback.as_slice()], Some(&update), false);
        assert_eq!(text, "h\nv\n");
    }
    #[test]
    fn capture_requests_only_the_latest_scrollback_rows() {
        assert_eq!(scrollback_tail(10_000, 2), Some((9_998, 2)));
        assert_eq!(scrollback_tail(3, 10), Some((0, 3)));
        assert_eq!(scrollback_tail(0, 10), None);
        assert_eq!(scrollback_tail(10, 0), None);
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
