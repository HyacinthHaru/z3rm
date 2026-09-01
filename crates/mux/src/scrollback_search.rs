//! §12 Scrollback search: a regex over a pane's history *and* its viewport.
//!
//! The server's `SearchScrollback` only covers history, so a match still on
//! screen would not be found — which is nothing but a surprise to the caller.
//! The viewport half is added here, over the grid snapshot that has to be
//! fetched anyway to convert line numbers, so it costs no extra round trip.
//!
//! Both surfaces search the same way. The CLI's `search-scrollback` and the
//! GUI's in-pane search are the same call with different presentation; a
//! second implementation would be a second set of off-by-ones.

use crate::MuxDomain;
use anyhow::{Context, Result};
use mux_protocol::proto::{
    Cell, FullGridSnapshot, fetch_grid_update_response::Update as GridUpdateKind,
};

/// 搜索方向与结果上限。
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    /// 起始行 (tmux 行号: 可见区第一行是 0, 负数进入历史)。缺省时覆盖整个
    /// pane —— 也是"从最开头"这个边界的含义, 两者落在同一个分支。
    pub start: Option<i32>,
    /// `--forward`：朝更新的方向搜，取最旧的 N 条；缺省朝更旧搜，取最新的 N 条。
    pub forward: bool,
    /// `-n`，结果上限。
    pub max_results: u32,
}

/// 一条命中：tmux 行号 + 该行的纯文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// tmux 行号：可见区第一行是 0，负数进入历史。
    pub line: i32,
    pub text: String,
}

/// 在 pane 里搜索 `pattern`，返回按行号升序排列的命中。
///
/// 服务端的 `SearchScrollback` 只覆盖历史，可见区不在范围内 —— 一个还显示在屏幕
/// 上的匹配会查不到，这对调用方是纯粹的意外。可见区那一段因此在这里补上：抓
/// grid 快照本来就是为了拿 `history_size` 换算行号，顺带扫一遍不额外要一次 RPC。
pub async fn search_scrollback(
    domain: &MuxDomain,
    pane_id: &str,
    pattern: &str,
    options: SearchOptions,
) -> Result<Vec<SearchHit>> {
    // 服务端对无法编译的正则是静默返回空列表的，从 CLI 看就是"没搜到"。先在本地
    // 编译一次，把语法错误变成用户能读懂的报错。
    let regex = regex::Regex::new(pattern)
        .with_context(|| format!("invalid regular expression: {pattern}"))?;

    let grid = domain
        .fetch_grid_update(pane_id, 0)
        .await
        .context("failed to fetch grid update")?;
    let Some(GridUpdateKind::FullSnapshot(snapshot)) = grid.update.as_ref() else {
        anyhow::bail!("search-scrollback expected a full grid snapshot");
    };

    let span = search_span(
        snapshot.history_size,
        snapshot.rows,
        options.start,
        options.forward,
    );

    let mut hits = Vec::new();
    if let Some((from_line, direction)) = span.history {
        let response = domain
            .search_scrollback(pane_id, pattern, from_line, direction, options.max_results)
            .await
            .context("failed to search scrollback")?;
        hits.extend(response.matches.into_iter().map(|found| SearchHit {
            line: found.line_number as i32 - clamp_to_i32(snapshot.history_size),
            text: plain_text(found.context.iter()),
        }));
    }
    if let Some((first, last)) = span.visible {
        hits.extend(visible_hits(snapshot, first, last, &regex));
    }

    hits.sort_by_key(|hit| hit.line);
    hits.dedup_by_key(|hit| hit.line);
    truncate_by_direction(&mut hits, options.max_results, options.forward);
    Ok(hits)
}

