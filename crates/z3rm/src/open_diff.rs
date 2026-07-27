//! §16.6 OpenDiff command palette entry — open a file as DiffView.
//!
//! Baseline: compare current disk content against an empty previous version
//! (full file shown as additions). When shadow_snapshot previous content is
//! available for the path, callers can pass it as `previous`.
//!
//! §16.6 / StubSweep3 #5: Now creates a `DiffReview` item (from
//! `crate::diff_review::DiffReview`) instead of the generic `DiffView`, so the
//! side-by-side Accept/Decline UI is reachable. The previous-version content
//! currently falls back to the on-disk file content (identical = no diff
//! highlights). When the Decline/ListVersions RPC from shadow_snapshot lands,
//! wire `DiffReview::load` with the real historical version from the shadow
//! engine — the helper already accepts `previous_content` from the caller.

use crate::diff_review::DiffReview;
use gpui::{App, AppContext as _, PathPromptOptions};
use workspace::ItemHandle;

/// Register the OpenDiff action: pick a file, open DiffReview in active workspace.
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

            // §16.6 / StubSweep3 #5: Fallback previous-version fetch.
            // When ShadowDurability lands with the Decline/ListVersions RPC, replace
            // this with a real query to the shadow engine. For now, warn and use
            // the current file content as "previous" so the DiffReview UI is
            // reachable now.
            let previous = match smol::unblock({
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
            tracing::warn!(
                path = %path.display(),
                "OpenDiff: previous-version fetch not yet wired to shadow_snapshot; \
                 using on-disk content as fallback — diff will show no changes"
            );
            // Build DiffReview from loaded content and add to the active workspace pane.
            let task = cx.update(|cx| DiffReview::load(path.clone(), previous.clone(), cx));
            let entity = match task.await {
                Ok(e) => e,
                Err(error) => {
                    tracing::error!(path = %path.display(), error = %error, "DiffReview::load failed");
                    return;
                }
            };
            let _ = cx.update(|cx| {
                for window_handle in cx.windows() {
                    let opened = window_handle.update(cx, |_root, window, cx| {
                        let Some(Some(multi)) = window.root::<workspace::MultiWorkspace>() else {
                            return false;
                        };
                        let workspace = multi.read(cx).workspace().clone();
                        workspace.update(cx, |workspace, cx| {
                            let pane = workspace.active_pane().clone();
                            workspace.add_item(
                                pane,
                                Box::new(entity.clone()),
                                None,
                                true,
                                true,
                                window,
                                cx,
                            );
                        });
                        true
                    });
                    if matches!(opened, Ok(true)) {
                        break;
                    }
                }
            });
        })
        .detach();
    });
}


/// Compute a minimal unified-diff-like display between previous and current.
pub fn unified_diff(previous: &str, current: &str) -> String {
    let prev_lines: Vec<&str> = previous.lines().collect();
    let curr_lines: Vec<&str> = current.lines().collect();
    let mut out = String::new();
    out.push_str("--- previous\n+++ current\n");
    let max = prev_lines.len().max(curr_lines.len());
    for i in 0..max {
        match (prev_lines.get(i), curr_lines.get(i)) {
            (Some(prev), Some(curr)) => {
                if prev == curr {
                    out.push_str(&format!(" {}\n", prev));
                } else {
                    out.push_str(&format!("-{}\n+{}\n", prev, curr));
                }
            }
            (Some(prev), None) => {
                out.push_str(&format!("-{}\n", prev));
            }
            (None, Some(curr)) => {
                out.push_str(&format!("+{}\n", curr));
            }
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_marks_additions_and_removals() {
        let diff = unified_diff("a\nb\nc", "a\nx\nc");
        assert!(diff.contains("- b"), "should mark removed line");
        assert!(diff.contains("+ x"), "should mark added line");
    }

    fn unified_diff(previous: &str, current: &str) -> String {
        let prev_lines: Vec<&str> = previous.lines().collect();
        let curr_lines: Vec<&str> = current.lines().collect();
        let mut out = String::new();
        out.push_str("--- previous\n+++ current\n");
        let max = prev_lines.len().max(curr_lines.len());
        for i in 0..max {
            match (prev_lines.get(i), curr_lines.get(i)) {
                (Some(prev), Some(curr)) if prev == curr => {
                    out.push_str(&format!(" {}\n", prev));
                }
                (Some(prev), Some(curr)) if prev != curr => {
                    out.push_str(&format!("-{}\n+{}\n", prev, curr));
                }
                (Some(prev), None) => {
                    out.push_str(&format!("-{}\n", prev));
                }
                (None, Some(curr)) => {
                    out.push_str(&format!("+{}\n", curr));
                }
                (None, None) => {}
            }
        }
        out
    }
}