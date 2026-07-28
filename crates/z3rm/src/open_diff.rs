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
use std::sync::Arc;
use gpui::{App, AppContext as _, PathPromptOptions};
use anyhow::Context as _;
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

            let (previous, target_version_id, domain, session_id) =
                match crate::open_diff::fetch_previous_version(&path, cx).await {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "OpenDiff: shadow version fetch failed; using on-disk content as fallback"
                        );
                        let fallback = match smol::unblock({
                            let path = path.clone();
                            move || std::fs::read_to_string(&path)
                        })
                        .await
                        {
                            Ok(text) => text,
                            Err(read_error) => {
                                tracing::error!(path = %path.display(), error = %read_error, "OpenDiff read failed");
                                return;
                            }
                        };
                        let domain_result = cx.update(|cx| {
                            workspace::AppState::try_global(cx)
                                .and_then(|state| state.mux_domain.clone())
                        });
                        let domain = match domain_result {
                            Some(domain) => domain,
                            None => {
                                tracing::error!("OpenDiff: mux domain unavailable for fallback");
                                return;
                            }
                        };
                        let session_id = domain
                            .last_attached_session_id()
                            .unwrap_or_default();
                        (fallback, 0, domain, session_id)
                    }
                };
            let task = cx.update(|cx| {
                DiffReview::load(
                    path.clone(),
                    previous.clone(),
                    domain,
                    session_id,
                    target_version_id,
                    cx,
                )
            });
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


async fn fetch_previous_version(
    path: &std::path::Path,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<(String, u64, Arc<mux::MuxDomain>, String)> {
    let domain = cx
        .update(|cx| {
            workspace::AppState::try_global(cx)
                .and_then(|state| state.mux_domain.clone())
        })
        .with_context(|| "mux domain not available")?;
    let session_id = domain
        .last_attached_session_id()
        .context("no session attached")?;
    let path_str = path.to_string_lossy().into_owned();
    let versions_response = domain
        .list_file_versions(&session_id, &path_str)
        .await?;
    let versions = versions_response.versions;
    if versions.len() < 2 {
        anyhow::bail!("need at least 2 versions to diff, found {}", versions.len());
    }
    let target = &versions[versions.len() - 2];
    let content_response = domain
        .get_file_version(&session_id, &path_str, target.version_id)
        .await?;
    let previous = String::from_utf8(content_response.content)
        .map_err(|error| anyhow::anyhow!("shadow version is not valid UTF-8: {error}"))?;
    Ok((previous, target.version_id, domain, session_id))
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

}