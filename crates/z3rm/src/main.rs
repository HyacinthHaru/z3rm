// §16.1 Disable command line from opening on release mode
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon;
mod zed;
mod cli;
mod log_viewer;
mod quickjs_extensions;
mod extension_status_bar;

use std::sync::Arc;

use anyhow::Context as _;
use assets::Assets;
use crashes::InitCrashHandler;
use fs::{Fs, RealFs};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Application, Context, Entity, Focusable, Global, TaskExt, Window};
use gpui_platform;
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use theme::ThemeRegistry;
use theme_settings::load_user_theme;
use util::ResultExt as _;

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

fn focus_mux_pane_index(
    workspace: &mut workspace::Workspace,
    index: u8,
    window: &mut Window,
    cx: &mut Context<workspace::Workspace>,
) {
    if let Some(pane) = workspace.panes().get(index as usize).cloned() {
        window.focus(&pane.focus_handle(cx), cx);
    }
}

#[derive(Clone, Debug, settings::RegisterSetting)]
struct MuxSettings {
    keymap_profile: String,
}

impl settings::Settings for MuxSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let mux = content.mux.clone().unwrap_or_default();
        Self {
            keymap_profile: mux.keymap_profile.unwrap_or_else(|| "default".to_string()),
        }
    }
}

struct ActiveMuxKeymapProfile(String);

impl Global for ActiveMuxKeymapProfile {}

fn bind_startup_keymaps(cx: &mut App) {
    match settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx) {
        Ok(key_bindings) => cx.bind_keys(key_bindings),
        Err(error) => tracing::error!(error = %error, "failed to load default keymap"),
    }
    bind_configured_mux_keymap_profile(cx);
    cx.observe_global::<settings::SettingsStore>(|cx| {
        bind_configured_mux_keymap_profile(cx);
    })
    .detach();
}

fn bind_configured_mux_keymap_profile(cx: &mut App) {
    let profile = <MuxSettings as settings::Settings>::get_global(cx)
        .keymap_profile
        .clone();
    if cx
        .try_global::<ActiveMuxKeymapProfile>()
        .is_some_and(|active| active.0 == profile)
    {
        return;
    }
    let path = settings::mux_keymap_profile_path(&profile);
    // Built-in mux profiles reject partial failures (see settings::load_mux_keymap_profile),
    // so a broken profile never leaves half-applied bindings.
    match settings::load_mux_keymap_profile(&profile, cx) {
        Ok(key_bindings) => {
            cx.bind_keys(key_bindings);
            cx.set_global(ActiveMuxKeymapProfile(profile));
        }
        Err(error) => tracing::error!(profile, path, error = %error, "failed to load mux keymap profile"),
    }
}

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
                    if let Some(bytes) = fs.load_bytes(&event.path).await.log_err()
                        && load_user_theme(&theme_registry, &bytes).log_err().is_some()
                    {
                        cx.update(theme_settings::reload_theme);
                    }
                }
            }
        }
    })
    .detach()
}

// ============================================================================
// §16.1 main: GPUI 应用启动 → daemon → window
// ============================================================================

