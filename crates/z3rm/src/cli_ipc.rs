use anyhow::{Context as _, Result};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{AppContext as _, AsyncApp, WindowHandle};
use std::{path::PathBuf, sync::Arc, time::Duration};

use ::cli::{CliRequest, CliResponse, IpcHandshake, ipc};
use util::paths::PathWithPosition;
use workspace::{MultiWorkspace, OpenMode, OpenOptions, OpenVisible, Workspace};

use crate::diff_review::DiffReview;

const CLI_URL_PREFIX: &str = "z3rm-cli://";

#[derive(Clone)]
pub(crate) struct OpenUrlSender(mpsc::UnboundedSender<String>);

pub(crate) fn open_url_channel() -> (OpenUrlSender, mpsc::UnboundedReceiver<String>) {
    let (sender, receiver) = mpsc::unbounded();
    (OpenUrlSender(sender), receiver)
}

impl OpenUrlSender {
    pub(crate) fn send(&self, url: String) {
        if self.0.unbounded_send(url).is_err() {
            tracing::warn!("open URL ignored because the application receiver has closed");
        }
    }
}

pub(crate) fn is_open_url(argument: &str) -> bool {
    argument.starts_with(CLI_URL_PREFIX)
        || argument.starts_with("z3rm://")
        || argument.starts_with("file://")
}

fn cli_server_name(url: &str) -> Option<&str> {
    url.strip_prefix(CLI_URL_PREFIX)
        .filter(|server_name| !server_name.is_empty())
}

pub(crate) async fn handle_open_url(
    url: String,
    app_state: Arc<workspace::AppState>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Some(server_name) = cli_server_name(&url) else {
        tracing::warn!(%url, "unsupported application URL");
        return Ok(());
    };

    let (mut requests, responses) = connect_to_cli(server_name)?;
    responses
        .send(CliResponse::Ping)
        .context("sending CLI handshake ping")?;

    let Some(request) = requests.next().await else {
        anyhow::bail!("CLI connection closed before sending a request");
    };

    let result = handle_cli_request(request, app_state, cx).await;
    if let Err(error) = &result {
        responses
            .send(CliResponse::Stderr {
                message: format!("{error:#}"),
            })
            .context("sending CLI error response")?;
    }
    responses
        .send(CliResponse::Exit {
            status: if result.is_ok() { 0 } else { 1 },
        })
        .context("sending CLI exit response")?;
    result
}

async fn handle_cli_request(
    request: CliRequest,
    app_state: Arc<workspace::AppState>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let CliRequest::Open {
        paths,
        urls,
        diff_paths,
        diff_all: _,
        wsl,
        wait,
        open_behavior,
        env,
        user_data_dir: _,
        dev_container,
        cwd: _,
    } = request
    else {
        anyhow::bail!("unexpected CLI request before an open request");
    };

    anyhow::ensure!(
        wsl.is_none(),
        "WSL path opening is not supported by this z3rm build"
    );
    anyhow::ensure!(
        !dev_container,
        "dev-container path opening is not supported by z3rm"
    );
    anyhow::ensure!(
        !wait,
        "--wait is not yet supported by the z3rm read-only file viewer"
    );

    let paths = collect_open_paths(paths, urls)?;
    let diff_paths = collect_diff_paths(diff_paths);
    let window = wait_for_workspace_window(cx).await?;
    if paths.is_empty() && diff_paths.is_empty() {
        window.update(cx, |_, window, _| window.activate_window())?;
        return Ok(());
    }

    let target_window = if matches!(
        open_behavior,
        ::cli::OpenBehavior::AlwaysNew | ::cli::OpenBehavior::PreferNewWindow
    ) {
        let open_result = cx
            .update(|cx| {
                Workspace::new_local(paths, app_state, None, env, None, OpenMode::NewWindow, cx)
            })
            .await?;
        ensure_open_results(open_result.opened_items)?;
        open_result.window
    } else {
        if !paths.is_empty() {
            let task = window.update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    workspace.open_paths(
                        paths,
                        OpenOptions {
                            visible: Some(OpenVisible::All),
                            focus: Some(true),
                            ..OpenOptions::default()
                        },
                        None,
                        window,
                        cx,
                    )
                })
            })?;
            ensure_open_results(task.await)?;
        }
        window
    };

    open_diff_paths(diff_paths, &target_window, cx).await?;
    target_window.update(cx, |_, window, _| window.activate_window())?;
    Ok(())
}

