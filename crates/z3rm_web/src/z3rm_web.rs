//! The z3rm client, in a browser tab.
//!
//! The desktop binary starts a daemon, connects to it over a socket, and opens
//! a `MultiWorkspace` whose panes render the session that daemon owns. This
//! does the same thing with the daemon in the same process (see
//! [`local_server`]) and over the in-memory transport instead of a socket.
//! Everything above that — the workspace, the pane views, the projection of the
//! attach snapshot — is the desktop code, reached through the same entry points.
//!
//! The crate exists only for the browser: on any other target it is empty, so
//! that a workspace build does not have to carry a second set of stubs for a
//! window it will never open.
#![cfg(target_family = "wasm")]

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Entity, Window, WindowHandle};
use std::sync::Arc;
use util::ResultExt as _;

mod local_server;
mod v86_bridge;

pub use local_server::LocalMuxServer;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Holds the tab's server for as long as the tab is open.
///
/// Dropping the server closes the client's end of the pipe, so it has to
/// outlive the window rather than the function that started it.
struct GlobalLocalServer(#[expect(dead_code)] LocalMuxServer);

impl gpui::Global for GlobalLocalServer {}

/// Bring the application up and open the window.
///
/// Mirrors the order the desktop binary uses inside `app.run`: settings and
/// theme first so every `init` below can read them, then the app state every
/// `init` needs, then the chrome, then the window.
pub fn boot(cx: &mut App) {
    let app_version = release_channel::AppVersion::load(env!("CARGO_PKG_VERSION"), None, None);
    release_channel::init(app_version, cx);
    settings::init(cx);
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
    load_embedded_fonts(cx);

    let fs: Arc<dyn fs::Fs> = Arc::new(fs::RealFs::new());
    <dyn fs::Fs>::set_global(fs.clone(), cx);
    git::GitHostingProviderRegistry::set_global(
        Arc::new(git::GitHostingProviderRegistry::new()),
        cx,
    );

    let (server, domain) = match local_server::start() {
        Ok(started) => started,
        Err(error) => {
            log::error!("could not start the in-tab mux server: {error:#}");
            return;
        }
    };
    let mux_server = server.server().clone();
    cx.set_global(GlobalLocalServer(server));

    let app_state = build_app_state(fs, domain.clone(), cx);
    workspace::AppState::set_global(app_state.clone(), cx);

    // §2.1 The chrome the window renders. Everything here is the same crate the
    // desktop registers; the browser simply has fewer of them, because the ones
    // left out drive a filesystem or a subprocess it does not have.
    workspace::init(app_state.clone(), cx);
    editor::init(cx);
    command_palette::init(cx);
    search::init(cx);
    title_bar::init(cx);
    terminal_view::init(cx);
    git_hosting_providers::init(cx);
    git_ui::init(cx);
    recent_projects::init(cx);

    cx.spawn(async move |cx| {
        if let Err(error) = open_window(app_state, domain, mux_server, cx).await {
            log::error!("could not open the z3rm window: {error:#}");
        }
    })
    .detach();
}

fn build_app_state(
    fs: Arc<dyn fs::Fs>,
    domain: Arc<mux::MuxDomain>,
    cx: &mut App,
) -> Arc<workspace::AppState> {
    let key_value_store = db::kvp::KeyValueStore::global(cx);
    let session_id = uuid::Uuid::new_v4().to_string();
    // The browser key-value store is in memory, so this future is already
    // resolved; the desktop blocks on the same call against SQLite.
    let session = cx
        .foreground_executor()
        .block_on(session::Session::new(session_id, key_value_store));
    let session = cx.new(|cx| session::AppSession::new(session, cx));
    let languages = Arc::new(language::LanguageRegistry::new(
        cx.background_executor().clone(),
    ));

    Arc::new(workspace::AppState {
        languages,
        fs,
        build_window_options: |_, _| Default::default(),
        session,
        client: Arc::new(()),
        node_runtime: (),
        user_store: (),
        mux_domain: Some(domain),
    })
}

/// §3.3 Start the tab's server, attach to a session, and render its layout.
async fn open_window(
    app_state: Arc<workspace::AppState>,
    domain: Arc<mux::MuxDomain>,
    mux_server: Arc<mux_server::wasm_server::WasmMuxServer>,
    cx: &mut gpui::AsyncApp,
) -> Result<WindowHandle<workspace::MultiWorkspace>> {
    let (_session_id, attach) = local_server::open_session(&domain).await?;
    let snapshot = workspace::layout_projection::MuxSnapshot::from_attach(&attach);

    // §3.1 The pane has a pty but nothing on the far end of it until the page's
    // emulator is bridged in. A page that never boots one leaves the pane empty,
    // which is the honest rendering of a terminal with no guest.
    if let Some(pane_id) = snapshot.pane_ids.first() {
        if !v86_bridge::attach(&mux_server, pane_id) {
            log::warn!("pane {pane_id} is not on the server; the guest bridge is not connected");
        }
    }

    let open_window = cx.update(|cx| {
        workspace::Workspace::new_local(
            vec![],
            app_state,
            None,
            None,
            Some(Box::new({
                let domain = domain.clone();
                let snapshot = snapshot.clone();
                move |workspace: &mut workspace::Workspace, window, cx| {
                    workspace::layout_projection::install_snapshot_panes(
                        workspace,
                        &snapshot,
                        |workspace, pane_id, window, cx| {
                            Box::new(new_mux_pane_view(
                                pane_id,
                                domain.clone(),
                                workspace,
                                window,
                                cx,
                            ))
                        },
                        window,
                        cx,
                    );
                }
            })),
            workspace::OpenMode::NewWindow,
            cx,
        )
    });
    let open_result = open_window.await.context("opening the workspace window")?;

    // Signal the JS bootstrap that the GPUI canvas is rendering.
    if let Some(root) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.document_element())
    {
        let _ = root.set_attribute("data-gpui-ready", "true");
    }

    Ok(open_result.window)
}

fn new_mux_pane_view(
    pane_id: String,
    domain: Arc<mux::MuxDomain>,
    workspace: &mut workspace::Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<workspace::Workspace>,
) -> Entity<terminal_view::mux_pane::MuxPaneView> {
    let workspace_handle = workspace.weak_handle();
    let project = workspace.project().downgrade();
    cx.new(|cx| {
        terminal_view::mux_pane::MuxPaneView::new(
            pane_id,
            domain,
            workspace_handle,
            project,
            window,
            cx,
        )
    })
}

/// Load the fonts bundled into the wasm module.
///
/// The browser has no system font directory to fall back to, so a missing font
/// here is a blank window rather than a different-looking one.
fn load_embedded_fonts(cx: &App) {
    let asset_source = cx.asset_source();
    let Ok(font_paths) = asset_source.list("fonts") else {
        log::warn!("no embedded fonts directory; text will not render");
        return;
    };
    let mut fonts = Vec::new();
    for font_path in font_paths {
        if !font_path.ends_with(".ttf") {
            continue;
        }
        if let Some(font_bytes) = asset_source.load(&font_path).log_err().flatten() {
            fonts.push(font_bytes);
        }
    }
    cx.text_system().add_fonts(fonts).log_err();
}