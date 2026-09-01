// §16.1 Disable command line from opening on release mode
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod cli_ipc;
mod daemon;
mod diff_review;
mod extension_status_bar;
mod log_viewer;
mod open_diff;
mod open_remote;
mod quickjs_extensions;
mod zed;

/// The desktop surfaces the shared mux window layer through the daemon error
/// toast and the QuickJS extension host's shortcut resolver.
fn desktop_mux_window_hooks() -> MuxWindowHooks {
    MuxWindowHooks {
        show_error: |cx, message| daemon::show_daemon_error(cx, message),
        extension_shortcut_resolver: |cx| {
            cx.try_global::<quickjs_extensions::GlobalHostController>()
                .map(|host| host.0.read(cx).extension_shortcut_resolver())
        },
        route_extension_action: |cx, action_id| {
            let Some(host) = cx.try_global::<quickjs_extensions::GlobalHostController>() else {
                tracing::debug!(action_id, "extension host absent; extension action dropped");
                return;
            };
            host.0.read(cx).execute_command(action_id, "");
        },
    }
}

use std::{
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Context as _;
use assets::Assets;
use crashes::InitCrashHandler;
use fs::{Fs, RealFs};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Application, BorrowAppContext as _, Context, Entity, Global,
    IntoElement, Render, TaskExt, WeakEntity, Window,
};
use gpui_platform;
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use mux_window::DumpAccessibilityTree;
use mux_window::{
    apply_mux_layout_to_workspace, focus_mux_pane_by_id, install_session_sidebar,
    install_snapshot_panes, mark_remote_mux_window_disconnected, mux_binding_for_window,
    mux_domain_for_window, mux_session_for_window, new_mux_pane_view, begin_mux_reconnect,
    finish_mux_reconnect, publish_mux_connection_state, register_core_mux_actions,
    register_mux_window, subscribe_mux_pane_extension_actions, take_mux_window,
    watch_mux_session_notifications, MuxReconnectUnavailable,
    MuxWindowHooks, MuxWindows, MuxConnectionStatusItem, OpenFileRoute, MAX_OPEN_FILE_BYTES,
};
use theme::ThemeRegistry;
use theme_settings::load_user_theme;
use util::ResultExt as _;
use workspace::layout_projection::MuxSnapshot;

use crate::zed::{init as zed_init, watch_settings_files};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ============================================================================
// §16.1 Application 构建
// ============================================================================

fn build_application() -> Application {
    let platform = gpui_platform::current_platform(false);
    // §16.4 Accessibility is on by default (AccessKit). Set Z3RM_A11Y=0 to
    // force-disable for diagnosis of platform integration issues.
    if std::env::var("Z3RM_A11Y").as_deref() == Ok("0") {
        Application::new_inaccessible(platform)
    } else {
        Application::with_platform(platform)
    }
}

// The z3rm_debug actions live in the shared mux_window layer so the browser
// surface registers the same names the bundled keymap references.


// ============================================================================
// §16.7 Extension shortcuts: MuxPaneView resolver installation + event routing
// ============================================================================

/// §16.7 Build a pane view with the extension shortcut resolver installed so
/// declared extension keybindings can match in the pane's priority chain.
/// Without a host (or before any keymap report lands) the resolver is

// ============================================================================
// §3.3 Multiple windows per session (Plan 32)
// ============================================================================

/// §15.4 Mirror `window_id`'s connection state into its status bar item.
/// a real RPC, which is the same probe the local daemon watcher uses.
fn watch_remote_mux_connection(
    domain: Arc<mux::MuxDomain>,
    window_id: gpui::WindowId,
    cx: &mut App,
) -> gpui::Task<()> {
    // Weak for the same reason the notification watcher is: this task must not
    // keep a closed window's tunnel open.
    let domain = Arc::downgrade(&domain);
    cx.spawn(async move |cx| {
        const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        loop {
            cx.background_executor().timer(PROBE_INTERVAL).await;
            let Some(domain) = domain.upgrade() else {
                return;
            };
            if domain.check_connection().await {
                continue;
            }
            let newly_disconnected = cx.update(|cx| {
                if cx.try_global::<MuxWindows>().is_none() {
                    return false;
                }
                let marked = cx.update_global::<MuxWindows, bool>(|windows, _| {
                    mark_remote_mux_window_disconnected(&mut windows.windows, window_id, &domain)
                });
                publish_mux_connection_state(window_id, cx);
                marked
            });
            if newly_disconnected {
                cx.update(|cx| {
                    daemon::show_daemon_error(
                        cx,
                        "Remote mux connection lost. Run mux::Reconnect to rebuild the SSH tunnel.",
                    );
                });
            }
        }
    })
}

/// §3.3 The mux connection that drives `window`.

