//! Browser bootstrap for the production z3rm workspace tree.

#[cfg(target_family = "wasm")]
use std::{path::Path, sync::Arc};

#[cfg(target_family = "wasm")]
use anyhow::Result;
#[cfg(target_family = "wasm")]
use gpui::{App, AppContext as _, Global, TaskExt as _};

/// Keep the in-process server alive for as long as the browser application.
#[cfg(target_family = "wasm")]
struct WebMuxRuntime {
    _server: Arc<mux_server::wasm_server::WasmMuxServer>,
}

#[cfg(target_family = "wasm")]
impl Global for WebMuxRuntime {}

#[cfg(target_family = "wasm")]

/// Initialize browser-safe globals and open the real `MultiWorkspace` root.
#[cfg(target_family = "wasm")]
pub fn open_real_workspace(cx: &mut App) {
    settings::init(cx);
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);

    let fs = Arc::new(fs::RealFs::new());
    <dyn fs::Fs>::set_global(fs.clone(), cx);

    let languages = Arc::new(language::LanguageRegistry::new(
        cx.background_executor().clone(),
    ));
    let key_value_store = db::kvp::KeyValueStore::global(cx);

    let (client_stream, server_stream) = mux_protocol::mem_transport::pair();
    let server = mux_server::wasm_server::WasmMuxServer::new(server_stream);
    server.attach_notify();
    let domain = match mux::MuxDomain::connect_in_memory(client_stream) {
        Ok(domain) => Arc::new(domain),
        Err(error) => {
            log::error!("failed to initialize in-browser mux client: {error:#}");
            return;
        }
    };
    cx.set_global(WebMuxRuntime { _server: server });

    cx.spawn(async move |cx| -> Result<()> {
        let session = session::Session::new(uuid::Uuid::new_v4().to_string(), key_value_store).await;
        let session_id = domain.create_session("web", Path::new("/")).await?;
        domain.create_and_attach_window(&session_id).await?;
        let pane_id = domain
            .spawn_pane(
                &session_id,
                "tab-0",
                mux_protocol::TerminalSize {
                    cols: 120,
                    rows: 32,
                },
                None,
                Some(Path::new("/")),
            )
            .await?;

        let app_state = cx.update(|cx| {
            let app_session = cx.new(|cx| session::AppSession::new(session, cx));
            let app_state = Arc::new(workspace::AppState {
                languages,
                fs: fs as Arc<dyn fs::Fs>,
                build_window_options: |_, _| Default::default(),
                session: app_session,
                client: Arc::new(()),
                node_runtime: (),
                user_store: (),
                mux_domain: Some(domain.clone()),
            });
            workspace::AppState::set_global(app_state.clone(), cx);
            workspace::init(app_state.clone(), cx);
            editor::init(cx);
            terminal_view::init(cx);
            app_state
        });

        cx.update(|cx| {
            let project = project::Project::local(
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                Default::default(),
                cx,
            );
            let domain = domain.clone();
            cx.open_window(gpui::WindowOptions::default(), move |window, cx| {
                let workspace = cx.new(|cx| {
                    let mut workspace = workspace::Workspace::new(
                        Some(Default::default()),
                        project,
                        app_state,
                        window,
                        cx,
                    );
                    let pane = workspace.active_pane().clone();
                    pane.update(cx, |pane, _| {
                        pane.set_should_display_welcome_page(false);
                    });
                    let view = cx.new(|cx| {
                        terminal_view::mux_pane::MuxPaneView::new(
                            pane_id,
                            domain,
                            workspace.weak_handle(),
                            workspace.project().downgrade(),
                            window,
                            cx,
                        )
                    });
                    workspace.add_item(
                        pane,
                        Box::new(view),
                        None,
                        true,
                        true,
                        window,
                        cx,
                    );
                    workspace
                });
                let root = cx.new(|cx| {
                    workspace::MultiWorkspace::new(workspace, window, cx)
                });
                root
            })
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
