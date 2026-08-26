//! Session bootstrap requests for the mux server running in the v86 guest.
//!
//! Transport setup lives in `serial_link`; this module only performs the
//! protocol-level create-session, spawn-pane and attach sequence once the
//! guest server is ready.

use anyhow::{Context as _, Result};
use mux::MuxDomain;
use std::path::Path;
use std::sync::Arc;

/// Create the session this tab opens into and attach a window to it.
///
/// The same three steps the desktop bootstrap takes once the daemon answers:
/// a session, a terminal in it, then a window whose attach response carries
/// the layout to render.
pub async fn open_session(
    domain: &Arc<MuxDomain>,
) -> Result<(String, mux_protocol::AttachResponse)> {
    let session_id = domain
        .create_session("web", Path::new("/"))
        .await
        .context("creating the session for this tab")?;
    ensure_pane_in_session(domain, &session_id).await?;
    let attach = domain
        .create_and_attach_window(&session_id)
        .await
        .context("attaching a window to the session")?;
    Ok((session_id, attach))
}

/// A fresh session has a tab but no pane, so give it one terminal.
///
/// The size is a placeholder: the pane reports its real size to the server on
/// the first layout pass, and the server resizes the pty then.
async fn ensure_pane_in_session(domain: &Arc<MuxDomain>, session_id: &str) -> Result<()> {
    let attach = domain
        .attach(session_id, mux::AttachMode::Shared)
        .await
        .context("reading the session before spawning its first pane")?;
    let snapshot = attach.snapshot.context("no snapshot in attach response")?;
    if snapshot.tabs.iter().any(|tab| !tab.panes.is_empty()) {
        domain.detach().await?;
        return Ok(());
    }
    let fallback_tab = String::from("default");
    let tab_id = snapshot
        .tabs
        .first()
        .map(|tab| &tab.id)
        .unwrap_or(&fallback_tab);
    domain
        .spawn_pane(
            session_id,
            tab_id,
            mux_protocol::TerminalSize {
                cols: 120,
                rows: 32,
            },
            None,
            Some(Path::new("/")),
        )
        .await
        .context("spawning the session's first pane")?;
    domain.detach().await?;
    Ok(())
}