/// 一次搜索要覆盖的范围，拆成"历史"和"可见区"两段。
#[derive(Debug, Default, PartialEq, Eq)]
struct SearchSpan {
    /// `(from_line, direction)`，直接喂给 `search_scrollback` RPC。
    /// `from_line` 是历史行下标 (0 = 最旧)，`direction` 0 = 向更旧、1 = 向更新。
    history: Option<(u32, u32)>,
    /// 可见区的闭区间 `[first_row, last_row]`。
    visible: Option<(u32, u32)>,
}

/// 把一个 tmux 起始行号 + 方向，换算成历史段的 RPC 参数和可见区的行区间。
fn search_span(
    history_size: u32,
    rows: u32,
    start: Option<i32>,
    forward: bool,
) -> SearchSpan {
    let oldest = -clamp_to_i32(history_size);
    let newest = clamp_to_i32(rows) - 1;
    if rows == 0 && history_size == 0 {
        return SearchSpan::default();
    }

    // 缺省起点取搜索方向的"上游"端点，于是整个 pane 都在范围内。
    let start = match start {
        None => {
            if forward {
                oldest
            } else {
                newest
            }
        }
        Some(line) => line.clamp(oldest, newest),
    };

    let history = if history_size == 0 {
        None
    } else if forward {
        // 向更新搜：起点落在可见区时历史全在其后方，没有要搜的历史。
        (start < 0).then(|| ((history_size as i64 + start as i64).max(0) as u32, 1))
    } else {
        // 向更旧搜：起点落在可见区时整段历史都在其前方，从最新一行历史开始。
        let from = if start < 0 {
            (history_size as i64 + start as i64).max(0) as u32
        } else {
            history_size - 1
        };
        Some((from, 0))
    };

    let visible = if rows == 0 {
        None
    } else if forward {
        Some((start.max(0) as u32, newest as u32))
    } else {
        // 向更旧搜且起点在历史里时，可见区整个都在起点之后，不参与。
        (start >= 0).then(|| (0, start as u32))
    };

    SearchSpan { history, visible }
}

/// A row of cells as the text a regex sees.
///
/// A wide character occupies two cells; the second carries no text of its own,
/// so including it would put a stray column between the halves of every CJK
/// glyph and break any pattern spanning one.
pub fn plain_text<'a>(cells: impl IntoIterator<Item = &'a Cell>) -> String {
    cells
        .into_iter()
        .filter(|cell| {
            !cell.style.as_ref().is_some_and(|style| {
                style.wide_char_spacer || style.leading_wide_char_spacer
            })
        })
        .map(|cell| {
            let mut text = cell.char.clone();
            text.push_str(&cell.zerowidth);
            text
        })
        .collect()
}

fn clamp_to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn visible_hits(
    snapshot: &FullGridSnapshot,
    first: u32,
    last: u32,
    regex: &regex::Regex,
) -> Vec<SearchHit> {
    let columns = snapshot.cols as usize;
    (first..=last)
        .filter_map(|row| {
            let offset = row as usize * columns;
            let cells: Vec<&Cell> = (0..columns)
                .filter_map(|column| snapshot.cells.get(offset + column))
                .collect();
            let text = plain_text(cells);
            regex.is_match(&text).then(|| SearchHit {
                line: row as i32,
                text,
            })
        })
        .collect()
}

