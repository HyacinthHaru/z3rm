//! §16.6 GUI entry point for remote sessions.
//!
//! `attach --ssh <uri>` is a launch flag: it only helps a user who is already
//! at a shell and already knows the URI. This is the same flow reachable from
//! inside a running window — pick a host, pick (or create) a session on it, and
//! get an ordinary z3rm window bound to the tunnel.

use anyhow::Context as _;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{App, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, prelude::*};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

/// Register the OpenRemote action: pick a host, connect, pick a session.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &settings::mux_actions::OpenRemote, cx: &mut App| {
        let hosts = ssh_config_hosts();
        crate::open_diff::in_active_workspace(cx, move |workspace, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    RemoteHostSelector::new(hosts, window, cx)
                });
            });
        });
    });
}

/// Host aliases declared in `~/.ssh/config`.
fn ssh_config_hosts() -> Vec<String> {
    let Some(path) = dirs::home_dir().map(|home| home.join(".ssh").join("config")) else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_ssh_config_hosts(&contents)
}

/// Wildcard patterns are skipped: `Host *` is a settings block, not somewhere
/// the user can connect to.
fn parse_ssh_config_hosts(contents: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("Host ")
            .or_else(|| line.strip_prefix("host "))
        else {
            continue;
        };
        for alias in rest.split_whitespace() {
            if !alias.contains(['*', '?']) && !hosts.iter().any(|known| known == alias) {
                hosts.push(alias.to_string());
            }
        }
    }
    hosts.sort();
    hosts
}

pub struct RemoteHostSelector {
    picker: Entity<Picker<RemoteHostSelectorDelegate>>,
}

impl ModalView for RemoteHostSelector {}

impl EventEmitter<DismissEvent> for RemoteHostSelector {}

impl Focusable for RemoteHostSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RemoteHostSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl RemoteHostSelector {
    fn new(hosts: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = RemoteHostSelectorDelegate::new(hosts, cx.entity().downgrade());
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

pub struct RemoteHostSelectorDelegate {
    selector: WeakEntity<RemoteHostSelector>,
    hosts: Vec<String>,
    query: String,
    selected_index: usize,
    matches: Vec<StringMatch>,
}

impl RemoteHostSelectorDelegate {
    fn new(hosts: Vec<String>, selector: WeakEntity<RemoteHostSelector>) -> Self {
        let matches = hosts
            .iter()
            .enumerate()
            .map(|(index, host)| StringMatch {
                candidate_id: index,
                score: 0.0,
                positions: Vec::new(),
                string: host.clone(),
            })
            .collect();
        Self {
            selector,
            hosts,
            query: String::new(),
            selected_index: 0,
            matches,
        }
    }
}

impl PickerDelegate for RemoteHostSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "remote host selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "ssh://user@host[:port] or an ~/.ssh/config host...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background_executor = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| StringMatchCandidate::new(index, host))
            .collect();

        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background_executor,
                )
                .await
            };

            this.update(cx, |this, _cx| {
                this.delegate.query = query;
                this.delegate.matches = matches;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.matches.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // A host the user typed in full is as valid a target as one from the
        // config, so an empty match list must still be able to connect.
        let target = self
            .matches
            .get(self.selected_index)
            .and_then(|selected| self.hosts.get(selected.candidate_id).cloned())
            .unwrap_or_else(|| self.query.trim().to_string());
        if target.is_empty() {
            self.dismissed(window, cx);
            return;
        }

        cx.spawn_in(window, async move |_this, cx| {
            if let Err(error) = open_remote_window(target.clone(), cx).await {
                tracing::error!(%target, %error, "mux::OpenRemote failed");
                cx.update(|_, cx| {
                    crate::daemon::show_daemon_error(
                        cx,
                        format!("Could not open {target}: {error:#}"),
                    );
                })
                .log_err();
            }
        })
        .detach();

        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let host_match = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(host_match.string.clone())),
        )
    }
}

/// §16.6 Connect to `target`, let the user choose a session there, open a window.
async fn open_remote_window(
    target: String,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    let uri = if target.contains("://") {
        target.clone()
    } else {
        format!("ssh://{target}")
    };
    let (domain, ssh_session) = mux::connect_ssh(&uri).await.with_context(|| {
        format!("failed to connect via SSH to {uri}. Ensure the host is reachable.")
    })?;
    let domain = Arc::new(domain);

    let session_id = choose_remote_session(&domain, cx).await?;
    let attach_response = domain.create_and_attach_window(&session_id).await?;
    let snapshot = crate::MuxSnapshot::from_attach(&attach_response);
    let app_state = cx
        .update(|_, cx| workspace::AppState::try_global(cx))?
        .context("the application state is not initialized")?;

    crate::open_mux_window_with_snapshot(
        domain,
        session_id,
        snapshot,
        app_state,
        Some(ssh_session),
        cx,
    )
    .await?;
    Ok(())
}

/// §16.6 Show the remote host's sessions and let the user pick or create one.
async fn choose_remote_session(
    domain: &Arc<mux::MuxDomain>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<String> {
    let sessions = domain
        .list_sessions()
        .await
        .context("failed to list sessions on the remote host")?;
    if sessions.is_empty() {
        return crate::daemon::ensure_target_session(domain, None).await;
    }

    let detail = sessions
        .iter()
        .map(|session| format!("{} (cwd: {})", session.name, session.cwd))
        .collect::<Vec<_>>()
        .join("\n");
    let mut answers = sessions
        .iter()
        .map(|session| {
            gpui::PromptButton::new(if session.name.is_empty() {
                session.id.clone()
            } else {
                session.name.clone()
            })
        })
        .collect::<Vec<_>>();
    answers.push(gpui::PromptButton::new("New session"));

    let answer = cx
        .update(|window, cx| {
            window.prompt(
                gpui::PromptLevel::Info,
                "Attach to a session on the remote host",
                Some(&detail),
                &answers,
                cx,
            )
        })?
        .await
        .context("the session prompt was dismissed")?;

    match sessions.get(answer) {
        Some(session) => Ok(session.id.clone()),
        // The trailing button is "New session".
        None => crate::daemon::ensure_target_session(domain, None).await,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ssh_config_hosts_skips_wildcard_blocks() {
        // `Host *` configures every connection; it is not a destination.
        let hosts = super::parse_ssh_config_hosts("Host *\n  User root\nHost build web\n");
        assert_eq!(hosts, vec!["build".to_string(), "web".to_string()]);
    }

    #[test]
    fn ssh_config_hosts_deduplicates() {
        let hosts = super::parse_ssh_config_hosts("Host web\nHost web\nHost api\n");
        assert_eq!(hosts, vec!["api".to_string(), "web".to_string()]);
    }
}