fn open_file_route(requested_session_id: &str, bound_session_id: Option<&str>) -> OpenFileRoute {
    match bound_session_id {
        None => OpenFileRoute::Unbound,
        Some(bound_session_id) if bound_session_id == requested_session_id => {
            OpenFileRoute::Matched
        }
        Some(_) => OpenFileRoute::WrongSession,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenFileMetadata {
    Readable,
    Missing,
    Directory,
    TooLarge,
}

fn validate_open_file_metadata(exists: bool, is_dir: bool, size: u64) -> OpenFileMetadata {
    if !exists {
        OpenFileMetadata::Missing
    } else if is_dir {
        OpenFileMetadata::Directory
    } else if size > MAX_OPEN_FILE_BYTES {
        OpenFileMetadata::TooLarge
    } else {
        OpenFileMetadata::Readable
    }
}

/// for this very window rather than a snapshot borrowed from another window.
async fn open_mux_window(
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    app_state: Arc<workspace::AppState>,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<gpui::WindowHandle<workspace::MultiWorkspace>> {
    let attach_response = domain.create_and_attach_window(&session_id).await?;
    let snapshot = MuxSnapshot::from_attach(&attach_response);
    open_mux_window_with_snapshot(domain, session_id, snapshot, app_state, None, cx).await
}

/// §3.3 Open a window for a session this domain has already attached to.
///
/// Split out of `open_mux_window` because the bootstrap window needs the
/// snapshot before the window exists: `observe_new` is registered against it.
async fn open_mux_window_with_snapshot(
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    snapshot: MuxSnapshot,
    app_state: Arc<workspace::AppState>,
    ssh_session: Option<mux::SshSession>,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<gpui::WindowHandle<workspace::MultiWorkspace>> {
    let open_result = cx
        .update(|cx| {
            workspace::Workspace::new_local(
                vec![],
                app_state,
                None,
                None,
                Some(Box::new({
                    let domain = domain.clone();
                    let snapshot = snapshot.clone();
                    move |workspace: &mut workspace::Workspace, window, cx| {
                        install_snapshot_panes(workspace, &snapshot, domain, window, cx);
                    }
                })),
                workspace::OpenMode::NewWindow,
                cx,
            )
        })
        .await?;

    let window_handle = open_result.window;
    let is_remote = ssh_session.is_some();
    cx.update(|cx| {
        register_mux_window(
            window_handle.window_id(),
            domain.clone(),
            session_id.clone(),
            ssh_session,
            cx,
        );
        // §15.4 Only remote windows need this: a local daemon window is already
        // covered by `daemon::watch_daemon_connection`, which reconnects on its
        // own instead of waiting for the user.
        if is_remote {
            watch_remote_mux_connection(domain.clone(), window_handle.window_id(), cx).detach();
        }
    });
    window_handle
        .update(cx, |multi_workspace, window, cx| {
            install_session_sidebar(
                multi_workspace,
                domain.clone(),
                session_id.clone(),
                snapshot.session.as_ref(),
                None,
                window,
                cx,
                Rc::new(handle_sidebar_request),
            );
        })
        .log_err();
    watch_mux_session_notifications(domain, session_id, window_handle, cx);
    Ok(window_handle)
}

/// §15.7 Give this window the native mux session tree.
///
fn handle_sidebar_request(
    workspace: &gpui::WeakEntity<workspace::Workspace>,
    domain: &Arc<mux::MuxDomain>,
    request: sidebar::SidebarRequest,
    window: &mut Window,
    cx: &mut App,
) {
    match request {
        sidebar::SidebarRequest::FocusPane(pane_id) => {
            workspace
                .update(cx, |workspace, cx| {
                    focus_mux_pane_by_id(workspace, &pane_id, window, cx);
                })
                .log_err();
        }
        sidebar::SidebarRequest::ActivateSession(session_id) => {
            activate_mux_session(workspace.clone(), domain.clone(), session_id, window, cx);
        }
        sidebar::SidebarRequest::ReviewFile { session_id, path } => {
            let binding = mux_binding_for_window(window, cx);
            match open_file_route(
                &session_id,
                binding
                    .as_ref()
                    .map(|(_, bound_session_id)| bound_session_id.as_str()),
            ) {
                OpenFileRoute::Matched => {
                    let (domain, _) = match binding {
                        Some(binding) => binding,
                        None => return,
                    };
                    let workspace = workspace.clone();
                    window
                        .spawn(cx, async move |cx| {
                            open_diff::open_diff_review(
                                std::path::PathBuf::from(path),
                                domain,
                                session_id,
                                workspace,
                                cx,
                            )
                            .await;
                        })
                        .detach();
                }
                OpenFileRoute::Unbound | OpenFileRoute::WrongSession => {
                    daemon::show_daemon_error(
                        cx,
                        format!(
                            "Cannot review {path}: this window is not attached to session {session_id}"
                        ),
                    );
                }
            }
        }
        sidebar::SidebarRequest::OpenFile { session_id, path } => {
            let binding = mux_binding_for_window(window, cx);
            match open_file_route(
                &session_id,
                binding
                    .as_ref()
                    .map(|(_, bound_session_id)| bound_session_id.as_str()),
            ) {
                OpenFileRoute::Matched => {
                    if let Some((domain, _)) = binding {
                        open_mux_file(workspace.clone(), domain, session_id, path, window, cx);
                    } else {
                        daemon::show_daemon_error(
                            cx,
                            format!(
                                "Cannot open {path}: this window is not attached to a mux session"
                            ),
                        );
                    }
                }
                OpenFileRoute::Unbound => {
                    daemon::show_daemon_error(
                        cx,
                        format!(
                            "Cannot open {path}: this window is not attached to a mux session"
                        ),
                    );
                }
                OpenFileRoute::WrongSession => {
                    if let Some((_, bound_session_id)) = binding {
                        daemon::show_daemon_error(
                            cx,
                            format!(
                                "Cannot open {path}: Files requested session {session_id}, but this window is attached to {bound_session_id}; select the file from this window's current Files browser"
                            ),
                        );
                    } else {
                        daemon::show_daemon_error(
                            cx,
                            format!(
                                "Cannot open {path}: this window is not attached to a mux session"
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// What a bounded session-file read produced.
enum MuxFileContent {
    Text(Vec<u8>),
    /// The bytes are intact, they just are not text. Kept so the viewer can
    /// offer to save them instead of throwing the transfer away.
    Binary(Vec<u8>),
}

async fn read_mux_file_bounded(
    domain: &mux::MuxDomain,
    path: &str,
    expected_size: u64,
) -> anyhow::Result<MuxFileContent> {
    anyhow::ensure!(
        expected_size <= MAX_OPEN_FILE_BYTES,
        "{path} exceeds the 2 MiB Files viewer limit"
    );

    let mut content = Vec::with_capacity(expected_size as usize);
    let mut offset_bytes = 0;
    let mut is_binary = false;
    loop {
        let remaining_capacity = MAX_OPEN_FILE_BYTES.saturating_sub(offset_bytes);
        let max_bytes = remaining_capacity
            .max(1)
            .min(mux_protocol::DEFAULT_READ_FILE_PAGE_BYTES as u64)
            as u32;
        let page = domain
            .read_file_page(path, offset_bytes, max_bytes)
            .await
            .with_context(|| format!("mux read_file RPC failed for {path}"))?;

        anyhow::ensure!(
            page.total_bytes == expected_size,
            "{path} changed size while opening (stat reported {expected_size} bytes, read reported {} bytes); refresh Files and try again",
            page.total_bytes
        );
        is_binary |= page.is_binary;
        anyhow::ensure!(
            content.len() + page.content.len() <= MAX_OPEN_FILE_BYTES as usize,
            "{path} exceeds the 2 MiB Files viewer limit"
        );
        content.extend_from_slice(&page.content);

        let Some(next_offset_bytes) = page.next_offset_bytes else {
            return Ok(if is_binary {
                MuxFileContent::Binary(content)
            } else {
                MuxFileContent::Text(content)
            });
        };
        offset_bytes = next_offset_bytes;
    }
}

/// §16.6 Describe a binary session file and offer to save it locally.
///
/// Refusing to render it as text is right; refusing to do anything with it is
/// not — the bytes already crossed the tunnel, and copying them out is the one
/// thing the user can still reasonably want.
async fn offer_binary_file_download(
    path: String,
    content: Vec<u8>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    let file_name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let detail = format!(
        "{path}\n\n{} bytes of binary data.\n\nBinary files are not shown in the text viewer. \
         The contents have already been fetched from the session host and can be saved locally.",
        content.len()
    );
    let answer = cx
        .update(|window, cx| {
            window.prompt(
                gpui::PromptLevel::Info,
                "Binary file",
                Some(&detail),
                &[
                    gpui::PromptButton::new("Save a copy…"),
                    gpui::PromptButton::cancel("Close"),
                ],
                cx,
            )
        })?
        .await
        .ok();
    if answer != Some(0) {
        return Ok(());
    }

    let directory = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let destination = cx
        .update(|_, cx| cx.prompt_for_new_path(&directory, Some(&file_name)))?
        .await
        .context("the save dialog was dismissed")??;
    let Some(destination) = destination else {
        return Ok(());
    };
    let display = destination.display().to_string();
    smol::unblock(move || std::fs::write(&destination, &content))
        .await
        .with_context(|| format!("failed to write {display}"))?;
    cx.update(|_, cx| daemon::show_daemon_error(cx, format!("Saved {display}")))?;
    Ok(())
}

fn open_mux_file(
    workspace: gpui::WeakEntity<workspace::Workspace>,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    path: String,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let error_path = path.clone();
            let result: anyhow::Result<()> = async {
                let stat = domain
                    .stat_file(&path)
                    .await
                    .with_context(|| format!("mux stat_file RPC failed for {path}"))?;
                match validate_open_file_metadata(stat.exists, stat.is_dir, stat.size) {
                    OpenFileMetadata::Readable => {}
                    OpenFileMetadata::Missing => {
                        anyhow::bail!(
                            "{path} no longer exists; refresh Files and choose an existing file"
                        )
                    }
                    OpenFileMetadata::Directory => {
                        anyhow::bail!("{path} is a directory; choose a file from Files")
                    }
                    OpenFileMetadata::TooLarge => {
                        anyhow::bail!(
                            "{path} is {} bytes; the Files viewer limit is 2 MiB",
                            stat.size
                        )
                    }
                }

                let content = match read_mux_file_bounded(&domain, &path, stat.size).await? {
                    MuxFileContent::Text(content) => content,
                    MuxFileContent::Binary(content) => {
                        return offer_binary_file_download(path.clone(), content, cx).await;
                    }
                };
                let text = String::from_utf8(content).map_err(|error| {
                    anyhow::anyhow!(
                        "{path} is not valid UTF-8 (invalid data begins at byte {}); convert it to UTF-8 on the session host and try again",
                        error.utf8_error().valid_up_to()
                    )
                })?;

                let current_session_id =
                    cx.update(|window, cx| mux_session_for_window(window, cx))?;
                match open_file_route(&session_id, current_session_id.as_deref()) {
                    OpenFileRoute::Matched => {}
                    OpenFileRoute::Unbound => {
                        anyhow::bail!(
                            "cannot open {path}: this window is no longer attached to a mux session"
                        )
                    }
                    OpenFileRoute::WrongSession => {
                        anyhow::bail!(
                            "cannot open {path}: this window switched away from session {session_id}; choose the file from the current session's Files browser"
                        )
                    }
                }

                let language_registry = workspace.read_with(cx, |workspace, cx| {
                    workspace.project().read(cx).languages().clone()
                })?;
                let language =
                    if let Some(available) = language_registry.language_for_file_path(Path::new(&path))
                    {
                        Some(
                            language_registry
                                .load_language(&available)
                                .await
                                .with_context(|| {
                                    format!("language detection was canceled for {path}")
                                })?
                                .with_context(|| {
                                    format!("failed to load the detected language for {path}")
                                })?,
                        )
                    } else {
                        None
                    };

                let current_session_id =
                    cx.update(|window, cx| mux_session_for_window(window, cx))?;
                anyhow::ensure!(
                    open_file_route(&session_id, current_session_id.as_deref())
                        == OpenFileRoute::Matched,
                    "cannot open {path}: the window's mux session changed while loading the file"
                );

                workspace.update_in(cx, move |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let buffer = cx.new(|cx| {
                        let mut buffer = language::Buffer::local(text, cx);
                        buffer.set_language_registry(language_registry);
                        buffer.set_capability(language::Capability::ReadOnly, cx);
                        if let Some(language) = language {
                            buffer.set_language_async(Some(language), cx);
                        }
                        buffer
                    });
                    let editor =
                        cx.new(|cx| editor::Editor::for_buffer(buffer, Some(project), window, cx));
                    let editor_buffer = editor.read(cx).buffer().clone();
                    editor_buffer.update(cx, |buffer, cx| {
                        buffer.set_title(path.clone(), cx);
                    });
                    workspace.add_item_to_active_pane(
                        Box::new(editor),
                        None,
                        true,
                        window,
                        cx,
                    );
                })?;
                Ok(())
            }
            .await;

            if let Err(error) = result {
                tracing::error!(session_id, path = error_path, %error, "opening a mux session file failed");
                cx.update(|_, cx| {
                    daemon::show_daemon_error(cx, format!("Failed to open {error_path}: {error}"));
                })
                .log_err();
            }
        })
        .detach();
}

/// §15.4 / §15.12 Attach `session_id` and reproject its authoritative layout
/// into this window.
///
/// Attach is the snapshot RPC, so this doubles as the resync path when the
/// requested session is the one already rendered.
fn activate_mux_session(
    workspace: gpui::WeakEntity<workspace::Workspace>,
    domain: Arc<mux::MuxDomain>,
    session_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let window_id = window.window_handle().window_id();
    window
        .spawn(cx, async move |cx| {
            let result: anyhow::Result<()> = async {
                let response = domain.attach(&session_id, mux::AttachMode::Shared).await?;
                let snapshot = response
                    .snapshot
                    .clone()
                    .context("attach response contained no session snapshot")?;
                let proto_layout = snapshot
                    .layout
                    .clone()
                    .context("attached session contained no layout")?;
                let layout = workspace::layout_projection::LayoutTree::from_proto(&proto_layout);
                let focused_pane = (!snapshot.focused_pane_id.is_empty())
                    .then(|| snapshot.focused_pane_id.clone());
                workspace.update_in(cx, {
                    let domain = domain.clone();
                    move |workspace, window, cx| {
                        apply_mux_layout_to_workspace(
                            workspace,
                            &layout,
                            focused_pane.as_deref(),
                            domain,
                            window,
                            cx,
                        );
                    }
                })?;
                cx.update(|window, cx| {
                    register_mux_window(window_id, domain.clone(), session_id.clone(), None, cx);
                    // The sidebar's tree is bound to one session, so switching
                    // rebinds it while keeping the user's chosen width.
                    let Some(multi_workspace) =
                        window.root::<workspace::MultiWorkspace>().flatten()
                    else {
                        return;
                    };
                    let previous_width = multi_workspace
                        .read(cx)
                        .sidebar()
                        .map(|sidebar| sidebar.width(cx));
                    multi_workspace.update(cx, |multi_workspace, cx| {
                        install_session_sidebar(
                            multi_workspace,
                            domain.clone(),
                            session_id.clone(),
                            Some(&snapshot),
                            previous_width,
                            window,
                            cx,
                            Rc::new(handle_sidebar_request),
                        );
                    });
                })?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                tracing::error!(%error, "activating a mux session from the sidebar failed");
                cx.update(|_, cx| {
                    daemon::show_daemon_error(
                        cx,
                        format!("Failed to switch to mux session: {error}"),
                    )
                })
                .log_err();
            }
        })
        .detach();
}

/// §15.4 The authoritative focus that rides along with a layout change.
///
/// The snapshot travels with the layout, so a reconnect that rebuilt panes and

// ============================================================================
// §16.1 Font 加载
// ============================================================================

fn load_embedded_fonts(cx: &App) {
    let asset_source = cx.asset_source();
    let Ok(font_paths) = asset_source.list("fonts") else {
        tracing::warn!("embedded fonts directory not found, skipping font loading");
        return;
    };
    let embedded_fonts = Arc::new(Mutex::new(Vec::new()));
    let executor = cx.background_executor();

    cx.foreground_executor().block_on(executor.scoped(|scope| {
        for font_path in &font_paths {
            if !font_path.ends_with(".ttf") {
                continue;
            }

            let font_path = font_path.clone();
            let embedded_fonts = embedded_fonts.clone();
            scope.spawn(async move {
                match asset_source.load(&font_path) {
                    Ok(Some(bytes)) => {
                        embedded_fonts.lock().push(bytes);
                    }
                    Ok(None) => {
                        tracing::warn!(path = %font_path, "font file not found");
                    }
                    Err(e) => {
                        tracing::error!(path = %font_path, error = ?e, "failed to load font");
                    }
                }
            });
        }
    }));
    if let Err(e) = cx.text_system().add_fonts(embedded_fonts.lock().to_vec()) {
        tracing::error!(error = ?e, "failed to add embedded fonts to text system");
    }
}

// ============================================================================
// §16.1 Theme 加载
// ============================================================================

/// 后台加载用户主题 (§16.1)
fn load_user_themes_in_background(fs: Arc<dyn Fs>, cx: &mut App) {
    cx.spawn({
        let fs = fs.clone();
        async move |cx| {
            let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
            let themes_dir = paths::themes_dir().as_ref();
            match fs
                .metadata(themes_dir)
                .await
                .ok()
                .flatten()
                .map(|m| m.is_dir)
            {
                Some(is_dir) => {
                    anyhow::ensure!(is_dir, "Themes dir path {themes_dir:?} is not a directory")
                }
                None => {
                    fs.create_dir(themes_dir).await.with_context(|| {
                        format!("Failed to create themes dir at path {themes_dir:?}")
                    })?;
                }
            }

            let mut theme_paths = fs
                .read_dir(themes_dir)
                .await
                .with_context(|| format!("reading themes from {themes_dir:?}"))?;

            while let Some(theme_path) = theme_paths.next().await {
                let Some(theme_path) = theme_path.log_err() else {
                    continue;
                };
                let Some(bytes) = fs.load_bytes(&theme_path).await.log_err() else {
                    continue;
                };

                load_user_theme(&theme_registry, &bytes).log_err();
            }

            cx.update(theme_settings::reload_theme);
            anyhow::Ok(())
        }
    })
    .detach_and_log_err(cx);
}

/// 监听主题目录变更 (§16.1)
fn watch_themes(fs: Arc<dyn Fs>, cx: &mut App) {
    use std::time::Duration;
    cx.spawn(async move |cx| {
        let (mut events, _) = fs
            .watch(paths::themes_dir(), Duration::from_millis(100))
            .await;

        while let Some(paths) = events.next().await {
            for event in paths {
                if fs
                    .metadata(&event.path)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|m| !m.is_dir)
                {
                    let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
                    if let Some(bytes) = fs.load_bytes(&event.path).await.log_err() {
                        if load_user_theme(&theme_registry, &bytes).log_err().is_some() {
                            cx.update(theme_settings::reload_theme);
                        }
                    }
                }
            }
        }
    })
    .detach()
}

/// §5.6 Present the first-install consent prompt for the controller's current
/// pending approvals, unless another window already claimed the batch.
///
/// Prompt results are handled exactly: `Ok(0)` approves, `Ok(1)` denies;
/// cancellation, an error, or any other value releases the claim and leaves
/// the requests pending. Approve/deny persistence errors are surfaced to the
/// user and also leave the requests pending.
fn check_pending_consent_prompt(
    host: &gpui::Entity<quickjs_extensions::ExtensionHostController>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<workspace::Workspace>,
) {
    let pending = host.read(cx).pending_approvals();
    if pending.is_empty() {
        return;
    }
    // Only one prompt may own the pending batch process-wide; the claim is
    // made atomically on the controller, so a second workspace cannot race.
    if !host.update(cx, |host, cx| host.claim_pending_prompt(cx)) {
        return;
    }
    let details = pending
        .iter()
        .map(|approval| {
            format!(
                "{} v{} — capabilities: {}",
                approval.id, approval.version, approval.capabilities_summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ids: Vec<String> = pending.iter().map(|approval| approval.id.clone()).collect();
    let host = host.clone();
    window.spawn(cx, async move |cx| {
        let answer = cx
            .prompt(
                gpui::PromptLevel::Info,
                "New extension wants to run",
                Some(&details),
                &["Allow", "Deny"],
            )
            .await;
        match answer {
            Ok(0) => {
                if let Err(error) = apply_prompt_decision(&host, cx, true, &ids) {
                    tracing::error!(%error, "approving extensions failed; requests stay pending");
                    surface_consent_error(cx, "Could not approve extension", &error);
                }
            }
            Ok(1) => {
                if let Err(error) = apply_prompt_decision(&host, cx, false, &ids) {
                    tracing::error!(%error, "denying extensions failed; requests stay pending");
                    surface_consent_error(cx, "Could not deny extension", &error);
                }
            }
            // Cancellation, a prompt error, or an unexpected button index:
            // no decision was made — release the claim and keep the requests
            // pending so a later check can present them again.
            _ => {}
        }
        host.update(cx, |host, cx| host.release_pending_prompt(cx));
    })
    .detach();
}

/// Apply the user's prompt decision on the controller. The controller
/// persists the proposed consent records before touching pending state or
/// sending host commands, so a persistence error propagates here with
/// pending/host state unchanged.
fn apply_prompt_decision(
    host: &gpui::Entity<quickjs_extensions::ExtensionHostController>,
    cx: &mut gpui::AsyncWindowContext,
    approve: bool,
    ids: &[String],
) -> anyhow::Result<()> {
    host.update(cx, |host, cx| {
        if approve {
            host.approve_extensions(ids, cx)
        } else {
            host.deny_extensions(ids)
        }
    })
}

/// §5.6 Surface a failed consent persistence to the user as an error toast.
fn surface_consent_error(cx: &mut gpui::AsyncWindowContext, action: &str, error: &anyhow::Error) {
    if let Err(error) = cx.update(|_window, cx| {
        daemon::show_daemon_error(cx, format!("{action}: {error}"));
    }) {
        tracing::debug!(%error, "window closed before the consent error toast could be shown");
    }
}

/// Process-global guard for the startup recovery prompt. A startup task can
/// race with another window's startup task, but only one may own the prompt.
static RECOVERY_PROMPT_CLAIMED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct StartupRecoveryPrompt;

impl Render for StartupRecoveryPrompt {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        gpui::div()
    }
}

enum StartupRecovery {
    Continue {
        warning: Option<String>,
    },
    Restored {
        session_id: String,
        snapshot: MuxSnapshot,
    },
}

fn recovery_prompt_detail(
    candidates: &[mux_protocol::RecoveryCandidateInfo],
    rejected: &[String],
) -> String {
    let mut detail = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{}\n  cwd: {}\n  panes: {}\n  metadata: {}",
                candidate.name,
                candidate.cwd,
                candidate.pane_ids.len(),
                if candidate.metadata_complete {
                    "complete"
                } else {
                    "incomplete"
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    // A rejected row is a session the user had and cannot get back. Saying so
    // beats letting it look like it was never persisted at all.
    if !rejected.is_empty() {
        detail.push_str(&format!(
            "\n\n{} saved session(s) could not be read and are not offered:\n  {}",
            rejected.len(),
            rejected.join("\n  ")
        ));
    }
    detail.push_str(
        "\n\nOnly the saved layout and new default shells are recreated. \
         Running processes and old commands are never restored or replayed.",
    );
    detail
}

fn should_probe_recovery(attach_target: Option<&str>) -> bool {
    attach_target.is_none()
}

async fn maybe_recover_mux_session(
    domain: &Arc<mux::MuxDomain>,
    attach_target: Option<&str>,
    cx: &mut gpui::AsyncApp,
) -> StartupRecovery {
    if !should_probe_recovery(attach_target) {
        return StartupRecovery::Continue { warning: None };
    }

    let listing = match domain.list_recovery_candidates().await {
        Ok(listing) if !listing.candidates.is_empty() => listing,
        // Nothing offerable. Rejected rows alone do not justify interrupting
        // startup with a prompt that has nothing to confirm.
        Ok(listing) => {
            for rejected in &listing.rejected {
                tracing::warn!(%rejected, "unreadable mux recovery candidate");
            }
            return StartupRecovery::Continue { warning: None };
        }
        Err(error) => {
            return StartupRecovery::Continue {
                warning: Some(format!(
                    "Could not inspect mux recovery candidates: {error}. Continuing with normal startup."
                )),
            };
        }
    };
    let candidates = listing.candidates;

    if RECOVERY_PROMPT_CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StartupRecovery::Continue { warning: None };
    }

    let release_claim = || {
        RECOVERY_PROMPT_CLAIMED.store(false, Ordering::Release);
    };
    let prompt_window = match cx.open_window(gpui::WindowOptions::default(), |_, cx| {
        cx.new(|_| StartupRecoveryPrompt)
    }) {
        Ok(window) => window,
        Err(error) => {
            release_claim();
            return StartupRecovery::Continue {
                warning: Some(format!(
                    "Could not open the mux recovery prompt: {error}. Continuing with normal startup."
                )),
            };
        }
    };

    let detail = recovery_prompt_detail(&candidates, &listing.rejected);
    let mut answers = candidates
        .iter()
        .map(|candidate| gpui::PromptButton::new(format!("Restore {}", candidate.name)))
        .collect::<Vec<_>>();
    answers.push(gpui::PromptButton::cancel("Skip"));
    let answer = match prompt_window.update(cx, |_, window, cx| {
        window.prompt(
            gpui::PromptLevel::Warning,
            "Recover a previous mux session?",
            Some(&detail),
            &answers,
            cx,
        )
    }) {
        Ok(receiver) => receiver.await.ok(),
        Err(_) => None,
    };
    let _ = prompt_window.update(cx, |_, window, _| window.remove_window());

    let Some(answer) = answer else {
        return StartupRecovery::Continue {
            warning: Some(
                "The mux recovery prompt was cancelled. Continuing with normal startup."
                    .to_string(),
            ),
        };
    };
    let Some(candidate) = candidates.get(answer).cloned() else {
        return StartupRecovery::Continue { warning: None };
    };

    let confirmed = match domain.confirm_recovery(&candidate.id).await {
        Ok(confirmed) => confirmed,
        Err(error) => {
            return StartupRecovery::Continue {
                warning: Some(format!(
                    "Could not restore mux session '{}' (it may be stale, incomplete, or already restored): {error}. Continuing with normal startup.",
                    candidate.name
                )),
            };
        }
    };
    if confirmed.session_id != candidate.id || confirmed.session_id.is_empty() {
        return StartupRecovery::Continue {
            warning: Some(format!(
                "Mux recovery returned an invalid session for '{}'. Continuing with normal startup.",
                candidate.name
            )),
        };
    }

    let attach_response = match domain.create_and_attach_window(&confirmed.session_id).await {
        Ok(response) => response,
        Err(error) => {
            return StartupRecovery::Continue {
                warning: Some(format!(
                    "Mux session '{}' was recovered but could not be attached: {error}. Continuing with normal startup.",
                    candidate.name
                )),
            };
        }
    };
    StartupRecovery::Restored {
        session_id: confirmed.session_id,
        snapshot: MuxSnapshot::from_attach(&attach_response),
    }
}

// ============================================================================
// §16.1 main: GPUI 应用启动 → daemon → window
// ============================================================================

fn main() {
    // §16.1 沙盒与权限检查
    sandbox::run_sandbox_launcher_if_invoked();

    let args: Vec<String> = std::env::args().collect();
    if let Some(socket) = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--crash-handler").then_some(pair[1].as_str()))
    {
        crashes::crash_server(Path::new(socket), paths::logs_dir().clone());
        std::process::exit(0);
    }

    let startup_open_url = args
        .iter()
        .skip(1)
        .find(|argument| cli_ipc::is_open_url(argument))
        .cloned();
    if let Some(data_dir) = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--user-data-dir").then_some(pair[1].as_str()))
    {
        paths::set_custom_data_dir(data_dir);
    }

    // §3.10 / §16.6 `attach` (local or `--ssh`) is the mux CLI command that
    // opens a GUI. The CLI process still returns immediately: it launches a
    // fresh GUI process with the target / SSH URI carried in environment
    // variables (never argv, and never echoed to the user), prints a
    // confirmation, and exits.
    let attach_target = if std::env::var_os("Z3RM_GUI_ATTACH").is_some() {
        std::env::var("Z3RM_ATTACH_TARGET").ok()
    } else if startup_open_url.is_some() {
        None
    } else {
        if let Some(intent) = cli::parse_launch_intent_from(&args) {
            let executable = std::env::current_exe().unwrap_or_else(|error| {
                eprintln!("error: failed to locate z3rm executable: {error}");
                std::process::exit(1);
            });
            // The parent exits immediately, so anything the GUI writes on its
            // way down has to land somewhere durable. Sending it to /dev/null
            // makes a GUI that dies on startup indistinguishable from one that
            // came up fine.
            // Two independent append handles rather than one plus `try_clone`:
            // both writers land in the same file without sharing a cursor.
            let attach_log_path = paths::logs_dir().join("attach.log");
            let open_attach_log = || {
                std::fs::create_dir_all(paths::logs_dir()).and_then(|()| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&attach_log_path)
                })
            };

            let mut command = std::process::Command::new(executable);
            command
                .env("Z3RM_GUI_ATTACH", "1")
                .stdin(std::process::Stdio::null());
            match (open_attach_log(), open_attach_log()) {
                (Ok(stdout), Ok(stderr)) => {
                    command
                        .stdout(std::process::Stdio::from(stdout))
                        .stderr(std::process::Stdio::from(stderr));
                }
                (Err(error), _) | (_, Err(error)) => {
                    eprintln!(
                        "warning: failed to open {}: {error}; GUI output will be discarded",
                        attach_log_path.display()
                    );
                    command
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                }
            }
            match intent {
                cli::LaunchIntent::Gui { target } => {
                    if let Some(target) = &target {
                        command.env("Z3RM_ATTACH_TARGET", target);
                    }
                    command.spawn().unwrap_or_else(|error| {
                        eprintln!("error: failed to launch z3rm GUI: {error}");
                        std::process::exit(1);
                    });
                    eprintln!(
                        "z3rm: attached to session '{}' in GUI window (log: {})",
                        target.as_deref().unwrap_or("default"),
                        attach_log_path.display()
                    );
                    std::process::exit(0);
                }
                cli::LaunchIntent::Ssh { uri } => {
                    // §16.6 The URI rides in the child's environment only: it
                    // must not appear in argv or in the launcher's output.
                    command.env("Z3RM_SSH_URI", &uri);
                    command.spawn().unwrap_or_else(|error| {
                        eprintln!("error: failed to launch z3rm GUI: {error}");
                        std::process::exit(1);
                    });
                    eprintln!(
                        "z3rm: connecting to remote mux_server in a GUI window (log: {})",
                        attach_log_path.display()
                    );
                    std::process::exit(0);
                }
            }
        }

        let cli_cmd = match cli::parse_cli_args_from(&args) {
            Ok(cmd) => cmd,
            Err(error) if error == cli::HELP_REQUESTED => {
                print!("{}", cli::format_usage());
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(2);
            }
        };
        if let Some(cmd) = cli_cmd {
            let runtime =
                tokio::runtime::Runtime::new().expect("failed to create tokio runtime for CLI");
            if let Err(error) = runtime.block_on(async { cli::run_cli_command(cmd).await }) {
                // `{error}` 只印最外层那句 context, 服务端给的真正原因全被吞掉:
                // 一次越界路径会显示成 "failed to read <path>" 而不是 "path may
                // not contain parent traversal"。`{error:#}` 把整条 anyhow 链印出来。
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }

        if let Ok(Some(extension_args)) = cli::marketplace::parse_extension_args() {
            let runtime = tokio::runtime::Runtime::new()
                .expect("failed to create tokio runtime for extension CLI");
            if let Err(error) = runtime
                .block_on(async { cli::marketplace::run_extension_command(extension_args).await })
            {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        None
    };

    // §16.6 SSH attach URI, carried from the CLI launcher via the child's
    // environment. Only present in the GUI child process; never logged.
    let ssh_uri = if std::env::var_os("Z3RM_GUI_ATTACH").is_some() {
        std::env::var("Z3RM_SSH_URI").ok()
    } else {
        None
    };

    #[cfg(unix)]
    util::prevent_root_execution();

    // Auto-detect Wayland display if not set (common in tmux sessions)
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let socket_path = std::path::PathBuf::from(&runtime_dir).join("wayland-0");
            if socket_path.exists() {
                // Safe: called before any threads are spawned
                unsafe { std::env::set_var("WAYLAND_DISPLAY", "wayland-0") };
            }
        }
    }

    ztracing::init();

    // §16.1 版本信息
    let version = option_env!("Z3RM_BUILD_ID");
    let app_commit_sha =
        option_env!("Z3RM_COMMIT_SHA").map(|commit_sha| AppCommitSha::new(commit_sha.to_string()));
    let app_version = AppVersion::load(env!("CARGO_PKG_VERSION"), version, app_commit_sha.clone());

    tracing::info!(
        "========== starting z3rm version {}, sha {} ==========",
        app_version,
        app_commit_sha
            .as_ref()
            .map(|sha| sha.short())
            .as_deref()
            .unwrap_or("unknown"),
    );

    let (open_url_sender, mut open_url_receiver) = cli_ipc::open_url_channel();
    if let Some(url) = startup_open_url {
        open_url_sender.send(url);
    }

    let app = build_application().with_assets(Assets);
    app.on_open_urls({
        let open_url_sender = open_url_sender.clone();
        move |urls| {
            for url in urls {
                open_url_sender.send(url);
            }
        }
    });
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if let Err(error) = cli_ipc::listen_for_cli_connections(open_url_sender.clone()) {
        tracing::warn!(error = %error, "failed to start installed-CLI listener");
    }
    let background_executor = app.background_executor();

    // §16.1 Crash handler
    let should_install_crash_handler = matches!(
        std::env::var("Z3RM_GENERATE_MINIDUMPS").as_deref(),
        Ok("true" | "1")
    ) || *release_channel::RELEASE_CHANNEL
        != ReleaseChannel::Dev;

    let crash_handler = if should_install_crash_handler {
        Some(
            background_executor.spawn(crashes::init(
                InitCrashHandler {
                    session_id: String::new(),
                    zed_version: format!(
                        "{}.{}.{}",
                        app_version.major, app_version.minor, app_version.patch
                    ),
                    binary: "z3rm".to_string(),
                    release_channel: release_channel::RELEASE_CHANNEL_NAME.clone(),
                    commit_sha: app_commit_sha
                        .as_ref()
                        .map(|sha| sha.full())
                        .unwrap_or_else(|| "no sha".to_owned()),
                },
                {
                    let background_executor = background_executor.clone();
                    move |task| {
                        background_executor.spawn(task).detach();
                    }
                },
                |pid| paths::temp_dir().join(format!("z3rm-crash-handler-{pid}")),
                {
                    let background_executor = background_executor.clone();
                    move |duration| background_executor.timer(duration)
                },
            )),
        )
    } else {
        crashes::force_backtrace();
        None
    };

    let fs = Arc::new(RealFs::new(None, background_executor.clone()));

    app.run(move |cx| {
        cx.set_global(db::AppDatabase::new());
        release_channel::init(app_version.clone(), cx);
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
        zed_init(cx);
        settings::bind_startup_keymaps(cx);
        watch_settings_files(fs.clone(), cx);

        load_embedded_fonts(cx);
        load_user_themes_in_background(fs.clone(), cx);
        watch_themes(fs.clone(), cx);

        if let Some(crash_handler) = crash_handler {
            cx.spawn(async move |_| {
                let _client = crash_handler.await;
                drop(_client);
            })
            .detach();
        }

        // §16.1 / §2.1 创建 AppState (同步,在 app.run 内，让所有 ::init 可调用)
        let kv_store = db::kvp::KeyValueStore::global(cx);
        let session_id = db::uuid::Uuid::new_v4().to_string();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for session init");
        let session = rt.block_on(session::Session::new(session_id, kv_store));
        let app_state = {
            let es: Entity<session::AppSession> = cx.new(|cx| session::AppSession::new(session, cx));
            let languages = Arc::new(language::LanguageRegistry::new(
                cx.background_executor().clone(),
            ));
            let app_state = Arc::new(workspace::AppState {
                languages,
                fs: fs.clone() as Arc<dyn fs::Fs>,
                build_window_options: |_, _| Default::default(),
                session: es,
                client: Arc::new(()),
                node_runtime: (),
                user_store: (),
                mux_domain: None,
            });
            workspace::AppState::set_global(app_state.clone(), cx);
            app_state
        };

        let app_state_for_cli = app_state.clone();
        cx.spawn(async move |cx| {
            while let Some(url) = open_url_receiver.next().await {
                if let Err(error) =
                    cli_ipc::handle_open_url(url, app_state_for_cli.clone(), cx).await
                {
                    tracing::error!(error = %error, "installed CLI request failed");
                }
            }
        })
        .detach();
        // §2.1 Backport all Zed UI chrome ::init calls (not in spec remove-list).
        // §2.1 Globals required by chrome ::init calls.
        // Fs and GitHostingProviderRegistry must exist before any git/git_ui
        // panel queries them via cx.global::<>().
        <dyn fs::Fs>::set_global(fs.clone(), cx);
        let git_hosting_provider_registry =
            Arc::new(git::GitHostingProviderRegistry::new());
        git::GitHostingProviderRegistry::set_global(git_hosting_provider_registry, cx);

        workspace::init(app_state.clone(), cx);
        open_diff::init(cx);
        open_remote::init(cx);
        editor::init(cx);
        command_palette::init(cx);
        file_finder::init(cx);
        tab_switcher::init(cx);
        project_panel::init(cx);
        search::init(cx);
        title_bar::init(cx);
        terminal_view::init(cx);
        // §5.2 QuickJS extension system — loads JS extensions on background thread
        quickjs_extensions::init_extensions(cx);
        settings_ui::init(cx);
        settings_profile_selector::init(cx);
        theme_selector::init(cx);
        language_selector::init(cx);
        keymap_editor::init(cx);
        line_ending_selector::init(cx);
        go_to_line::init(cx);
        encoding_selector::init(cx);
        git_hosting_providers::init(cx);
        git_ui::init(cx);
        recent_projects::init(cx);
        which_key::init(cx);
        zlog_settings::init(cx);

        // §16.1 daemon 自动启动 → 连接 → session → pane → 窗口
        // §16.6 `attach --ssh <uri>` 不启动本地 daemon: connect_ssh 建立隧道,
        // 返回的 SshSession 由窗口持有 (见 MuxWindow), 直到窗口关闭才释放。
        cx.spawn(async move |cx| {
            eprintln!("[z3rm] Starting mux connection flow");
            let (domain, ssh_session) = match ssh_uri {
                Some(uri) => {
                    let (domain, session) = mux::connect_ssh(&uri)
                        .await
                        .context("failed to connect via SSH. Ensure the remote host has an OpenSSH client and is reachable.")?;
                    (Arc::new(domain), Some(session))
                }
                None => (Arc::new(daemon::ensure_daemon_running().await?), None),
            };
            eprintln!("[z3rm] Daemon connected");

            // §16.12 Push the user's server-side extensions to the mux
            // server (SSH tunnel or local daemon) so path/git-installed
            // extensions are available remotely too. Fire-and-forget: a
            // sync failure must never block the attach, and validation
            // stays server-side — the install endpoint rejects bad
            // manifests, client-only sides, and unsafe archives (§16.6).
            {
                let sync_domain = domain.clone();
                let sync_dir = paths::extensions_dir().clone();
                cx.spawn(async move |_| {
                    if let Err(error) =
                        mux::sync_extensions_to_remote(&sync_domain, &sync_dir).await
                    {
                        tracing::warn!(
                            %error,
                            "syncing local extensions to the mux server failed; extensions still run locally"
                        );
                    }
                })
                .detach();
            }

            // §3.6 Startup recovery is offered before the normal default
            // session path. Explicit CLI targets always win and bypass it.
            let (session_id, snapshot, startup_warning) =
                match maybe_recover_mux_session(&domain, attach_target.as_deref(), cx).await {
                    StartupRecovery::Restored {
                        session_id,
                        snapshot,
                    } => {
                        eprintln!("[z3rm] Recovered session: {}", session_id);
                        (session_id, snapshot, None)
                    }
                    StartupRecovery::Continue { warning } => {
                        // §3.10 GUI attach target: `attach [-t target]` takes
                        // precedence; without one this is the normal
                        // create-or-reuse default session path.
                        let session_id = daemon::ensure_target_session(
                            &domain,
                            attach_target.as_deref(),
                        )
                        .await?;
                        eprintln!("[z3rm] Session: {}", session_id);

                        daemon::ensure_pane_in_session(&domain, &session_id).await?;
                        eprintln!("[z3rm] Pane ensured");
                        // §3.3 Attach with a server-minted window id so this GUI
                        // window is a distinct session member (Plan 32).
                        let attach_resp = domain.create_and_attach_window(&session_id).await?;
                        let snapshot = MuxSnapshot::from_attach(&attach_resp);
                        (session_id, snapshot, warning)
                    }
                };
            eprintln!(
                "[z3rm] Attached to session ({} panes in snapshot)",
                snapshot.pane_ids.len()
            );

            // §3.2 把 domain 注入 AppState. AppState 是 Arc<AppState>,
            // 替换整个 Arc 让后续代码 (含未来的 workspace::Open 路径) 能拿到。
            let domain_for_state = domain.clone();
            cx.update(|cx| {
                let updated = workspace::AppState::try_global(cx).map(|state| {
                    let mut next = state.as_ref().clone();
                    next.mux_domain = Some(domain_for_state.clone());
                    Arc::new(next)
                });
                if let Some(next) = updated {
                    workspace::AppState::set_global(next, cx);
                }
            });

            // §3.8/§15.12 Start daemon connection watcher for automatic
            // authoritative reconnect. Pass the active session_id so the
            // watcher can reattach and broadcast a synthetic layout
            // notification after the swap.
            // §16.6 The watcher is local-daemon machinery: an SSH window's
            // connection is the tunnel itself, which the SshSession owns —
            // there is no local daemon to watch or restart.
            if ssh_session.is_none() {
                let domain_for_watch = domain.clone();
                let session_for_watch = session_id.clone();
                cx.update(|cx| {
                    daemon::watch_daemon_connection(domain_for_watch, session_for_watch, cx)
                        .detach();
                });
            }

            // §1.1 spec: terminal 是默认 center pane item.
            // 任何新 Workspace 如果 active pane 为空, 自动 spawn terminal pane。
            // 覆盖 bootstrap / workspace::Open / NewWindow / restore 全部路径。
            let session_for_observer = session_id.clone();
            let snapshot_for_observer = snapshot.clone();
            cx.update(|cx| {
                cx.observe_new::<workspace::Workspace>(move |workspace, window, cx| {
                    let Some(window) = window else { return };

                    // §5.5 Add extension status bar (renders QuickJS extension VDOM).
                    // Apply any VDOM the host already published (init_extensions runs
                    // at startup and may resolve before the first workspace is observed).
                    let pending = quickjs_extensions::take_pending_vdom(cx);
                    let ext_status = cx.new(|_| extension_status_bar::ExtensionStatusBar::new());
                    if !pending.is_empty() {
                        let pending_for_update = pending.clone();
                        ext_status.update(cx, |bar, cx| bar.set_vdom_nodes(pending_for_update, cx));
                    }
                    let host = cx
                        .try_global::<quickjs_extensions::GlobalHostController>()
                        .map(|host| host.0.clone());
                    if let Some(host) = &host {
                        host.update(cx, |host, cx| host.add_status_bar(ext_status.downgrade(), cx));
                    }
                    workspace.status_bar().update(cx, |sb, cx| {
                        sb.add_right_item(ext_status, window, cx);
                    });

                    // §15.4 The remote window's own offline/reconnecting
                    // indicator, kept next to the extension chrome.
                    let connection_status = cx.new(|_| MuxConnectionStatusItem::new());
                    let status_window_id = window.window_handle().window_id();
                    let connection_status_handle = connection_status.downgrade();
                    workspace.status_bar().update(cx, |sb, cx| {
                        sb.add_right_item(connection_status, window, cx);
                    });
                    if cx.try_global::<MuxWindows>().is_none() {
                        cx.set_global(MuxWindows::default());
                    }
                    cx.update_global::<MuxWindows, ()>(|windows, _| {
                        windows
                            .status_items
                            .insert(status_window_id, connection_status_handle);
                    });
                    publish_mux_connection_state(status_window_id, cx);

                    // §5.6 First-install consent prompt: extensions the user has
                    // not approved yet surface as `pending_approvals` on the
                    // host controller. Present them browser-extension style and
                    // let nothing run until the user decides. Ownership of the
                    // prompt is a controller-global claim, so two workspaces
                    // can never race the same pending batch.
                    if let Some(host) = &host {
                        cx.observe_in(&host, window, |_workspace, host, window, cx| {
                            check_pending_consent_prompt(&host, window, cx);
                        })
                        .detach();
                        // The host may already hold pending approvals from
                        // before this workspace existed; the observer only
                        // fires on notifications, so run the same check once
                        // now to reach pre-existing requests.
                        check_pending_consent_prompt(host, window, cx);
                    }

                    // §15.7 Register the shared mux pane action handlers, then the
                    // desktop-only ones (attach/detach/reconnect/kill/new window)
                    // that drive the daemon.
                    mux_window::register_core_mux_actions(workspace, window, cx);
                    workspace
                        .register_action(|workspace, _: &settings::mux_actions::Attach, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let known_session = mux_session_for_window(window, cx);
                            let weak_workspace = workspace.weak_handle();
                            window.spawn(cx, async move |cx| {
                                let result: anyhow::Result<()> = async {
                                let session_id = if let Some(session_id) =
                                    known_session.or_else(|| domain.last_attached_session_id())
                                {
                                    session_id
                                } else {
                                    domain
                                        .list_sessions()
                                        .await?
                                        .first()
                                        .map(|session| session.id.clone())
                                        .ok_or_else(|| anyhow::anyhow!("no mux session is available"))?
                                };
                                // Shares the sidebar's activation path so both
                                // entry points rebind the window identically.
                                cx.update(|window, cx| {
                                    activate_mux_session(
                                        weak_workspace.clone(),
                                        domain.clone(),
                                        session_id,
                                        window,
                                        cx,
                                    );
                                })?;
                                anyhow::Ok(())
                                }
                                .await;
                                if let Err(error) = result {
                                    tracing::error!(%error, "mux::Attach failed");
                                    cx.update(|_, cx| daemon::show_daemon_error(
                                        cx,
                                        format!("Failed to attach mux session: {error}"),
                                    ))?;
                                }
                                anyhow::Ok(())
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::NewWindow, window, cx| {
                            // §3.3 / §15.7 Native path to a second window on the
                            // same session, reachable without the extension host.
                            let Some(app_state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let known_session = mux_session_for_window(window, cx);
                            cx.spawn(async move |_, cx| {
                                let result: anyhow::Result<()> = async {
                                    let session_id = match known_session
                                        .or_else(|| domain.last_attached_session_id())
                                    {
                                        Some(session_id) => session_id,
                                        None => domain
                                            .list_sessions()
                                            .await?
                                            .first()
                                            .map(|session| session.id.clone())
                                            .ok_or_else(|| anyhow::anyhow!("no mux session is available"))?,
                                    };
                                    // A window is one connection, so the new window
                                    // opens its own domain rather than sharing this
                                    // window's socket (Plan 32).
                                    let new_domain = Arc::new(daemon::ensure_daemon_running().await?);
                                    open_mux_window(new_domain, session_id, app_state, cx).await?;
                                    anyhow::Ok(())
                                }
                                .await;
                                if let Err(error) = result {
                                    tracing::error!(%error, "mux::NewWindow failed");
                                    cx.update(|cx| daemon::show_daemon_error(
                                        cx,
                                        format!("Failed to open a new mux window: {error}"),
                                    ));
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::Detach, window, cx| {
                            // §3.3 Detach this window's own connection (Plan 32).
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            cx.spawn(async move |_, cx| {
                                if let Err(error) = domain.detach().await {
                                    tracing::error!(%error, "mux::Detach failed");
                                    cx.update(|cx| daemon::show_daemon_error(
                                        cx,
                                        format!("Failed to detach from mux session: {error}"),
                                    ));
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::Reconnect, window, cx| {
                            let window_id = window.window_handle().window_id();
                            let reconnect = if cx.try_global::<MuxWindows>().is_some() {
                                cx.update_global::<MuxWindows, _>(|windows, _| {
                                    begin_mux_reconnect(&mut windows.windows, window_id)
                                })
                            } else {
                                Err(MuxReconnectUnavailable::WindowNotBound)
                            };
                            let reconnect = match reconnect {
                                Ok(reconnect) => reconnect,
                                Err(MuxReconnectUnavailable::InProgress) => {
                                    tracing::info!("mux::Reconnect ignored because this window is already reconnecting");
                                    return;
                                }
                                Err(MuxReconnectUnavailable::LocalWindow) => {
                                    daemon::show_daemon_error(
                                        cx,
                                        "This is a local mux window; local reconnect is automatic.",
                                    );
                                    return;
                                }
                                Err(MuxReconnectUnavailable::WindowNotBound) => {
                                    daemon::show_daemon_error(
                                        cx,
                                        "This window is not bound to a mux connection.",
                                    );
                                    return;
                                }
                            };

                            publish_mux_connection_state(window_id, cx);
                            daemon::show_daemon_connection_lost(cx);
                            window
                                .spawn(cx, async move |cx| {
                                    let result: anyhow::Result<()> = async {
                                        let local_path = {
                                            let mut ssh_session = reconnect.ssh_session.lock().await;
                                            ssh_session
                                                .reconnect()
                                                .await
                                                .context("failed to rebuild the SSH tunnel")?
                                        };
                                        reconnect
                                            .domain
                                            .reconnect_at_path_in_place(
                                                &local_path,
                                                &reconnect.session_id,
                                                mux::AttachMode::Shared,
                                            )
                                            .await
                                            .context("failed to reattach the remote mux session")?;
                                        anyhow::Ok(())
                                    }
                                    .await;

                                    if let Err(error) = &result {
                                        tracing::error!(
                                            session_id = %reconnect.session_id,
                                            %error,
                                            "mux::Reconnect failed"
                                        );
                                    }
                                    let succeeded = result.is_ok();
                                    if let Err(error) = cx.update(|_, cx| {
                                        let state_updated =
                                            if cx.try_global::<MuxWindows>().is_some() {
                                                cx.update_global::<MuxWindows, bool>(|windows, _| {
                                                    finish_mux_reconnect(
                                                        &mut windows.windows,
                                                        window_id,
                                                        &reconnect.domain,
                                                        succeeded,
                                                    )
                                                })
                                            } else {
                                                false
                                            };
                                        if !state_updated {
                                            return;
                                        }
                                        publish_mux_connection_state(window_id, cx);
                                        match &result {
                                            Ok(()) => daemon::show_daemon_error(
                                                cx,
                                                "Remote mux connection restored.",
                                            ),
                                            Err(error) => daemon::show_daemon_error(
                                                cx,
                                                format!(
                                                    "Remote reconnect failed: {error}. Run mux::Reconnect to try again."
                                                ),
                                            ),
                                        }
                                    }) {
                                        tracing::debug!(%error, "window closed before mux reconnect status could be shown");
                                    }
                                })
                                .detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::KillSession, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            let known_session = mux_session_for_window(window, cx);
                            cx.spawn(async move |_, cx| {
                                let session_id = if let Some(session_id) =
                                    known_session.or_else(|| domain.last_attached_session_id())
                                {
                                    Some(session_id)
                                } else {
                                    match domain.list_sessions().await {
                                        Ok(sessions) => sessions.first().map(|session| session.id.clone()),
                                        Err(error) => {
                                            tracing::error!(%error, "mux::KillSession list_sessions failed");
                                            cx.update(|cx| daemon::show_daemon_error(
                                                cx,
                                                format!("Failed to find the mux session to kill: {error}"),
                                            ));
                                            None
                                        }
                                    }
                                };
                                let Some(session_id) = session_id else {
                                    cx.update(|cx| daemon::show_daemon_error(
                                        cx,
                                        "No mux session is available to kill",
                                    ));
                                    return;
                                };
                                if let Err(error) = domain.kill_session(&session_id).await {
                                    tracing::error!(session_id, %error, "mux::KillSession failed");
                                    cx.update(|cx| daemon::show_daemon_error(
                                        cx,
                                        format!("Failed to kill mux session {session_id}: {error}"),
                                    ));
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::KillServer, window, cx| {
                            let Some(domain) = mux_domain_for_window(window, cx) else { return };
                            cx.spawn(async move |_, cx| {
                                if let Err(error) = domain.shutdown().await {
                                    tracing::error!(%error, "mux::KillServer failed");
                                    cx.update(|cx| daemon::show_daemon_error(
                                        cx,
                                        format!("Failed to stop mux server: {error}"),
                                    ));
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &DumpAccessibilityTree, window, _cx| {
                            // Debug-only: dump the last frame's AccessKit tree for
                            // AT-SPI/screenshot automation. Writes under
                            // $Z3RM_A11Y_DUMP_PATH or /tmp/z3rm-a11y-tree.json.
                            match window.debug_a11y_tree_json() {
                                Some(json) => {
                                    let path = std::env::var("Z3RM_A11Y_DUMP_PATH").unwrap_or_else(
                                        |_| "/tmp/z3rm-a11y-tree.json".to_string(),
                                    );
                                    match std::fs::write(&path, &json) {
                                        Ok(()) => {
                                            tracing::info!(%path, bytes = json.len(), "dumped a11y tree");
                                            eprintln!("a11y tree dumped to {path} ({} bytes)", json.len());
                                        }
                                        Err(error) => {
                                            tracing::error!(%path, error = %error, "failed to write a11y dump");
                                            eprintln!("error writing a11y dump to {path}: {error}");
                                        }
                                    }
                                }
                                None => {
                                    eprintln!(
                                        "a11y tree unavailable (AccessKit inactive or no frame yet; check Z3RM_A11Y)"
                                    );
                                }
                            }
                        });

                    // §15.7 Mount ProjectPanel + GitPanel into their docks so the
                    // bundled native toggle shortcuts (Ctrl+Shift+E / Ctrl+Shift+G)
                    // reach real panels instead of no-op lookups against the empty
                    // docks Workspace::new creates. observe_new fires once per
                    // Workspace; the guards are defensive against duplicate mounts.
                    if workspace.panel::<project_panel::ProjectPanel>(cx).is_none() {
                        let weak = workspace.weak_handle();
                        let async_cx = window.to_async(cx);
                        let async_cx_for_load = async_cx.clone();
                        window
                            .spawn(cx, async move |cx| {
                                let panel = project_panel::ProjectPanel::load(
                                    weak.clone(),
                                    async_cx_for_load,
                                )
                                .await?;
                                weak.update_in(cx, |workspace, window, cx| {
                                    workspace.add_panel(panel, window, cx);
                                })?;
                                anyhow::Ok(())
                            })
                            .detach();
                    }
                    if workspace.panel::<git_ui::git_panel::GitPanel>(cx).is_none() {
                        let weak = workspace.weak_handle();
                        let async_cx = window.to_async(cx);
                        let async_cx_for_load = async_cx.clone();
                        window
                            .spawn(cx, async move |cx| {
                                let panel =
                                    git_ui::git_panel::GitPanel::load(weak.clone(), async_cx_for_load)
                                        .await?;
                                weak.update_in(cx, |workspace, window, cx| {
                                    workspace.add_panel(panel, window, cx);
                                })?;
                                anyhow::Ok(())
                            })
                            .detach();
                    }
                    if workspace.active_pane().read(cx).items().next().is_some() {
                        return;
                    }
                    let Some(domain) = mux_domain_for_window(window, cx) else { return };
                    let snapshot = snapshot_for_observer.clone();
                    tracing::info!("observe_new Workspace: injecting {} MuxPaneViews", snapshot.pane_ids.len());

                    // §15.12 Sync path: snapshot has panes → project the authoritative layout.
                    if !snapshot.pane_ids.is_empty() {
                        install_snapshot_panes(workspace, &snapshot, domain, window, cx);
                        return;
                    }

                    // Async path: no snapshot panes → spawn a new one.
                    let session_id = session_for_observer.clone();
                    let weak_workspace = workspace.weak_handle();
                    let window_handle = window.window_handle();
                    let worktree_cwd = workspace
                        .project()
                        .read(cx)
                        .worktrees(cx)
                        .next()
                        .and_then(|worktree| {
                            worktree.read(cx).as_local().map(|w| w.abs_path().to_path_buf())
                        });
                    window.spawn(cx, async move |cx| {
                        let pane_id = match worktree_cwd.as_ref() {
                            Some(cwd) => {
                                let size = mux_protocol::TerminalSize { cols: 80, rows: 24 };
                                match domain.spawn_pane(&session_id, "main", size, None, Some(cwd.as_path())).await {
                                    Ok(id) => id,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "spawn_pane with cwd failed");
                                        daemon::get_first_pane_id(&domain).await.ok().flatten().unwrap_or_else(|| "default".to_string())
                                    }
                                }
                            }
                            None => {
                                daemon::get_first_pane_id(&domain).await.ok().flatten().unwrap_or_else(|| "default".to_string())
                            }
                        };
                        if let Err(e) = window_handle.update(cx, |_, window, cx| {
                            if let Err(e) = weak_workspace.update(cx, |workspace, cx| {
                                use workspace::ItemHandle;
                                workspace.active_pane().update(cx, |pane, _| {
                                    pane.set_should_display_welcome_page(false);
                                });
                                let view = cx.new(|cx| {
                                    new_mux_pane_view(pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                });
                                subscribe_mux_pane_extension_actions(&view, cx);
                                let item: Box<dyn ItemHandle> = Box::new(view);
                                let pane = workspace.active_pane().clone();
                                workspace.add_item(pane, item, None, true, true, window, cx);
                            }) {
                                tracing::debug!(error = %e, "workspace dropped during pane auto-spawn observer");
                            }
                        }) {
                            tracing::debug!(error = %e, "window dropped during pane auto-spawn observer");
                        }
                    })
                    .detach();
                })
                .detach();
            });

            // §3.3 Window close = detach that window's own connection (Plan 32).
            // Detaching the process-wide domain here would tear down the first
            // window's session membership whenever any other window closed.
            cx.update(|cx| {
                cx.on_window_closed(move |app, window_id| {
                    let Some(closed_window) = take_mux_window(window_id, app) else {
                        return;
                    };
                    app.spawn(async move |_| {
                        if let Err(error) = closed_window.domain.detach().await {
                            tracing::warn!(error = %error, "detach after window close failed");
                        }
                        // Dropping the last handle closes the socket, so the
                        // server releases this window even if detach failed.
                        drop(closed_window);
                    })
                    .detach();
                })
                .detach();
            });

            eprintln!("[z3rm] Creating window via Workspace::new_local");
            let window_handle = open_mux_window_with_snapshot(
                domain,
                session_id,
                snapshot,
                app_state.clone(),
                ssh_session,
                cx,
            )
            .await?;
            eprintln!("[z3rm] Window created Ok: {:?}", window_handle);
            if let Some(warning) = startup_warning {
                cx.update(|cx| daemon::show_daemon_error(cx, warning));
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    });
}

#[cfg(test)]
mod tests {
    use gpui::{App, AppContext as _};
    use settings::{KeymapFile, KeymapFileLoadResult, Settings as _};

    /// Every accessibility test in the workspace draws one surface at a time,
    /// so nothing has ever checked what happens when two of them are open
    /// together: a name that is unique within a panel can still collide with a
    /// name in the panel beside it, and two docks are two landmarks.
    #[gpui::test]
    /// `workspace` pins where focus goes when a tab closes, but it does so
    /// with a `TestItem`, which is a focus handle and nothing else. A real
    /// editor owns its own focus, its own element, and the accessibility node
    /// that focus has to land on — so the plumbing being right there says
    /// nothing about it being right here.
    #[gpui::test]
    async fn closing_a_real_editor_leaves_focus_in_the_tree(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use gpui::VisualTestContext;
        use project::Project;
        use serde_json::json;
        use workspace::MultiWorkspace;

        zlog::init_test();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "main.rs": "fn main() {}\n" }))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            for text in ["fn one() {}\n", "fn two() {}\n"] {
                let buffer = cx.new(|cx| language::Buffer::local(text, cx));
                let editor = cx.new(|cx| {
                    editor::Editor::for_buffer(buffer, Some(project.clone()), window, cx)
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            }
        });
        cx.run_until_parked();

        cx.activate_a11y(cx.window_handle());
        let focus_reaches_tree = |cx: &mut VisualTestContext| {
            let json = cx
                .update(|window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "closing a real editor");
            let role = tree["gpui_focus"]
                .as_str()
                .and_then(|id| tree["nodes"].get(id))
                .and_then(|node| node["aria"]["role"].as_str())
                .map(str::to_string);
            (role, tree)
        };

        let (before, _) = focus_reaches_tree(&mut cx);
        assert!(
            before.is_some(),
            "the open editor holds focus and is a node before anything closes"
        );

        let pane = workspace.read_with(&cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
                .detach();
        });
        cx.run_until_parked();

        let (after, tree) = focus_reaches_tree(&mut cx);
        assert!(
            after.is_some(),
            "focus after closing a real editor has to be on a node: {}",
            serde_json::to_string(&tree["frame"]).unwrap_or_default()
        );

        // Closing the last one is the case with no obvious answer: there is no
        // next tab to fall back to, and an empty pane is the only thing left.
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
                .detach();
        });
        cx.run_until_parked();

        let (empty, tree) = focus_reaches_tree(&mut cx);
        assert!(
            empty.is_some(),
            "closing the last editor must not leave focus without a node: {}",
            serde_json::to_string(&tree["frame"]).unwrap_or_default()
        );
    }

    #[gpui::test]
    async fn two_panels_open_at_once_stay_distinguishable(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use git_ui::git_panel::GitPanel;
        use gpui::VisualTestContext;
        use project::Project;
        use project_panel::ProjectPanel;
        use serde_json::json;
        use workspace::MultiWorkspace;
        use workspace::dock::{Panel as _, PanelHandle as _};

        zlog::init_test();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            git_ui::init(cx);
            project_panel::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                ".git": {},
                "main.rs": "fn main() {}\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo("/project/.git".as_ref(), &[("main.rs", "old\n".into())]);

        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        // Both panels are built the way the app builds them, through the
        // loaders that also read serialized state, rather than through a
        // constructor a test can reach and the product cannot.
        let project_panel = cx
            .update(|window, cx| {
                let workspace = workspace.downgrade();
                window.spawn(cx, async move |cx| {
                    ProjectPanel::load(workspace, cx.clone()).await
                })
            })
            .await
            .expect("the project panel loads");
        let git_panel = workspace.update_in(&mut cx, GitPanel::new_test);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            // One on each side, so both are drawn: two panels in one dock means
            // only the active one renders, which is the single-surface case
            // every other test already covers.
            project_panel.update(cx, |panel, cx| {
                panel.set_position(workspace::dock::DockPosition::Left, window, cx)
            });
            git_panel.update(cx, |panel, cx| {
                panel.set_position(workspace::dock::DockPosition::Right, window, cx)
            });
            workspace.add_panel(project_panel.clone(), window, cx);
            workspace.add_panel(git_panel.clone(), window, cx);
            workspace.open_panel::<ProjectPanel>(window, cx);
            workspace.open_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();

        // With the centre empty the frame is two panels and a status bar; the
        // tab bar and its per-tab controls only exist once something is open,
        // and those are the controls most likely to repeat a name.
        workspace.update_in(&mut cx, |workspace, window, cx| {
            for label in ["main.rs", "notes.md"] {
                workspace.add_item_to_active_pane(
                    Box::new(cx.new(|cx| {
                        workspace::item::test::TestItem::new(cx).with_label(label)
                    })),
                    None,
                    true,
                    window,
                    cx,
                );
            }
        });
        cx.run_until_parked();

        cx.activate_a11y(cx.window_handle());
        let json = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "two panels");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "two panels");
        gpui::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "two panels");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "two panels");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "two panels");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "two panels");
        let handle = cx.window_handle();
        gpui::a11y_checks::assert_every_tab_stop_reaches_the_tree(&mut cx.cx, handle, "two panels");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "two panels");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "two panels");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "two panels");
        gpui::a11y_checks::assert_click_targets_are_reachable(&tree, "two panels");
        gpui::a11y_checks::assert_controls_have_area(&tree, "two panels");
        gpui::a11y_checks::assert_landmarks_are_distinguishable(&tree, "two panels");

        let names: Vec<&str> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter_map(|node| node["aria"]["label"].as_str())
            .collect();
        for panel in ["Project files", "Changed files"] {
            assert!(
                names.contains(&panel),
                "both panels have to be in the tree at once: {names:?}"
            );
        }

        // The landmark list is how a reader jumps between regions, so it has to
        // say what each one holds rather than only where it sits.
        let mut landmarks: Vec<&str> = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .filter(|node| node["aria"]["role"] == "Complementary")
            .filter_map(|node| node["aria"]["label"].as_str())
            .collect();
        landmarks.sort();
        assert_eq!(
            landmarks,
            vec!["Left dock: Project Panel", "Right dock: Git Panel"],
            "a landmark that only says where it sits offers a destination and \
             refuses to say what is there"
        );
    }

    #[gpui::test]
    fn mux_keymap_profiles_load(cx: &mut App) {
        for profile in settings::MUX_KEYMAP_PROFILE_NAMES {
            let content = settings::mux_keymap_profile_content(profile);
            match KeymapFile::load(content.as_ref(), cx) {
                KeymapFileLoadResult::Success { key_bindings } => {
                    assert!(
                        !key_bindings.is_empty(),
                        "{profile} profile has no bindings"
                    );
                }
                KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
                    panic!("mux profile {profile} failed to load: {error_message}");
                }
                KeymapFileLoadResult::JsonParseFailure { error } => {
                    panic!("mux profile {profile} has invalid JSON: {error}");
                }
            }
        }
    }

    #[gpui::test]
    fn startup_keymaps_bind_default_and_mux_profile(cx: &mut App) {
        settings::init(cx);
        settings::bind_startup_keymaps(cx);
        assert_eq!(
            cx.try_global::<settings::ActiveMuxKeymapProfile>()
                .map(settings::ActiveMuxKeymapProfile::profile),
            Some("default")
        );
    }

    #[test]
    fn reconnect_action_is_registered() {
        assert!(
            gpui::generate_list_of_all_registered_actions()
                .any(|action| action.name == "mux::Reconnect"),
            "native mux::Reconnect action must be registered"
        );
    }

    #[gpui::test]
    fn default_profile_binds_all_native_focus_and_kill_actions(cx: &mut App) {
        let content = settings::mux_keymap_profile_content("default");
        let key_bindings = match KeymapFile::load(content.as_ref(), cx) {
            KeymapFileLoadResult::Success { key_bindings } => key_bindings,
            KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
                panic!("default mux profile failed to load: {error_message}")
            }
            KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("default mux profile has invalid JSON: {error}")
            }
        };
        let action_names = key_bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect::<std::collections::HashSet<_>>();

        for required in [
            "mux_pane::FocusDown",
            "mux_pane::FocusNextPane",
            "mux_pane::FocusPrevPane",
            "mux_pane::FocusPane7",
            "mux::KillSession",
            "mux::KillServer",
            "mux::NewWindow",
            "mux::Reconnect",
            // §3.3 `bind_startup_keymaps` drops bindings whose action does not
            // resolve, so a renamed or mistyped action here is silent: the
            // keystroke simply stops doing anything.
            "mux_pane::JumpToPreviousPrompt",
            "mux_pane::JumpToNextPrompt",
        ] {
            assert!(
                action_names.contains(required),
                "default profile is missing native action {required}"
            );
        }
    }

    /// §15.7 The mux session sidebar is a core surface, so it must be reachable
    /// from the default keymap. `bind_startup_keymaps` drops bindings whose
    /// action does not resolve, which the Zed fork still relies on, so assert
    /// against the bindings that actually survive that pass.
    #[gpui::test]
    fn default_keymap_binds_the_session_sidebar(cx: &mut App) {
        let key_bindings = match KeymapFile::load(settings::default_keymap().as_ref(), cx) {
            KeymapFileLoadResult::Success { key_bindings }
            | KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
            KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("default keymap has invalid JSON: {error}")
            }
        };
        let action_names = key_bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect::<std::collections::HashSet<_>>();

        for required in [
            "multi_workspace::ToggleWorkspaceSidebar",
            "multi_workspace::FocusWorkspaceSidebar",
            "mux_sidebar::FocusFilter",
        ] {
            assert!(
                action_names.contains(required),
                "default keymap is missing sidebar action {required}"
            );
        }
    }

    /// §16.7 Switching the mux keymap profile must not leave the previous
    /// profile's bindings active. After switching from `default` to `tmux`,
    /// the `ctrl-shift-t` chord (bound by default, absent from tmux) must be
    /// unbound. This guards against the regressions where `bind_keys` only
    /// appended and `Unbind` entries were never emitted for the prior profile.
    #[gpui::test]
    fn mux_profile_switch_unbinds_previous_keystrokes(cx: &mut App) {
        settings::init(cx);
        settings::bind_startup_keymaps(cx);

        // Use bindings_for_input with a Terminal context so disabled
        // (NoAction/Unbind) bindings are honored by the keymap's resolution
        // pass — all_bindings_for_input bypasses that pass and would always
        // surface the raw bindings.
        let mut ctx_stack = gpui::KeyContext::new_with_defaults();
        ctx_stack.set("Terminal", "Terminal");
        let keystroke = gpui::Keystroke::parse("ctrl-shift-d").unwrap();
        let (before, _) = cx
            .key_bindings()
            .borrow()
            .bindings_for_input(&[keystroke.clone()], &[ctx_stack.clone()]);
        assert!(
            before.iter().any(|b| b.action().name() == "mux::Detach"),
            "default profile should bind ctrl-shift-d to mux::Detach, got actions: {:?}",
            before.iter().map(|b| b.action().name()).collect::<Vec<_>>()
        );

        // Switch profile to tmux (which does not bind ctrl-shift-d).
        settings::MuxSettings::override_global(
            settings::MuxSettings {
                keymap_profile: "tmux".to_string(),
            },
            cx,
        );
        // override_global does not notify SettingsStore on its own; drive the
        // rebind function directly so the test exercises the unbind path.
        settings::bind_configured_mux_keymap_profile(cx);

        let (after, _) = cx
            .key_bindings()
            .borrow()
            .bindings_for_input(&[keystroke], &[ctx_stack]);
        assert!(
            !after.iter().any(|b| b.action().name() == "mux::Detach"),
            "tmux profile must not retain default's mux::Detach binding, still bound to: {:?}",
            after.iter().map(|b| b.action().name()).collect::<Vec<_>>()
        );
    }
    #[test]
    fn file_viewer_routes_only_to_the_window_bound_session() {
        assert_eq!(
            super::open_file_route("session-a", Some("session-a")),
            super::OpenFileRoute::Matched
        );
        assert_eq!(
            super::open_file_route("session-a", Some("session-b")),
            super::OpenFileRoute::WrongSession
        );
        assert_eq!(
            super::open_file_route("session-a", None),
            super::OpenFileRoute::Unbound
        );
    }

    #[test]
    fn file_viewer_enforces_the_two_mib_limit() {
        assert_eq!(
            super::validate_open_file_metadata(true, false, super::MAX_OPEN_FILE_BYTES),
            super::OpenFileMetadata::Readable
        );
        assert_eq!(
            super::validate_open_file_metadata(true, false, super::MAX_OPEN_FILE_BYTES + 1),
            super::OpenFileMetadata::TooLarge
        );
        assert_eq!(
            super::validate_open_file_metadata(false, false, 0),
            super::OpenFileMetadata::Missing
        );
        assert_eq!(
            super::validate_open_file_metadata(true, true, 0),
            super::OpenFileMetadata::Directory
        );
    }

    #[test]
    fn recovery_prompt_is_skipped_for_explicit_attach_targets() {
        assert!(super::should_probe_recovery(None));
        assert!(!super::should_probe_recovery(Some("session-1")));
    }

    #[test]
    fn recovery_prompt_detail_describes_layout_only_recreation() {
        let detail = super::recovery_prompt_detail(
            &[mux_protocol::RecoveryCandidateInfo {
                id: "session-1".to_string(),
                name: "work".to_string(),
                cwd: "/tmp/work".to_string(),
                metadata_complete: false,
                pane_ids: vec!["pane-1".to_string(), "pane-2".to_string()],
            }],
            &[],
        );
        assert!(detail.contains("work"));
        assert!(detail.contains("cwd: /tmp/work"));
        assert!(detail.contains("panes: 2"));
        assert!(detail.contains("metadata: incomplete"));
        assert!(detail.contains("new default shells"));
        assert!(detail.contains("never restored or replayed"));
    }

    /// §3.6 A row the scan could not read is a session the user had and cannot
    /// get back. Logging it server-side only makes it look like it never
    /// existed, so the prompt has to name it.
    #[test]
    fn recovery_prompt_detail_names_unreadable_candidates() {
        let detail = super::recovery_prompt_detail(
            &[mux_protocol::RecoveryCandidateInfo {
                id: "session-1".to_string(),
                name: "work".to_string(),
                cwd: "/tmp/work".to_string(),
                metadata_complete: true,
                pane_ids: vec!["pane-1".to_string()],
            }],
            &["invalid persisted layout for session old-1".to_string()],
        );
        assert!(detail.contains("1 saved session(s) could not be read"));
        assert!(detail.contains("invalid persisted layout for session old-1"));
    }
}
