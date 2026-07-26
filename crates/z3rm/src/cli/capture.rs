// capture-pane 实现: 从 server 拉取 grid 并转为文本
// 来源: spec §3.10 — capture-pane -p 输出 pane 可见内容

use anyhow::{Context, Result};
use mux::MuxDomain;
use mux_protocol::proto::{
    fetch_grid_update_response::Update as GridUpdateKind, Cell, CellStyle,
};

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
    let mut output = String::new();

    // 负值 -N：取 scrollback 尾部最新 N 行（不是最旧 N 行）。
    if let Some(n) = scrollback_lines.filter(|n| *n < 0) {
        let count = n.unsigned_abs();
        let scrollback = domain
            .fetch_scrollback(pane_id, 0, 0, u32::MAX)
            .await
            .context("failed to fetch scrollback")?;
        let total = scrollback.lines.len();
        let start = total.saturating_sub(count as usize);
        for row in &scrollback.lines[start..] {
            output.push_str(&render_cells(&row.cells, preserve_ansi));
            output.push('\n');
        }
    }

    let grid = domain
        .fetch_grid_update(pane_id, 0)
        .await
        .context("failed to fetch grid update")?;

    if let Some(update) = &grid.update {
        match update {
            GridUpdateKind::FullSnapshot(snapshot) => {
                for row in 0..snapshot.rows {
                    let mut row_cells = Vec::with_capacity(snapshot.cols as usize);
                    for col in 0..snapshot.cols {
                        let idx = (row * snapshot.cols + col) as usize;
                        if idx < snapshot.cells.len() {
                            row_cells.push(snapshot.cells[idx].clone());
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

    Ok(output)
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
        assert_eq!(text.matches("\x1b[").count(), 2, "one open SGR + one reset: {text:?}");
        assert!(text.contains("ab"));
    }
}