fn collect_open_paths(paths: Vec<String>, urls: Vec<String>) -> Result<Vec<PathBuf>> {
    let mut result = Vec::with_capacity(paths.len() + urls.len());
    for path in paths {
        result.push(PathWithPosition::parse_str(&path).path);
    }
    for url in urls {
        let parsed = url::Url::parse(&url).with_context(|| format!("invalid URL {url:?}"))?;
        anyhow::ensure!(
            parsed.scheme() == "file",
            "unsupported URL scheme in {url:?}"
        );
        let path = parsed
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file URL {url:?}"))?;
        result.push(PathWithPosition::parse_str(&path.to_string_lossy()).path);
    }
    Ok(result)
}

fn collect_diff_paths(diff_paths: Vec<[String; 2]>) -> Vec<(PathBuf, PathBuf)> {
    diff_paths
        .into_iter()
        .map(|[previous, current]| {
            (
                PathWithPosition::parse_str(&previous).path,
                PathWithPosition::parse_str(&current).path,
            )
        })
        .collect()
}

async fn open_diff_paths(
    diff_paths: Vec<(PathBuf, PathBuf)>,
    window: &WindowHandle<MultiWorkspace>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut reviews = Vec::with_capacity(diff_paths.len());
    for (previous_path, current_path) in diff_paths {
        let (previous_content, current_content) = smol::unblock({
            let previous_path = previous_path.clone();
            let current_path = current_path.clone();
            move || -> Result<(String, String)> {
                let previous_content = std::fs::read_to_string(&previous_path)
                    .with_context(|| format!("reading diff input {}", previous_path.display()))?;
                let current_content = std::fs::read_to_string(&current_path)
                    .with_context(|| format!("reading diff input {}", current_path.display()))?;
                Ok((previous_content, current_content))
            }
        })
        .await?;
        let review = cx.update(|cx| {
            cx.new(|cx| DiffReview::new(current_path, previous_content, current_content, None, cx))
        });
        reviews.push(review);
    }

    window.update(cx, |multi_workspace, window, cx| {
        let workspace = multi_workspace.workspace().clone();
        workspace.update(cx, |workspace, cx| {
            let pane = workspace.active_pane().clone();
            for review in reviews {
                workspace.add_item(pane.clone(), Box::new(review), None, true, true, window, cx);
            }
        });
    })?;
    Ok(())
}

fn ensure_open_results(
    results: Vec<Option<anyhow::Result<Box<dyn workspace::ItemHandle>>>>,
) -> Result<()> {
    let errors = results
        .into_iter()
        .flatten()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        errors.is_empty(),
        "failed to open paths: {}",
        errors.join("; ")
    );
    Ok(())
}

async fn wait_for_workspace_window(cx: &mut AsyncApp) -> Result<WindowHandle<MultiWorkspace>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(window) = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<MultiWorkspace>())
        }) {
            return Ok(window);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the z3rm window"
        );
        cx.background_executor()
            .timer(Duration::from_millis(20))
            .await;
    }
}