/// 结果上限按搜索方向的"下游"端裁剪：向更旧搜时留最新的 N 条，反之留最旧的 N 条。
fn truncate_by_direction(hits: &mut Vec<SearchHit>, max_results: u32, forward: bool) {
    let max_results = max_results as usize;
    if hits.len() <= max_results {
        return;
    }
    if forward {
        hits.truncate(max_results);
    } else {
        hits.drain(..hits.len() - max_results);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::proto::CellStyle;

    fn snapshot(rows: u32, cols: u32, text: &[&str]) -> FullGridSnapshot {
        let cells = text
            .iter()
            .flat_map(|line| {
                let mut row: Vec<Cell> = line
                    .chars()
                    .map(|character| Cell {
                        char: character.to_string(),
                        style: Some(CellStyle::default()),
                        ..Default::default()
                    })
                    .collect();
                row.resize_with(cols as usize, || Cell {
                    char: " ".to_string(),
                    style: Some(CellStyle::default()),
                    ..Default::default()
                });
                row
            })
            .collect();
        FullGridSnapshot {
            rows,
            cols,
            cells,
            ..Default::default()
        }
    }

    #[test]
    fn default_span_covers_history_and_the_whole_viewport() {
        // 向更旧搜(缺省): 从最新一行历史开始，可见区整个参与。
        assert_eq!(
            search_span(50, 24, None, false),
            SearchSpan {
                history: Some((49, 0)),
                visible: Some((0, 23)),
            }
        );
        // 向更新搜: 从最旧一行历史开始，可见区整个参与。
        assert_eq!(
            search_span(50, 24, None, true),
            SearchSpan {
                history: Some((0, 1)),
                visible: Some((0, 23)),
            }
        );
    }

    #[test]
    fn a_start_inside_the_viewport_splits_the_two_segments() {
        // 向更旧: 可见区只到起点为止，历史全在范围内。
        assert_eq!(
            search_span(50, 24, Some(5), false),
            SearchSpan {
                history: Some((49, 0)),
                visible: Some((0, 5)),
            }
        );
        // 向更新: 历史全在起点之前，不参与。
        assert_eq!(
            search_span(50, 24, Some(5), true),
            SearchSpan {
                history: None,
                visible: Some((5, 23)),
            }
        );
    }

    #[test]
    fn a_start_inside_history_splits_the_two_segments() {
        // 向更旧: 只搜起点及更旧的历史，可见区不参与。
        assert_eq!(
            search_span(50, 24, Some(-10), false),
            SearchSpan {
                history: Some((40, 0)),
                visible: None,
            }
        );
        // 向更新: 从起点开始的历史 + 整个可见区。
        assert_eq!(
            search_span(50, 24, Some(-10), true),
            SearchSpan {
                history: Some((40, 1)),
                visible: Some((0, 23)),
            }
        );
    }

    #[test]
    fn a_start_beyond_the_pane_is_clamped_not_sent_out_of_range() {
        assert_eq!(
            search_span(3, 24, Some(-999), false),
            SearchSpan {
                history: Some((0, 0)),
                visible: None,
            }
        );
        assert_eq!(
            search_span(3, 24, Some(999), true),
            SearchSpan {
                history: None,
                visible: Some((23, 23)),
            }
        );
    }

    #[test]
    fn a_pane_without_history_searches_only_the_viewport() {
        assert_eq!(
            search_span(0, 24, None, false),
            SearchSpan {
                history: None,
                visible: Some((0, 23)),
            }
        );
    }

    #[test]
    fn visible_matches_carry_viewport_line_numbers() {
        let grid = snapshot(3, 6, &["alpha", "beta", "alpha2"]);
        let regex = regex::Regex::new("alpha").expect("regex");
        let hits = visible_hits(&grid, 0, 2, &regex);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].line, 0);
        assert_eq!(hits[0].text, "alpha ");
        assert_eq!(hits[1].line, 2);
    }

    #[test]
    fn backward_search_keeps_the_newest_results() {
        let mut hits: Vec<SearchHit> = (-5..0)
            .map(|line| SearchHit {
                line,
                text: String::new(),
            })
            .collect();
        truncate_by_direction(&mut hits, 2, false);
        assert_eq!(
            hits.iter().map(|hit| hit.line).collect::<Vec<_>>(),
            vec![-2, -1]
        );
    }

    #[test]
    fn forward_search_keeps_the_oldest_results() {
        let mut hits: Vec<SearchHit> = (-5..0)
            .map(|line| SearchHit {
                line,
                text: String::new(),
            })
            .collect();
        truncate_by_direction(&mut hits, 2, true);
        assert_eq!(
            hits.iter().map(|hit| hit.line).collect::<Vec<_>>(),
            vec![-5, -4]
        );
    }
}
