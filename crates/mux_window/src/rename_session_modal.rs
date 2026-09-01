//! §3.10 Renaming a session from the GUI.
//!
//! The CLI has had `z3rm rename-session` since the beginning; the GUI listed
//! sessions by name and offered no way to change one. The server owns the name,
//! so this is a prompt and a request — what the window renders afterwards is
//! whatever the server broadcasts back.

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window,
};
use std::sync::Arc;
use ui::{AlertModal, prelude::*};
use ui_input::InputField;

pub struct RenameSessionModal {
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    name: Entity<InputField>,
    focus_handle: FocusHandle,
}

impl RenameSessionModal {
    pub fn new(
        domain: Arc<mux::MuxDomain>,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputField::new(window, cx, "Session name").label("Name"));
        // Opening on the current name is what makes this a rename rather than
        // a retype, and the server is the only place that name lives. The
        // field is usable while the answer is in flight; a user who has already
        // typed keeps what they typed.
        cx.spawn_in(window, {
            let domain = domain.clone();
            let session_id = session_id.clone();
            let name = name.clone();
            async move |_, cx| {
                let Ok(sessions) = domain.list_sessions().await else {
                    return;
                };
                let Some(current) = sessions
                    .into_iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.name)
                else {
                    return;
                };
                if let Err(error) = cx.update(|window, cx| {
                    let editor = name.read(cx).editor().clone();
                    if editor.text(cx).is_empty() {
                        editor.set_text(&current, window, cx);
                    }
                }) {
                    tracing::debug!(%error, "modal closed before the current name arrived");
                }
            }
        })
        .detach();

        Self {
            domain,
            session_id,
            name,
            focus_handle: cx.focus_handle(),
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            // A session with no name is one the sidebar cannot label, so this
            // is refused here rather than sent for the server to refuse.
            self.set_error(Some("A session needs a name"), cx);
            return;
        }
        self.set_error(None::<&str>, cx);
        let domain = self.domain.clone();
        let session_id = self.session_id.clone();
        cx.spawn(async move |this, cx| {
            let result = domain.rename_session(&session_id, &name).await;
            if let Err(error) = this.update(cx, |this, cx| match result {
                Ok(()) => cx.emit(DismissEvent),
                Err(error) => {
                    tracing::warn!(error = %error, session_id, "rename_session failed");
                    // Kept on screen rather than dismissing: renaming needs the
                    // admin role, and a modal that simply closed would look
                    // exactly like a rename that worked.
                    this.set_error(Some(format!("{error}")), cx);
                }
            }) {
                tracing::debug!(%error, "modal dismissed before the rename answered");
            }
        })
        .detach();
    }

    fn set_error(&self, error: Option<impl Into<SharedString>>, cx: &mut Context<Self>) {
        self.name
            .update(cx, |field, cx| field.set_error(error, cx));
        cx.notify();
    }
}

impl Focusable for RenameSessionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RenameSessionModal {}

impl workspace::ModalView for RenameSessionModal {
    fn a11y_name(&self, _cx: &App) -> Option<SharedString> {
        Some("Rename session".into())
    }
}

impl Render for RenameSessionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        AlertModal::new("rename-session-modal")
            .title("Rename session")
            .width(rems(28.))
            .key_context("RenameSessionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| this.confirm(cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _window, cx| cx.emit(DismissEvent)))
            // The field carries its own error live region, so the refusal is
            // announced from beside the input it is about.
            .child(self.name.clone())
    }
}