fn connect_to_cli(
    server_name: &str,
) -> Result<(
    mpsc::UnboundedReceiver<CliRequest>,
    ipc::IpcSender<CliResponse>,
)> {
    let handshake_sender = ipc::IpcSender::<IpcHandshake>::connect(server_name.to_string())
        .context("connecting to CLI handshake server")?;
    let (request_sender, request_receiver) = ipc::channel::<CliRequest>()?;
    let (response_sender, response_receiver) = ipc::channel::<CliResponse>()?;
    handshake_sender
        .send(IpcHandshake {
            requests: request_sender,
            responses: response_receiver,
        })
        .context("sending CLI handshake")?;

    let (async_sender, async_receiver) = mpsc::unbounded();
    std::thread::Builder::new()
        .name("z3rm-cli-ipc".into())
        .spawn(move || {
            while let Ok(request) = request_receiver.recv() {
                if async_sender.unbounded_send(request).is_err() {
                    break;
                }
            }
        })
        .context("spawning CLI request bridge")?;
    Ok((async_receiver, response_sender))
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn listen_for_cli_connections(sender: OpenUrlSender) -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let socket_path = paths::data_dir().join(format!(
        "z3rm-{}.sock",
        *release_channel::RELEASE_CHANNEL_NAME
    ));
    if let Err(error) = UnixDatagram::unbound()?.connect(&socket_path)
        && error.kind() == std::io::ErrorKind::ConnectionRefused
    {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale CLI socket {socket_path:?}"))?;
    }
    let listener = UnixDatagram::bind(&socket_path)
        .with_context(|| format!("binding CLI socket {socket_path:?}"))?;
    std::thread::Builder::new()
        .name("z3rm-cli-listener".into())
        .spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match listener.recv(&mut buffer) {
                    Ok(length) => sender.send(String::from_utf8_lossy(&buffer[..length]).into()),
                    Err(error) => {
                        tracing::warn!(%error, "CLI socket listener stopped");
                        break;
                    }
                }
            }
        })
        .context("spawning CLI socket listener")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_non_empty_cli_handshake_urls() {
        assert_eq!(cli_server_name("z3rm-cli://server-1"), Some("server-1"));
        assert_eq!(cli_server_name("z3rm-cli://"), None);
        assert_eq!(cli_server_name("z3rm://open"), None);
    }

    #[test]
    fn collects_paths_and_file_urls() {
        let paths = collect_open_paths(
            vec!["/tmp/plain.txt:4:2".into()],
            vec!["file:///tmp/with%20space.txt".into()],
        )
        .expect("valid paths");
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/plain.txt"),
                PathBuf::from("/tmp/with space.txt")
            ]
        );
    }

    #[test]
    fn collects_diff_pairs_and_strips_positions() {
        let paths = collect_diff_paths(vec![[
            "/tmp/previous.txt:4:2".into(),
            "/tmp/current.txt:8".into(),
        ]]);
        assert_eq!(
            paths,
            vec![(
                PathBuf::from("/tmp/previous.txt"),
                PathBuf::from("/tmp/current.txt")
            )]
        );
    }

    #[test]
    fn ipc_handshake_round_trip_delivers_requests_and_responses() {
        let (server, server_name) =
            ipc::IpcOneShotServer::<IpcHandshake>::new().expect("create one-shot IPC server");
        let worker = std::thread::spawn(move || {
            let (mut requests, responses) =
                connect_to_cli(&server_name).expect("connect to one-shot IPC server");
            let request =
                futures::executor::block_on(requests.next()).expect("receive CLI open request");
            assert!(matches!(request, CliRequest::Open { .. }));
            responses
                .send(CliResponse::Exit { status: 0 })
                .expect("send CLI exit response");
        });

        let (_, handshake) = server.accept().expect("accept CLI handshake");
        handshake
            .requests
            .send(CliRequest::Open {
                paths: vec!["/tmp/file.txt".into()],
                urls: Vec::new(),
                diff_paths: Vec::new(),
                diff_all: false,
                wsl: None,
                wait: false,
                open_behavior: ::cli::OpenBehavior::Default,
                env: None,
                user_data_dir: None,
                dev_container: false,
                cwd: None,
            })
            .expect("send CLI open request");
        assert!(matches!(
            handshake.responses.recv().expect("receive CLI response"),
            CliResponse::Exit { status: 0 }
        ));
        worker.join().expect("join IPC bridge worker");
    }
}