fn main() {
    // §16.1 沙盒与权限检查
    sandbox::run_sandbox_launcher_if_invoked();

    // §3.10 `attach` is the only mux CLI command that opens a GUI. The CLI
    // process still returns immediately: it launches a fresh GUI process with
    // the target carried in environment variables, prints confirmation, and exits.
    let attach_target = if std::env::var_os("Z3RM_GUI_ATTACH").is_some() {
        std::env::var("Z3RM_ATTACH_TARGET").ok()
    } else {
        let args: Vec<String> = std::env::args().collect();
        if let Some(cli::LaunchIntent::Gui { target }) = cli::parse_launch_intent_from(&args) {
            let executable = std::env::current_exe().unwrap_or_else(|error| {
                eprintln!("error: failed to locate z3rm executable: {error}");
                std::process::exit(1);
            });
            let mut command = std::process::Command::new(executable);
            command
                .env("Z3RM_GUI_ATTACH", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(target) = &target {
                command.env("Z3RM_ATTACH_TARGET", target);
            }
            command.spawn().unwrap_or_else(|error| {
                eprintln!("error: failed to launch z3rm GUI: {error}");
                std::process::exit(1);
            });
            eprintln!(
                "z3rm: attached to session '{}' in GUI window",
                target.as_deref().unwrap_or("default")
            );
            std::process::exit(0);
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
            let runtime = tokio::runtime::Runtime::new()
                .expect("failed to create tokio runtime for CLI");
            if let Err(error) = runtime.block_on(async { cli::run_cli_command(cmd).await }) {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }

        if let Ok(Some(extension_args)) = cli::marketplace::parse_extension_args() {
            let runtime = tokio::runtime::Runtime::new()
                .expect("failed to create tokio runtime for extension CLI");
            if let Err(error) = runtime.block_on(async {
                cli::marketplace::run_extension_command(extension_args).await
            }) {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
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

    let app = build_application().with_assets(Assets);
    let background_executor = app.background_executor();

    // §16.1 Crash handler
    let should_install_crash_handler = matches!(
        std::env::var("Z3RM_GENERATE_MINIDUMPS").as_deref(),
        Ok("true" | "1")
    ) || *release_channel::RELEASE_CHANNEL != ReleaseChannel::Dev;

    let crash_handler = if should_install_crash_handler {
        Some(background_executor.spawn(crashes::init(
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
        )))
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
        bind_startup_keymaps(cx);
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
        // §2.1 Backport all Zed UI chrome ::init calls (not in spec remove-list).
        // §2.1 Globals required by chrome ::init calls.
        // Fs and GitHostingProviderRegistry must exist before any git/git_ui
        // panel queries them via cx.global::<>().
        <dyn fs::Fs>::set_global(fs.clone(), cx);
        let git_hosting_provider_registry =
            Arc::new(git::GitHostingProviderRegistry::new());
        git::GitHostingProviderRegistry::set_global(git_hosting_provider_registry, cx);

        workspace::init(app_state.clone(), cx);
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
        git_hosting_providers::init(cx);
        git_ui::init(cx);
        recent_projects::init(cx);
        which_key::init(cx);
        zlog_settings::init(cx);

        // §16.1 daemon 自动启动 → 连接 → session → pane → 窗口
        cx.spawn(async move |cx| {
            eprintln!("[z3rm] Starting daemon connection flow");
            let domain = Arc::new(daemon::ensure_daemon_running().await?);
            eprintln!("[z3rm] Daemon connected");

            // §3.10 GUI attach target: 命令行 `attach [-t target]` 把目标 session
            // 携带过来, 优先解析；target 为空时退回到默认 session。
            let session_id = daemon::ensure_target_session(
                &domain,
                attach_target.as_deref(),
            )
            .await?;
            eprintln!("[z3rm] Session: {}", session_id);

            daemon::ensure_pane_in_session(&domain, &session_id).await?;
            eprintln!("[z3rm] Pane ensured");
            let attach_resp = domain
                .attach(&session_id, mux::AttachMode::Shared)
                .await?;
            // §15.12 / §15.4 Authoritative snapshot: layout tree + pane IDs.
            let snapshot_layout: Option<workspace::layout_projection::LayoutTree> = attach_resp
                .snapshot
                .as_ref()
                .and_then(|s| s.layout.as_ref())
                .map(workspace::layout_projection::LayoutTree::from_proto);
            let snapshot_pane_ids: Vec<String> = match &snapshot_layout {
                Some(layout) => layout.pane_ids(),
                None => attach_resp
                    .snapshot
                    .as_ref()
                    .map(|s| {
                        s.tabs
                            .iter()
                            .flat_map(|t| t.panes.iter().map(|p| p.id.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            eprintln!("[z3rm] Attached to session ({} panes in snapshot)", snapshot_pane_ids.len());

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
            let domain_for_watch = domain.clone();
            let session_for_watch = session_id.clone();
            cx.update(|cx| {
                daemon::watch_daemon_connection(domain_for_watch, session_for_watch, cx).detach();
            });

            // §1.1 spec: terminal 是默认 center pane item.
            // 任何新 Workspace 如果 active pane 为空, 自动 spawn terminal pane。
            // 覆盖 bootstrap / workspace::Open / NewWindow / restore 全部路径。
            let session_for_observer = session_id.clone();
            let snapshot_panes_for_observer = snapshot_pane_ids.clone();
            let snapshot_layout_for_observer = snapshot_layout.clone();
            cx.update(|cx| {
                cx.observe_new::<workspace::Workspace>(move |workspace, window, cx| {
                    let Some(window) = window else { return };

                    // §5.5 Add extension status bar (renders QuickJS extension VDOM)
                    let ext_status = cx.new(|_| extension_status_bar::ExtensionStatusBar::new());
                    workspace.status_bar().update(cx, |sb, cx| {
                        sb.add_right_item(ext_status, window, cx);
                    });

                    // §15.7 Register mux_pane action handlers on every workspace.
                    workspace
                        .register_action(|workspace, _: &settings::mux_actions::SplitRight, window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let pane_id = mux_view.read(cx).pane_id.clone();
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                match domain.split_pane(&pane_id, mux_protocol::split_node::SplitDirection::LeftRight).await {
                                    Ok(new_pane_id) => {
                                        if let Err(e) = window_handle.update(cx, |_, window, cx| {
                                            if let Err(e) = weak_workspace.update(cx, |workspace, cx| {
                                                let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                                    terminal_view::mux_pane::MuxPaneView::new(new_pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                                }));
                                                workspace.split_item(workspace::SplitDirection::Right, item, window, cx);
                                            }) {
                                                tracing::debug!(error = %e, "workspace dropped during mux_pane::SplitRight handler");
                                            }
                                        }) {
                                            tracing::debug!(error = %e, "window dropped during mux_pane::SplitRight handler");
                                        }
                                    }
                                    Err(e) => tracing::error!(error = %e, "mux_pane::SplitRight failed"),
                                }
                            }).detach();
                        })
                        .register_action(|workspace, _: &settings::mux_actions::SplitDown, window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let pane_id = mux_view.read(cx).pane_id.clone();
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                match domain.split_pane(&pane_id, mux_protocol::split_node::SplitDirection::TopBottom).await {
                                    Ok(new_pane_id) => {
                                        if let Err(e) = window_handle.update(cx, |_, window, cx| {
                                            if let Err(e) = weak_workspace.update(cx, |workspace, cx| {
                                                let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                                    terminal_view::mux_pane::MuxPaneView::new(new_pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                                }));
                                                workspace.split_item(workspace::SplitDirection::Down, item, window, cx);
                                            }) {
                                                tracing::debug!(error = %e, "workspace dropped during mux_pane::SplitDown handler");
                                            }
                                        }) {
                                            tracing::debug!(error = %e, "window dropped during mux_pane::SplitDown handler");
                                        }
                                    }
                                    Err(e) => tracing::error!(error = %e, "mux_pane::SplitDown failed"),
                                }
                            }).detach();
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusLeft, window, cx| {
                            workspace.activate_pane_in_direction(workspace::SplitDirection::Left, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusRight, window, cx| {
                            workspace.activate_pane_in_direction(workspace::SplitDirection::Right, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusUp, window, cx| {
                            workspace.activate_pane_in_direction(workspace::SplitDirection::Up, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::NextTab, window, cx| {
                            workspace.active_pane().update(cx, |pane, cx| {
                                pane.activate_next_item(&workspace::pane::ActivateNextItem::default(), window, cx);
                            });
                        })
                        .register_action(|workspace, _: &settings::mux_actions::PrevTab, window, cx| {
                            workspace.active_pane().update(cx, |pane, cx| {
                                pane.activate_previous_item(&workspace::pane::ActivatePreviousItem::default(), window, cx);
                            });
                        })
                        .register_action(|workspace, action: &settings::mux_actions::FocusPaneIndex, window, cx| {
                            focus_mux_pane_index(workspace, action.index, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane0, window, cx| {
                            focus_mux_pane_index(workspace, 0, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane1, window, cx| {
                            focus_mux_pane_index(workspace, 1, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane2, window, cx| {
                            focus_mux_pane_index(workspace, 2, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane3, window, cx| {
                            focus_mux_pane_index(workspace, 3, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane4, window, cx| {
                            focus_mux_pane_index(workspace, 4, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane5, window, cx| {
                            focus_mux_pane_index(workspace, 5, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane6, window, cx| {
                            focus_mux_pane_index(workspace, 6, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane7, window, cx| {
                            focus_mux_pane_index(workspace, 7, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPane8, window, cx| {
                            focus_mux_pane_index(workspace, 8, window, cx);
                        })
                        .register_action(|workspace, action: &settings::mux_actions::EnterPrefixMode, _window, cx| {
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            mux_view.update(cx, |view, cx| view.enter_prefix_mode(action.timeout_ms, cx));
                        })
                        .register_action(|workspace, action: &settings::mux_actions::SendLiteral, _window, cx| {
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            mux_view.update(cx, |view, cx| view.send_literal(&action.keystroke, cx));
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusDown, window, cx| {
                            workspace.activate_pane_in_direction(workspace::SplitDirection::Down, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusNextPane, window, cx| {
                            workspace.activate_next_pane(window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::FocusPrevPane, window, cx| {
                            workspace.activate_previous_pane(window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeLeft, window, cx| {
                            workspace.resize_pane(gpui::Axis::Horizontal, gpui::px(-50.0), window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeRight, window, cx| {
                            workspace.resize_pane(gpui::Axis::Horizontal, gpui::px(50.0), window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeUp, window, cx| {
                            workspace.resize_pane(gpui::Axis::Vertical, gpui::px(-50.0), window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeDown, window, cx| {
                            workspace.resize_pane(gpui::Axis::Vertical, gpui::px(50.0), window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ResizeEqual, _window, cx| {
                            workspace.reset_pane_sizes(cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::CloseTab, window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let pane_id = mux_view.read(cx).pane_id.clone();
                            // Close server-side pane first, then remove the workspace item.
                            cx.background_executor().spawn(async move {
                                if let Err(e) = domain.close_pane(&pane_id).await {
                                    tracing::error!(error = %e, "mux_pane::CloseTab: close_pane failed");
                                }
                            }).detach();
                            workspace.active_pane().update(cx, |pane, cx| {
                                pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
                                    .detach_and_log_err(cx);
                            });
                        })
                        .register_action(|workspace, _: &settings::mux_actions::ZoomToggle, window, cx| {
                            let Some(mux_view) = workspace.active_item_as::<terminal_view::mux_pane::MuxPaneView>(cx) else { return };
                            let new_zoom = !mux_view.read(cx).is_zoomed();
                            // Updates the view's zoom state and notifies the server
                            // (zoom_pane RPC is fire-and-forget; errors logged in set_zoomed).
                            mux_view.update(cx, |view, cx| view.set_zoomed(new_zoom, cx));
                            // Reflect the zoom into the workspace's zoomed view.
                            let pane = workspace.active_pane().clone();
                            workspace.set_pane_zoomed(pane, new_zoom, window, cx);
                        })
                        .register_action(|workspace, _: &settings::mux_actions::NewTab, window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            let weak_workspace = workspace.weak_handle();
                            let window_handle = window.window_handle();
                            window.spawn(cx, async move |cx| {
                                // Resolve the active session, then spawn a pane in a fresh tab.
                                let session_id = match domain.list_sessions().await {
                                    Ok(sessions) => sessions.first().map(|s| s.id.clone()),
                                    Err(e) => { tracing::error!(error = %e, "list_sessions failed"); None }
                                };
                                let Some(session_id) = session_id else { return };
                                let size = mux_protocol::TerminalSize { cols: 80, rows: 24 };
                                let tab_id = format!("tab-{}", std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0));
                                match domain.spawn_pane(&session_id, &tab_id, size, None, None).await {
                                    Ok(new_pane_id) => {
                                        if let Err(e) = window_handle.update(cx, |_, window, cx| {
                                            if let Err(e) = weak_workspace.update(cx, |workspace, cx| {
                                                let pane = workspace.active_pane().clone();
                                                let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                                    terminal_view::mux_pane::MuxPaneView::new(new_pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                                }));
                                                workspace.add_item(pane, item, None, true, true, window, cx);
                                            }) {
                                                tracing::debug!(error = %e, "workspace dropped during mux_pane::NewTab handler");
                                            }
                                        }) {
                                            tracing::debug!(error = %e, "window dropped during mux_pane::NewTab handler");
                                        }
                                    }
                                    Err(e) => tracing::error!(error = %e, "mux_pane::NewTab: spawn_pane failed"),
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::Detach, _window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            cx.background_executor().spawn(async move {
                                if let Err(e) = domain.detach().await {
                                    tracing::error!(error = %e, "mux::Detach failed");
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::KillSession, _window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            cx.background_executor().spawn(async move {
                                // Resolve the first session as the kill target.
                                let session_id = match domain.list_sessions().await {
                                    Ok(sessions) => sessions.first().map(|s| s.id.clone()),
                                    Err(e) => { tracing::error!(error = %e, "KillSession: list_sessions failed"); None }
                                };
                                if let Some(session_id) = session_id {
                                    if let Err(e) = domain.kill_session(&session_id).await {
                                        tracing::error!(error = %e, "mux::KillSession failed");
                                    }
                                }
                            }).detach();
                        })
                        .register_action(|_workspace, _: &settings::mux_actions::KillServer, _window, cx| {
                            let Some(state) = workspace::AppState::try_global(cx) else { return };
                            let Some(domain) = state.mux_domain.clone() else { return };
                            cx.background_executor().spawn(async move {
                                if let Err(e) = domain.shutdown().await {
                                    tracing::error!(error = %e, "mux::KillServer failed");
                                }
                            }).detach();
                        });
                    if workspace.active_pane().read(cx).items().next().is_some() {
                        return;
                    }
                    let Some(state) = workspace::AppState::try_global(cx) else { return };
                    let Some(domain) = state.mux_domain.clone() else { return };
                    let snapshot_layout_for_observer = snapshot_layout_for_observer.clone();
                    let snapshot_panes = snapshot_panes_for_observer.clone();
                    tracing::info!("observe_new Workspace: injecting {} MuxPaneViews", snapshot_panes.len());

                    // §15.12 Sync path: snapshot has panes → project the authoritative layout.
                    if !snapshot_panes.is_empty() {
                        match &snapshot_layout_for_observer {
                            Some(layout) => {
                                workspace.apply_initial_layout(
                                    layout,
                                    |workspace, window, cx| workspace.add_pane_for_layout(window, cx),
                                    |workspace, pane, pane_id, window, cx| {
                                        let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                            terminal_view::mux_pane::MuxPaneView::new(
                                                pane_id,
                                                domain.clone(),
                                                workspace.weak_handle(),
                                                workspace.project().downgrade(),
                                                window,
                                                cx,
                                            )
                                        }));
                                        workspace.add_item(pane.clone(), item, None, true, true, window, cx);
                                    },
                                    window,
                                    cx,
                                );
                            }
                            None => {
                                // No layout tree in snapshot: tabs in the default pane.
                                workspace.active_pane().update(cx, |pane, _| {
                                    pane.set_should_display_welcome_page(false);
                                });
                                for (index, pane_id) in snapshot_panes.into_iter().enumerate() {
                                    let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                        terminal_view::mux_pane::MuxPaneView::new(
                                            pane_id,
                                            domain.clone(),
                                            workspace.weak_handle(),
                                            workspace.project().downgrade(),
                                            window,
                                            cx,
                                        )
                                    }));
                                    let pane = workspace.active_pane().clone();
                                    workspace.add_item(pane, item, None, index == 0, true, window, cx);
                                }
                            }
                        }
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
                                let item: Box<dyn ItemHandle> = Box::new(cx.new(|cx| {
                                    terminal_view::mux_pane::MuxPaneView::new(pane_id, domain, workspace.weak_handle(), workspace.project().downgrade(), window, cx)
                                }));
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

            // window close = detach
            let domain_for_close = domain.clone();
            cx.update(|cx| {
                cx.on_window_closed(move |app, _window_id| {
                    let d = domain_for_close.clone();
                    app.spawn(async move |_| {
                        if let Err(e) = d.detach().await {
                            tracing::warn!(error = %e, "detach failed");
                        }
                    })
                    .detach();
                })
                .detach();
            });

            eprintln!("[z3rm] Creating window via Workspace::new_local");
            let domain_for_init = domain.clone();
            let snapshot_for_init = snapshot_pane_ids.clone();
            let layout_for_init = snapshot_layout.clone();
            let open_result = cx.update(|cx| {
                workspace::Workspace::new_local(
                    vec![],
                    app_state.clone(),
                    None,
                    None,
                    Some(Box::new(move |workspace: &mut workspace::Workspace, window, cx| {
                        // §15.4 / §15.12 Project the authoritative server layout during
                        // construction: one GPUI pane per server pane.
                        match &layout_for_init {
                            Some(layout) => {
                                workspace.apply_initial_layout(
                                    layout,
                                    |workspace, window, cx| workspace.add_pane_for_layout(window, cx),
                                    |workspace, pane, pane_id, window, cx| {
                                        let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                            terminal_view::mux_pane::MuxPaneView::new(
                                                pane_id,
                                                domain_for_init.clone(),
                                                workspace.weak_handle(),
                                                workspace.project().downgrade(),
                                                window,
                                                cx,
                                            )
                                        }));
                                        workspace.add_item(pane.clone(), item, None, true, true, window, cx);
                                    },
                                    window,
                                    cx,
                                );
                            }
                            None => {
                                // No layout tree: single default pane with all views as tabs.
                                let pane = workspace.active_pane().clone();
                                pane.update(cx, |pane, _| {
                                    pane.set_should_display_welcome_page(false);
                                });
                                let pane_ids = if !snapshot_for_init.is_empty() {
                                    snapshot_for_init.clone()
                                } else {
                                    vec!["default".to_string()]
                                };
                                for (index, pane_id) in pane_ids.into_iter().enumerate() {
                                    let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                        terminal_view::mux_pane::MuxPaneView::new(
                                            pane_id,
                                            domain_for_init.clone(),
                                            workspace.weak_handle(),
                                            workspace.project().downgrade(),
                                            window,
                                            cx,
                                        )
                                    }));
                                    workspace.add_item(pane.clone(), item, None, index == 0, true, window, cx);
                                }
                            }
                        }
                    })),
                    workspace::OpenMode::default(),
                    cx,
                )
            }).await?;
            eprintln!("[z3rm] Window created Ok: {:?}", open_result.window);

            // §15.4 / §15.12 Authoritative reconcile: every SessionLayoutChanged carries
            // the server's layout tree; project it into the window's workspace.
            let domain_for_notify = domain.clone();
            let window_handle: gpui::AnyWindowHandle = open_result.window.into();
            cx.spawn(async move |cx| {
                let rx = domain_for_notify.subscribe();
                while let Ok(notif) = rx.recv().await {
                    let Some(mux_protocol::notification::Event::SessionLayoutChanged(layout_change)) =
                        notif.event
                    else {
                        continue;
                    };
                    let Some(proto_layout) = layout_change.layout else {
                        continue;
                    };
                    let layout = workspace::layout_projection::LayoutTree::from_proto(&proto_layout);
                    if let Err(e) = cx.update_window(window_handle, |_, window, cx| {
                        let Some(multi_workspace) = window.root::<workspace::MultiWorkspace>().flatten() else {
                            return;
                        };
                        let Some(workspace) = multi_workspace.read(cx).workspaces().next().cloned() else {
                            return;
                        };
                        workspace.update(cx, |workspace, cx| {
                            let mut existing: std::collections::HashMap<String, Entity<workspace::Pane>> =
                                std::collections::HashMap::default();
                            for pane in workspace.panes() {
                                for item in pane.read(cx).items() {
                                    if let Ok(view) =
                                        item.to_any_view().downcast::<terminal_view::mux_pane::MuxPaneView>()
                                    {
                                        let pane_id = view.read(cx).pane_id.clone();
                                        existing.entry(pane_id).or_insert_with(|| pane.clone());
                                    }
                                }
                            }
                            workspace.apply_layout_snapshot(
                                &layout,
                                existing,
                                |workspace, window, cx| workspace.add_pane_for_layout(window, cx),
                                |workspace, pane, pane_id, window, cx| {
                                    let item: Box<dyn workspace::ItemHandle> = Box::new(cx.new(|cx| {
                                        terminal_view::mux_pane::MuxPaneView::new(
                                            pane_id,
                                            domain_for_notify.clone(),
                                            workspace.weak_handle(),
                                            workspace.project().downgrade(),
                                            window,
                                            cx,
                                        )
                                    }));
                                    workspace.add_item(pane.clone(), item, None, true, true, window, cx);
                                },
                                window,
                                cx,
                            );
                        });
                    }) {
                        tracing::debug!(error = %e, "app context closed during SessionLayoutChanged reconcile");
                        break;
                    }
                }
            })
            .detach();

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    });
}

#[cfg(test)]
mod tests {
    use gpui::App;
    use settings::{KeymapFile, KeymapFileLoadResult};

    #[gpui::test]
    fn mux_keymap_profiles_load(cx: &mut App) {
        for profile in settings::MUX_KEYMAP_PROFILE_NAMES {
            let content = settings::mux_keymap_profile_content(profile);
            match KeymapFile::load(content.as_ref(), cx) {
                KeymapFileLoadResult::Success { key_bindings } => {
                    assert!(!key_bindings.is_empty(), "{profile} profile has no bindings");
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
        super::bind_startup_keymaps(cx);
        assert_eq!(
            cx.try_global::<super::ActiveMuxKeymapProfile>()
                .map(|profile| profile.0.as_str()),
            Some("default")
        );
    }
}
