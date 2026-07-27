//! §16.6 OpenDiff command palette entry — open a file as DiffView.
//!
//! Baseline: compare current disk content against an empty previous version
//! (full file shown as additions). When shadow_snapshot previous content is
//! available for the path, callers can pass it as `previous`.

use gpui::{App, AppContext as _, PathPromptOptions};
use workspace::ItemHandle;

/// Compute a minimal unified-diff-like display between previous and current.
pub fn unified_diff(previous: &str, current: &str) -> String {
    let prev_lines: Vec<&str> = previous.lines().collect();
    let curr_lines: Vec<&str> = current.lines().collect();
    let mut out = String::new();
    out.push_str("--- previous\n+++ current\n");
    let max = prev_lines.len().max(curr_lines.len());
    for i in 0..max {
        match (prev_lines.get(i), curr_lines.get(i)) {
            (Some(p), Some(c)) if p == c => {
                out.push(' ');
                out.push_str(p);
                out.push('\n');
            }
            (Some(p), Some(c)) => {
                out.push('-');
                out.push_str(p);
                out.push('\n');
                out.push('+');
                out.push_str(c);
                out.push('\n');
            }
            (Some(p), None) => {
                out.push('-');
                out.push_str(p);
                out.push('\n');
            }
            (None, Some(c)) => {
                out.push('+');
                out.push_str(c);
                out.push('\n');
            }
            (None, None) => {}
        }
    }
    out
}

/// Register the OpenDiff action: pick a file, open DiffView in active workspace.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &workspace::OpenDiff, cx: &mut App| {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select file for diff review".into()),
        });
        cx.spawn(async move |cx| {
            let result = match paths.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "OpenDiff path prompt failed");
                    return;
                }
                Err(_) => return,
            };
            let Some(path) = result.into_iter().next() else {
                return;
            };
            let current = match smol::unblock({
                let path = path.clone();
                move || std::fs::read_to_string(&path)
            })
            .await
            {
                Ok(text) => text,
                Err(error) => {
                    tracing::error!(path = %path.display(), error = %error, "OpenDiff read failed");
                    return;
                }
            };
            let previous = String::new();
            let title = format!(
                "Diff: {}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
            );
            let unified = unified_diff(&previous, &current);
            let _ = cx.update(|cx| {
                open_diff_in_any_workspace(unified, title, cx);
            });
        })
        .detach();
    });
}

fn open_diff_in_any_workspace(unified: String, title: String, cx: &mut App) {
    for window_handle in cx.windows() {
        let opened = window_handle.update(cx, |_root, window, cx| {
            let Some(Some(multi)) = window.root::<workspace::MultiWorkspace>() else {
                return false;
            };
            let workspace = multi.read(cx).workspace().clone();
            workspace.update(cx, |workspace, cx| {
                let item: Box<dyn ItemHandle> = Box::new(cx.new(|cx| {
                    terminal_view::diff_view::DiffView::new(
                        unified.clone(),
                        title.clone(),
                        window,
                        cx,
                    )
                }));
                let pane = workspace.active_pane().clone();
                workspace.add_item(pane, item, None, true, true, window, cx);
            });
            true
        });
        if matches!(opened, Ok(true)) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unified_diff;

    #[test]
    fn unified_diff_marks_additions_and_removals() {
        let diff = unified_diff("a\nb\n", "a\nc\n");
        assert!(diff.contains("-b"));
        assert!(diff.contains("+c"));
        assert!(diff.contains(" a"));
    }
}
