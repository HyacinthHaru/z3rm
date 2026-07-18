// §16.1 Disable command line from opening on release mode
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon;
mod zed;
mod input;
mod cli;
mod log_viewer;

use std::sync::Arc;

use anyhow::Context as _;
use assets::Assets;
use crashes::InitCrashHandler;
use fs::{Fs, RealFs};
use futures::StreamExt as _;
use gpui::{App, Application, TaskExt, WindowOptions};
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
    if std::env::var("Z3RM_EXPERIMENTAL_A11Y").as_deref() == Ok("1") {
        Application::with_platform(platform)
    } else {
        Application::new_inaccessible(platform)
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

    // §3.10 CLI 子命令处理: 如果是 CLI 命令, 执行后直接退出
    if let Some(cmd) = cli::parse_cli_args() {
        let rt = tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime for CLI");
        if let Err(e) = rt.block_on(async { cli::run_cli_command(cmd).await }) {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // §16.11 扩展市场 CLI 命令处理
    if let Ok(Some(ext_args)) = cli::marketplace::parse_extension_args() {
        let rt = tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime for extension CLI");
        if let Err(e) = rt.block_on(async { cli::marketplace::run_extension_command(ext_args).await }) {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

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

        // §16.1 daemon 自动启动 → 连接 → session → window
        cx.spawn(async move |cx| {
            // 1. 确保 daemon 运行并获取 MuxDomain
            let domain = Arc::new(daemon::ensure_daemon_running().await?);

            // 2. 创建/获取默认 session
            let session_id = daemon::ensure_default_session(&domain).await?;

            // 3. attach 到 session
            let _attach_resp = domain.attach(&session_id, mux::AttachMode::ReadOnly).await?;
            tracing::info!(session_id = %session_id, "attached to session");

            // 4. 注册窗口关闭回调: detach session (daemon 继续运行)
            let domain_for_close = domain.clone();
            cx.update(|cx| {
                let _ = cx.on_window_closed(move |app, _window_id| {
                    let d = domain_for_close.clone();
                    app.spawn(async move |_| {
                        if let Err(e) = d.detach().await {
                            tracing::warn!(error = %e, "detach failed on window close");
                        } else {
                            tracing::info!("detached on window close");
                        }
                    }).detach();
                });
            });

            // 5. 创建窗口
            use gpui::AppContext as _;
            let _window = cx.update(|cx| {
                cx.open_window(
                    WindowOptions::default(),
                    |_, cx| cx.new(|_| gpui::EmptyView),
                )
            })?;
            cx.update(|cx| cx.activate(true));

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    });
}
