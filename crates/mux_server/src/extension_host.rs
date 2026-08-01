// §16.8 Server-side QuickJS extension host.
//
// mux_server is the authority for sessions and panes; this module loads
// extensions that declare `runtime.side = "server"` (or `"both"`) and runs
// them inside the daemon under the same quickjs_runtime resource limits the
// GUI client applies (CPU fuel, memory cap, IO token bucket).
//
// Design (§5.2): every `LiveExtension` lives on one dedicated OS thread
// (`z3rm-ext-host`). All QuickJS `ctx.with` re-entry — activation, rendering,
// events, commands — happens on that thread only; connection handlers talk to
// it through a command channel and await a oneshot reply, so the async tokio
// tasks never block on JS execution. Chrome views rendered by server
// extensions are fanned out to attached clients as `ExtensionChromeUpdate`
// notifications (§16), using each session's existing lifecycle subscriber
// set — the daemon never invents its own client list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use anyhow::{Context as _, Result, bail};
use mux_protocol::{ExtensionChromeUpdate, Notification};
use quickjs_runtime::{
    DiscoveredExtension, ExtensionRunner, HostBridge, LiveExtension, discover_server_extensions,
    extension_roots, parse_manifest_str,
};

type Sessions = Arc<parking_lot::RwLock<Vec<crate::session::Session>>>;

/// Compressed install payload cap: extensions are a few KB of JS; a 16 MiB
/// tar.gz already dwarfs anything legitimate and bounds decode work.
const MAX_INSTALL_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
/// Uncompressed cap, guarding against decompression bombs on the host thread.
const MAX_EXTRACTED_BYTES: u64 = 64 * 1024 * 1024;
/// Max extension id / install-name length; it becomes a directory component.
const MAX_EXTENSION_ID_LEN: usize = 128;

/// Default user extension directory, matching the client sync path
/// (`mux::sync::default_extensions_dir`): installs land where both sides
/// already scan.
pub fn default_user_extensions_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("z3rm")
        .join("extensions")
}

/// §16.8 One rendered chrome view on its way to attached clients.
struct ChromeView {
    extension_id: String,
    view_id: String,
    vdom_json: String,
}

// ---------------------------------------------------------------------------
// §5.4 Server-authoritative host bridge
// ---------------------------------------------------------------------------

/// Host bridge backed by the daemon's own session/pane state.
///
/// Calls run synchronously on the extension thread against the in-process
/// server state — unlike the client bridge there is no RPC hop, so there is
/// nothing to time out. Capability gating (`mux.*` namespace) and IO rate
/// limiting happen inside quickjs_runtime before `call` is reached; this
/// impl only decides what the daemon *can* answer, and fails loudly for
/// anything it cannot.
pub struct ServerHostBridge {
    sessions: Sessions,
}

impl ServerHostBridge {
    pub fn new(sessions: Sessions) -> Self {
        Self { sessions }
    }

    fn find_pane(&self, pane_id: &str) -> Option<Arc<crate::pane::Pane>> {
        for session in self.sessions.read().iter() {
            if let Some(pane) = session.panes.read().get(pane_id) {
                return Some(pane.clone());
            }
        }
        None
    }
}

fn required_string(args: &serde_json::Value, index: usize, method: &str) -> Result<String> {
    args.get(index)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("`{method}` requires a string argument at position {index}"))
}

fn optional_u32(args: &serde_json::Value, index: usize) -> Option<u32> {
    args.get(index)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(u32::MAX as u64) as u32)
}

impl HostBridge for ServerHostBridge {
    fn call(&self, method: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
        match method {
            // Same JSON shape the client bridge returns, so an extension
            // written against `mux.listSessions` behaves identically on both
            // sides. `clients` is the field name built-in extensions read.
            "mux.listSessions" => {
                let sessions = self.sessions.read();
                Ok(serde_json::Value::Array(
                    sessions
                        .iter()
                        .map(|session| {
                            serde_json::json!({
                                "id": session.id,
                                "name": session.name,
                                "cwd": session.cwd,
                                "clients": session.attached_client_count(),
                                "createdTimestamp": session.created_timestamp,
                            })
                        })
                        .collect(),
                ))
            }
            "mux.listPanes" => {
                let filter = args.get(0).and_then(serde_json::Value::as_str);
                let mut panes = Vec::new();
                for session in self.sessions.read().iter() {
                    if let Some(wanted) = filter
                        && session.id != wanted
                    {
                        continue;
                    }
                    for (pane_id, pane) in session.panes.read().iter() {
                        panes.push(serde_json::json!({
                            "paneId": pane_id,
                            "sessionId": session.id,
                            "title": *pane.title.read(),
                        }));
                    }
                }
                Ok(serde_json::Value::Array(panes))
            }
            // Focus is per-window client state; the daemon has no single
            // focused pane, so the honest answer is null (same as the client
            // bridge when nothing is focused).
            "mux.focusedPane" => Ok(serde_json::Value::Null),
            "mux.sendInput" | "terminal.write" => {
                let pane_id = required_string(args, 0, method)?;
                let data = required_string(args, 1, method)?;
                let pane = self
                    .find_pane(&pane_id)
                    .with_context(|| format!("pane not found: {pane_id}"))?;
                pane.write_input(data.as_bytes())
                    .with_context(|| format!("writing input to pane {pane_id}"))?;
                Ok(serde_json::json!(true))
            }
            "mux.capturePane" | "terminal.capture" => {
                let pane_id = required_string(args, 0, method)?;
                let count = optional_u32(args, 1).unwrap_or(100);
                let pane = self
                    .find_pane(&pane_id)
                    .with_context(|| format!("pane not found: {pane_id}"))?;
                // from_line 0 / direction 1: oldest-first scrollback fetch,
                // identical to the FetchScrollback handler's parameters.
                let (lines, _total, _version) = pane.fetch_scrollback(0, 1, count);
                let text = lines
                    .iter()
                    .map(|row| {
                        row.cells
                            .iter()
                            .map(|cell| cell.character.as_str())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(serde_json::json!(text))
            }
            other => bail!("unknown host method: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Host thread actor
// ---------------------------------------------------------------------------

enum HostCommand {
    /// Extract + load (or replace) an extension, answering on `reply`.
    Install {
        id: String,
        archive: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    /// §3.4 Deliver a server event to extension subscribers.
    Emit { event: String, payload: String },
    /// Force a full chrome re-render and push.
    Render,
    ListIds(tokio::sync::oneshot::Sender<Vec<String>>),
    Shutdown,
}

/// A live extension plus §5.6 suspension state: an extension that blows its
/// CPU or memory budget is suspended for the daemon's lifetime instead of
/// keep burning the host thread.
struct HostedExtension {
    live: LiveExtension,
    suspended: bool,
}

impl HostedExtension {
    fn note_resource_violations(&mut self) {
        match self.live.take_errors() {
            Ok(errors) => {
                for error in errors {
                    tracing::warn!(id = %self.live.id(), %error, "server extension reported an error");
                }
            }
            Err(error) => {
                tracing::warn!(id = %self.live.id(), %error, "draining extension errors failed");
            }
        }
        if self.live.take_cpu_interrupted() {
            self.suspended = true;
            tracing::error!(
                id = %self.live.id(),
                "server extension exceeded its CPU budget and was suspended"
            );
            return;
        }
        if self.live.take_memory_violated() {
            self.suspended = true;
            tracing::error!(
                id = %self.live.id(),
                "server extension exceeded its memory budget and was suspended"
            );
        }
    }
}

pub struct ServerExtensionHost {
    command_tx: mpsc::Sender<HostCommand>,
    thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    user_extensions_dir: PathBuf,
    sessions: Sessions,
}

fn layout_json(layout: &mux_protocol::LayoutTree) -> serde_json::Value {
    serde_json::json!({
        "root": layout.root.as_ref().map(layout_node_json),
    })
}

fn layout_node_json(node: &mux_protocol::LayoutNode) -> serde_json::Value {
    use mux_protocol::proto::layout_node::Node;

    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), serde_json::Value::String(node.id.clone()));
    match node.node.as_ref() {
        Some(Node::Pane(pane)) => {
            object.insert(
                "paneId".to_string(),
                serde_json::Value::String(pane.pane_id.clone()),
            );
        }
        Some(Node::Split(split)) => {
            let direction = match split.direction {
                1 => "left-right",
                2 => "top-bottom",
                _ => "unspecified",
            };
            object.insert(
                "direction".to_string(),
                serde_json::Value::String(direction.to_string()),
            );
            object.insert(
                "children".to_string(),
                serde_json::Value::Array(split.children.iter().map(layout_node_json).collect()),
            );
            object.insert(
                "ratios".to_string(),
                serde_json::Value::Array(
                    split
                        .ratios
                        .iter()
                        .map(|ratio| serde_json::json!(ratio))
                        .collect(),
                ),
            );
        }
        None => {}
    }
    serde_json::Value::Object(object)
}

impl ServerExtensionHost {
    /// Spawn the dedicated extension thread, discover already-installed
    /// server extensions (§5.5 / §16.8), and start the chrome fan-out task.
    ///
    /// Startup failures inside the thread are logged, never fatal (§15.7):
    /// a broken extension directory must not keep the daemon from booting.
    pub fn start(sessions: Sessions, user_extensions_dir: PathBuf) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::channel::<HostCommand>();
        let (chrome_tx, mut chrome_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<ChromeView>>();
        let bridge = Arc::new(ServerHostBridge::new(sessions.clone()));
        let thread_dir = user_extensions_dir.clone();
        let thread = match std::thread::Builder::new()
            .name("z3rm-ext-host".into())
            .spawn(move || {
                host_thread_main(&thread_dir, bridge, command_rx, chrome_tx);
            }) {
            Ok(thread) => Some(thread),
            Err(error) => {
                tracing::error!(%error, "spawning the extension host thread failed");
                None
            }
        };

        // Fan rendered chrome out over each session's lifecycle subscribers.
        // Only spawned when a tokio runtime exists (always true in `run()`);
        // unit tests that exercise the host directly get the host thread
        // without push delivery.
        if tokio::runtime::Handle::try_current().is_ok() {
            let chrome_sessions = sessions.clone();
            tokio::spawn(async move {
                while let Some(views) = chrome_rx.recv().await {
                    for view in views {
                        let notification = Notification {
                            event: Some(mux_protocol::notification::Event::ExtensionChrome(
                                ExtensionChromeUpdate {
                                    extension_id: view.extension_id,
                                    view_id: view.view_id,
                                    vdom_payload: view.vdom_json.into_bytes(),
                                },
                            )),
                        };
                        for session in chrome_sessions.read().iter() {
                            session.broadcast_lifecycle(notification.clone());
                        }
                    }
                }
            });
        }

        Arc::new(Self {
            command_tx,
            thread: parking_lot::Mutex::new(thread),
            user_extensions_dir,
            sessions,
        })
    }

    pub fn user_extensions_dir(&self) -> &Path {
        &self.user_extensions_dir
    }

    /// Attach the extension observer to every live session.
    ///
    /// Sessions are created after the daemon starts, so this is called from
    /// request dispatch as well as during startup. A weak host reference keeps
    /// session state from retaining the dedicated extension thread.
    pub fn bind_sessions(self: &Arc<Self>, sessions: &Sessions) {
        let host = Arc::downgrade(self);
        let hook: Arc<dyn Fn(Notification) + Send + Sync> = Arc::new(move |notification| {
            if let Some(host) = host.upgrade() {
                host.emit_notification(&notification);
            }
        });
        for session in sessions.write().iter_mut() {
            if session.lifecycle_hook.is_none() {
                session.set_lifecycle_hook(Some(hook.clone()));
            }
            for pane in session.panes.read().values() {
                pane.set_notification_hook(hook.clone());
            }
        }
    }

    fn emit_notification(&self, notification: &Notification) {
        use mux_protocol::notification::Event;

        let Some(event) = notification.event.as_ref() else {
            return;
        };
        let (name, payload) = match event {
            // Pane hooks run on the PTY reader thread and can also run while
            // the connection layer holds the session registry lock. Keep this
            // path lock-free with respect to `Sessions`; the client-side
            // extension bridge hydrates session metadata from snapshots.
            Event::PaneFocused(event) => (
                "pane:focus",
                serde_json::json!({
                    "paneId": event.pane_id,
                }),
            ),
            Event::PaneTitleChanged(event) => (
                "pane:title",
                serde_json::json!({
                    "paneId": event.pane_id,
                    "title": event.title,
                }),
            ),
            Event::PaneAdded(event) => (
                "pane:add",
                serde_json::json!({
                    "paneId": event.pane_id,
                    "tabId": event.tab_id,
                }),
            ),
            Event::PaneRemoved(event) => (
                "pane:remove",
                serde_json::json!({
                    "paneId": event.pane_id,
                    "exitCode": event.exit_code,
                }),
            ),
            Event::TabTitleChanged(event) => (
                "tab:title",
                serde_json::json!({
                    "tabId": event.tab_id,
                    "title": event.title,
                }),
            ),
            Event::SessionLayoutChanged(event) => {
                let Some(layout) = event.layout.as_ref() else {
                    return;
                };
                ("session:layout", layout_json(layout))
            }
            Event::WindowAdded(event) => (
                "window:add",
                serde_json::json!({
                    "windowId": event.window_id,
                    "sessionId": event.session_id,
                }),
            ),
            Event::WindowRemoved(event) => (
                "window:remove",
                serde_json::json!({
                    "windowId": event.window_id,
                    "sessionId": event.session_id,
                }),
            ),
            Event::PaneZoomed(event) => (
                "pane:zoom",
                serde_json::json!({
                    "paneId": event.pane_id,
                    "zoomed": event.zoomed,
                }),
            ),
            Event::ShellIntegrationChanged(event) => (
                "shell:integration",
                serde_json::json!({"cwd": event.cwd}),
            ),
            Event::PaneDirty(event) => (
                "pane:dirty",
                serde_json::json!({"paneId": event.pane_id}),
            ),
            Event::PaneBell(event) => (
                "pane:bell",
                serde_json::json!({"paneId": event.pane_id}),
            ),
            Event::PaneOutput(event) => (
                "pane:output",
                serde_json::json!({
                    "paneId": event.pane_id,
                    "data": event.data,
                    "outputSequence": event.output_sequence,
                }),
            ),
            Event::ClipboardChanged(_) => ("clipboard", serde_json::Value::Null),
            Event::SyncScrollback(_) | Event::ExtensionChrome(_) => return,
        };
        self.emit_event(name, payload);
    }

    /// §16.6 / §16.8 Validate and install an extension archive, blocking
    /// (async) until the host thread finishes extraction + activation.
    pub async fn install_extension(
        &self,
        request: &mux_protocol::InstallExtensionRequest,
    ) -> Result<()> {
        let name = request.name.trim();
        validate_extension_id(name)?;
        if request.source.len() > MAX_INSTALL_ARCHIVE_BYTES {
            bail!(
                "extension archive for `{name}` is {} bytes; limit is {MAX_INSTALL_ARCHIVE_BYTES}",
                request.source.len()
            );
        }
        // Pre-validate the shipped manifest so a client-only extension is
        // rejected up front instead of being extracted and then refused.
        let manifest_text = std::str::from_utf8(&request.manifest)
            .context("extension manifest must be UTF-8 `extension.toml` text")?;
        let manifest =
            parse_manifest_str(name, manifest_text).context("parsing extension manifest")?;
        if !manifest.side.runs_on_server() {
            bail!(
                "extension `{name}` declares runtime side `{:?}`; the daemon only runs `server` or `both` extensions",
                manifest.side
            );
        }
        validate_extension_id(&manifest.id)?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(HostCommand::Install {
                id: manifest.id.clone(),
                archive: request.source.clone(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("extension host thread is gone"))?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => bail!("extension host thread exited before answering install"),
        }
    }

    /// §3.4 Forward a server event to every loaded extension. Never blocks
    /// meaningfully: this is an mpsc send; JS runs later on the host thread.
    pub fn emit_event(&self, event: &str, payload: serde_json::Value) {
        if self
            .command_tx
            .send(HostCommand::Emit {
                event: event.to_string(),
                payload: payload.to_string(),
            })
            .is_err()
        {
            tracing::debug!(event, "extension host thread is gone; event dropped");
        }
    }

    /// Force a chrome re-render + push (e.g. after a client (re)attaches).
    pub fn request_render(&self) {
        if let Err(error) = self.command_tx.send(HostCommand::Render) {
            tracing::debug!(%error, "extension host thread is gone; render request dropped");
        }
    }

    /// Ids of extensions currently loaded on the host thread (test + status
    /// surface).
    pub async fn loaded_extension_ids(&self) -> Vec<String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.command_tx.send(HostCommand::ListIds(reply_tx)).is_err() {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }
}

impl Drop for ServerExtensionHost {
    fn drop(&mut self) {
        if let Err(error) = self.command_tx.send(HostCommand::Shutdown) {
            tracing::debug!(%error, "extension host thread already gone");
        }
        if let Some(handle) = self.thread.lock().take()
            && handle.join().is_err()
        {
            tracing::warn!("extension host thread panicked during shutdown");
        }
    }
}

/// Extension ids become directory components and log fields; reject anything
/// that could escape the install root or confuse tooling.
fn validate_extension_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("extension id must not be empty");
    }
    if id.len() > MAX_EXTENSION_ID_LEN {
        bail!("extension id `{id}` exceeds {MAX_EXTENSION_ID_LEN} bytes");
    }
    if id == "." || id == ".." {
        bail!("extension id `{id}` is not a valid directory name");
    }
    if id.contains(['/', '\\']) || id.chars().any(char::is_control) {
        bail!("extension id `{id}` contains a path separator or control character");
    }
    Ok(())
}

fn host_thread_main(
    user_extensions_dir: &Path,
    bridge: Arc<ServerHostBridge>,
    command_rx: mpsc::Receiver<HostCommand>,
    chrome_tx: tokio::sync::mpsc::UnboundedSender<Vec<ChromeView>>,
) {
    // §5.5 / §16.8 discovery: user dir + built-in roots, server-side filter.
    // discover_server_extensions already skips directories without
    // extension.toml + main.js and logs per-extension failures.
    let roots = extension_roots(user_extensions_dir);
    let discovered = discover_server_extensions(&roots);
    if discovered.is_empty() {
        tracing::info!(?roots, "no server-side extensions discovered");
    }
    let mut hosted = activate_discovered(discovered, bridge.clone());
    let mut published_views = BTreeSet::new();

    // First paint: extensions register chrome during activate.
    if push_chrome_if_dirty(&mut hosted, &chrome_tx, true, &mut published_views).is_err() {
        return;
    }

    loop {
        let command = match command_rx.recv() {
            Ok(command) => command,
            Err(_) => break,
        };
        match command {
            HostCommand::Install { id, archive, reply } => {
                let result =
                    install_on_host_thread(user_extensions_dir, &id, &archive, bridge.clone())
                        .and_then(|live| {
                            // Replace any previous instance of the same id.
                            hosted.retain(|extension| extension.live.id() != live.id());
                            hosted.push(HostedExtension {
                                live,
                                suspended: false,
                            });
                            Ok(())
                        });
                if reply.send(result).is_err() {
                    tracing::debug!(id = %id, "extension install caller dropped before reply");
                }
            }
            HostCommand::Emit { event, payload } => {
                for extension in hosted.iter().filter(|extension| !extension.suspended) {
                    if let Err(error) = extension.live.emit_event(&event, &payload) {
                        tracing::warn!(id = %extension.live.id(), %event, %error, "server extension emit failed");
                    }
                }
            }
            HostCommand::Render => {
                if push_chrome_if_dirty(&mut hosted, &chrome_tx, true, &mut published_views).is_err() {
                    break;
                }
                continue;
            }
            HostCommand::ListIds(reply) => {
                let ids: Vec<String> = hosted
                    .iter()
                    .map(|extension| extension.live.id().to_string())
                    .collect();
                if reply.send(ids).is_err() {
                    tracing::debug!("extension id caller dropped before reply");
                }
            }
            HostCommand::Shutdown => break,
        }
        // §5.6 suspend runaways before the next command, then publish any
        // chrome they invalidated.
        for extension in hosted.iter_mut() {
            extension.note_resource_violations();
        }
        if push_chrome_if_dirty(&mut hosted, &chrome_tx, false, &mut published_views).is_err() {
            break;
        }
    }
}

/// Activate every discovered extension, skipping (and logging) failures —
/// §15.7: one broken extension must not take the others (or the daemon) down.
fn activate_discovered(
    discovered: Vec<DiscoveredExtension>,
    bridge: Arc<ServerHostBridge>,
) -> Vec<HostedExtension> {
    let mut hosted = Vec::new();
    for extension in discovered {
        let runner = ExtensionRunner::for_manifest(&extension.manifest).with_bridge(bridge.clone());
        match runner.load_live(&extension.manifest.id, &extension.source, "activate") {
            Ok(live) => {
                tracing::info!(
                    id = %extension.manifest.id,
                    path = %extension.directory.display(),
                    "server extension loaded"
                );
                hosted.push(HostedExtension {
                    live,
                    suspended: false,
                });
            }
            Err(error) => {
                tracing::warn!(
                    id = %extension.manifest.id,
                    error = %format!("{error:#}"),
                    "server extension load failed"
                );
            }
        }
    }
    hosted
}

/// Extract, validate on disk, and activate an installed extension. Extraction
/// goes to a staging directory first, so a failed install never leaves a
/// half-written directory that startup discovery would pick up, and the
/// previously installed version stays live until the new one activates.
fn install_on_host_thread(
    user_extensions_dir: &Path,
    id: &str,
    archive: &[u8],
    bridge: Arc<ServerHostBridge>,
) -> Result<LiveExtension> {
    std::fs::create_dir_all(user_extensions_dir)
        .with_context(|| format!("creating {}", user_extensions_dir.display()))?;
    let staging_root = user_extensions_dir.join(".staging");
    std::fs::create_dir_all(&staging_root)
        .with_context(|| format!("creating {}", staging_root.display()))?;
    // Discovery scans depth-1 directories of the user dir; `.staging` itself
    // has no manifest so it is skipped, and its per-install children are one
    // level too deep to be picked up.
    let unique = format!(
        "{id}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0),
    );
    let staged = staging_root.join(unique);

    let load = (|| -> Result<LiveExtension> {
        extract_archive(archive, &staged)?;

        let manifest_path = staged.join("extension.toml");
        let manifest_text = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let manifest = parse_manifest_str(id, &manifest_text)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        // Defense in depth: the request pre-validated the shipped manifest,
        // but the on-disk copy is what actually loads.
        if !manifest.side.runs_on_server() {
            bail!(
                "on-disk manifest for `{id}` declares runtime side `{:?}`; refusing to run it on the server",
                manifest.side
            );
        }
        let source_path = staged.join("main.js");
        let source = std::fs::read_to_string(&source_path)
            .with_context(|| format!("reading {}", source_path.display()))?;

        let runner = ExtensionRunner::for_manifest(&manifest).with_bridge(bridge);
        let live = runner
            .load_live(&manifest.id, &source, "activate")
            .with_context(|| format!("activating extension `{id}`"))?;

        // Activation succeeded — swap the staged directory into place.
        let target = user_extensions_dir.join(id);
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("removing previous install at {}", target.display()))?;
        }
        std::fs::rename(&staged, &target).with_context(|| {
            format!("moving staged extension to {}", target.display())
        })?;
        tracing::info!(id = %manifest.id, path = %target.display(), "server extension installed");
        Ok(live)
    })();

    if load.is_err()
        && let Err(error) = std::fs::remove_dir_all(&staged)
    {
        tracing::warn!(path = %staged.display(), %error, "failed to remove failed extension staging directory");
    }
    load
}

/// Extract a tar.gz archive into `target`, refusing path traversal and
/// enforcing an uncompressed size ceiling. `Entry::unpack_in` re-checks
/// containment of every entry against the destination.
fn extract_archive(archive: &[u8], target: &Path) -> Result<()> {
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive.entries().context("reading tar archive")?;
    let mut extracted: u64 = 0;
    while let Some(entry_result) = entries.next() {
        let mut entry = entry_result.context("reading tar entry")?;
        // `entry.path()` errors on absolute paths and `..` components; that
        // is the traversal guard, so inspect it before touching the FS.
        let relative = entry
            .path()
            .context("tar entry has an unsafe path")?
            .into_owned();
        // Declared size bounds the check; the 16 MiB compressed cap above
        // limits how much a lying header can still stream to disk.
        let size = entry.header().entry_size().context("tar entry missing size")?;
        extracted = extracted.saturating_add(size);
        if extracted > MAX_EXTRACTED_BYTES {
            bail!(
                "extension archive exceeds the {MAX_EXTRACTED_BYTES}-byte uncompressed limit"
            );
        }
        let unpacked = entry
            .unpack_in(target)
            .with_context(|| format!("unpacking {}", relative.display()))?;
        if !unpacked {
            bail!("tar entry {} escapes the install directory", relative.display());
        }
    }
    Ok(())
}

/// The VDOM JSON carries the view's own `id` when extensions register named
/// chrome views; fall back to a stable placeholder for bare renders.
fn view_id_of(vdom_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(vdom_json)
        .ok()
        .and_then(|value| value.get("id").and_then(serde_json::Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "default".to_string())
}

/// Render dirty (or all, when `force`) extensions and hand the VDOM batch to
/// the fan-out task. `Err` means the daemon side stopped listening; the host
/// thread treats that as its shutdown signal.
fn push_chrome_if_dirty(
    hosted: &mut [HostedExtension],
    chrome_tx: &tokio::sync::mpsc::UnboundedSender<Vec<ChromeView>>,
    force: bool,
    published_views: &mut BTreeSet<(String, String)>,
) -> Result<(), ()> {
    let stale_published_view = published_views.iter().any(|(extension_id, _)| {
        !hosted.iter().any(|extension| {
            !extension.suspended && extension.live.id() == extension_id
        })
    });
    let dirty = force
        || stale_published_view
        || hosted.iter().any(|extension| {
            !extension.suspended
                && extension.live.needs_render().unwrap_or_else(|error| {
                    tracing::warn!(id = %extension.live.id(), %error, "invalidation check failed");
                    false
                })
        });
    if !dirty {
        return Ok(());
    }

    // Rendering can remove a view (for example, an extension closes an
    // overlay, or gets suspended). Send an empty payload for every previously
    // published key that disappeared; the client treats that as a tombstone.
    let mut current_views = BTreeMap::new();
    for extension in hosted.iter_mut().filter(|extension| !extension.suspended) {
        match extension.live.render_all_views() {
            Ok(rendered) => {
                for json in rendered {
                    current_views.insert(
                        (extension.live.id().to_string(), view_id_of(&json)),
                        json,
                    );
                }
            }
            Err(error) => {
                tracing::warn!(id = %extension.live.id(), %error, "server extension render failed");
            }
        }
        extension.note_resource_violations();
    }

    let current_keys: BTreeSet<(String, String)> = current_views.keys().cloned().collect();
    let mut views =
        Vec::with_capacity(current_views.len() + published_views.difference(&current_keys).count());
    for ((extension_id, view_id), vdom_json) in current_views {
        views.push(ChromeView {
            extension_id,
            view_id,
            vdom_json,
        });
    }
    for (extension_id, view_id) in published_views.difference(&current_keys) {
        views.push(ChromeView {
            extension_id: extension_id.clone(),
            view_id: view_id.clone(),
            vdom_json: String::new(),
        });
    }

    *published_views = current_keys;
    if views.is_empty() {
        return Ok(());
    }
    chrome_tx.send(views).map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write as _;

    fn sessions_with_subscriber() -> (
        Sessions,
        tokio::sync::mpsc::UnboundedReceiver<mux_protocol::Envelope>,
    ) {
        let session = Session::new("s1".into(), "test".into(), "/tmp".into());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        session.add_lifecycle_subscriber("client-1".into(), tx);
        (Arc::new(parking_lot::RwLock::new(vec![session])), rx)
    }

    /// Build a tar.gz containing the given relative path → content entries.
    fn pack_archive(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in entries {
            let bytes = content.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, bytes).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn server_manifest(id: &str) -> String {
        format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"server\"\n\n[capabilities]\nmux = true\n"
        )
    }

    fn install_request(id: &str, main_js: &str) -> mux_protocol::InstallExtensionRequest {
        mux_protocol::InstallExtensionRequest {
            name: id.to_string(),
            manifest: server_manifest(id).into_bytes(),
            source: pack_archive(&[
                ("extension.toml", &server_manifest(id)),
                ("main.js", main_js),
            ]),
        }
    }

    #[test]
    fn validate_extension_id_rejects_unsafe_names() {
        assert!(validate_extension_id("demo").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", "a\x00b", "a\nb"] {
            assert!(validate_extension_id(bad).is_err(), "{bad:?} must be rejected");
        }
        let long = "x".repeat(MAX_EXTENSION_ID_LEN + 1);
        assert!(validate_extension_id(&long).is_err());
    }

    #[test]
    fn bridge_lists_sessions_and_rejects_unknown_methods() {
        let (sessions, _rx) = sessions_with_subscriber();
        let bridge = ServerHostBridge::new(sessions);

        let listed = bridge.call("mux.listSessions", &serde_json::json!([])).unwrap();
        assert_eq!(
            listed,
            serde_json::json!([{
                "id": "s1",
                "name": "test",
                "cwd": "/tmp",
                "clients": 0,
                "createdTimestamp": listed[0]["createdTimestamp"],
            }])
        );

        assert!(bridge
            .call("mux.listSessions", &serde_json::json!([]))
            .is_ok());
        // Unknown method: fail closed with a contextual error.
        let error = bridge
            .call("process.spawn", &serde_json::json!([]))
            .unwrap_err();
        assert!(error.to_string().contains("unknown host method"));
        // Missing argument: contextual, names the method and position.
        let error = bridge
            .call("mux.sendInput", &serde_json::json!([]))
            .unwrap_err();
        assert!(error.to_string().contains("mux.sendInput"));
    }

    /// tar's own `Builder` refuses to emit `..` paths, so a traversal archive
    /// must be forged by hand — exactly the situation the daemon faces from a
    /// malicious peer.
    fn traversal_archive() -> Vec<u8> {
        let mut header = [0u8; 512];
        let name = b"../escaped.txt";
        header[..name.len()].copy_from_slice(name);
        header[100..108].copy_from_slice(b"0000644\0"); // mode
        header[108..116].copy_from_slice(b"0000000\0"); // uid
        header[116..124].copy_from_slice(b"0000000\0"); // gid
        header[124..136].copy_from_slice(b"00000000004\0"); // size = 4
        header[136..148].copy_from_slice(b"00000000000\0"); // mtime
        header[148..156].copy_from_slice(b"        "); // checksum placeholder
        header[156] = b'0'; // regular file
        let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());

        let mut tar_bytes = Vec::new();
        tar_bytes.extend_from_slice(&header);
        tar_bytes.extend_from_slice(b"evil");
        tar_bytes.resize(tar_bytes.len() + 508, 0); // pad entry to 512
        tar_bytes.extend_from_slice(&[0u8; 1024]); // end-of-archive marker

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extract_archive_rejects_traversal() {
        let target = tempfile::tempdir().unwrap();
        assert!(extract_archive(&traversal_archive(), target.path()).is_err());
        assert!(!target.path().parent().unwrap().join("escaped.txt").exists());
        // Nothing half-extracted inside the target either.
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn install_rejects_client_only_extension() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));

        let client_manifest =
            "id = \"client-only\"\nname = \"client-only\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"client\"\n";
        let request = mux_protocol::InstallExtensionRequest {
            name: "client-only".to_string(),
            manifest: client_manifest.as_bytes().to_vec(),
            source: pack_archive(&[("extension.toml", client_manifest), ("main.js", "")]),
        };
        let error = host.install_extension(&request).await.unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("only runs `server` or `both`"),
            "unexpected error: {message}"
        );
        // Nothing was extracted for a rejected install.
        assert!(!temp.path().join("extensions/client-only").exists());
    }

    #[tokio::test]
    async fn install_rejects_traversal_name_and_oversized_archive() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));

        let mut request = install_request("demo", "export function activate() {}");
        request.name = "../escaped".to_string();
        assert!(host.install_extension(&request).await.is_err());

        let mut request = install_request("demo", "export function activate() {}");
        request.source = vec![0u8; MAX_INSTALL_ARCHIVE_BYTES + 1];
        let error = host.install_extension(&request).await.unwrap_err();
        assert!(error.to_string().contains("limit"));
    }

    /// Drain envelopes until the chrome update for `extension_id` arrives;
    /// built-in extensions paint at startup too, so filtering is required.
    async fn recv_chrome_for(
        subscriber: &mut tokio::sync::mpsc::UnboundedReceiver<mux_protocol::Envelope>,
        extension_id: &str,
    ) -> mux_protocol::ExtensionChromeUpdate {
        for _ in 0..64 {
            let Some(envelope) = subscriber.recv().await else {
                break;
            };
            let mux_protocol::proto::envelope::Payload::Notification(notification) =
                envelope.payload.unwrap()
            else {
                continue;
            };
            let Some(mux_protocol::notification::Event::ExtensionChrome(update)) =
                notification.event
            else {
                continue;
            };
            if update.extension_id == extension_id {
                return update;
            }
        }
        panic!("no ExtensionChromeUpdate for {extension_id}");
    }

    #[tokio::test]
    async fn request_render_replays_chrome_to_late_subscriber() {
        let temp = tempfile::tempdir().unwrap();
        let session = Session::new("late-session".into(), "late".into(), "/tmp".into());
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));
        let host = ServerExtensionHost::start(sessions.clone(), temp.path().join("extensions"));
        let main_js = r#"
            export function activate(context) {
                context.registerChromeView("status-bar", {
                    render: () => ({ type: "span", text: "late" }),
                });
            }
        "#;

        host.install_extension(&install_request("late-ext", main_js))
            .await
            .unwrap();
        let (subscriber, mut notifications) = tokio::sync::mpsc::unbounded_channel();
        sessions
            .read()
            .first()
            .expect("late session exists")
            .add_lifecycle_subscriber("late-client".into(), subscriber);

        let update = recv_chrome_for(&mut notifications, "late-ext").await;
        assert_eq!(update.view_id, "status-bar");
        assert!(String::from_utf8(update.vdom_payload)
            .unwrap()
            .contains("late"));
    }

    #[tokio::test]
    async fn install_loads_and_executes_server_extension() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, mut subscriber) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));

        // Registers a named chrome view that reflects live server state via
        // the host bridge — proves extraction, activation, capability-gated
        // host calls, and chrome fan-out end to end.
        let main_js = r#"
            export function activate(context) {
                context.registerChromeView("status-bar", {
                    render: () => {
                        const sessions = context.mux.listSessions();
                        return { id: "server-demo", kind: "div", text: "sessions=" + sessions.length };
                    },
                });
            }
        "#;
        host.install_extension(&install_request("server-demo", main_js))
            .await
            .unwrap();

        // Built-in server extensions from the repo root load alongside ours.
        assert!(
            host.loaded_extension_ids().await.iter().any(|id| id == "server-demo"),
            "server-demo not loaded"
        );
        // The extracted install replaced the staging directory on disk.
        assert!(temp.path().join("extensions/server-demo/main.js").exists());
        let staging = temp.path().join("extensions/.staging");
        let staging_leftovers = std::fs::read_dir(&staging)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(staging_leftovers, 0, "staging debris left behind");

        // Install pushed the first paint to the attached client (skip chrome
        // from built-in extensions that painted at startup).
        let update = recv_chrome_for(&mut subscriber, "server-demo").await;
        assert_eq!(update.view_id, "server-demo");
        let payload = String::from_utf8(update.vdom_payload).unwrap();
        assert!(payload.contains("sessions=1"), "payload was: {payload}");
    }

    #[tokio::test]
    async fn failed_activation_propagates_and_isolates_other_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));

        // Throwing during activate must surface as an install error…
        let error = host
            .install_extension(&install_request("bad-ext", "export function activate() { throw new Error('nope'); }"))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("nope"));
        // …without leaving a broken directory for startup discovery.
        assert!(!temp.path().join("extensions/bad-ext").exists());

        // …and a subsequent good extension still installs.
        host.install_extension(&install_request("good-ext", "export function activate(context) {}"))
            .await
            .unwrap();
        let ids = host.loaded_extension_ids().await;
        assert!(ids.iter().any(|id| id == "good-ext"), "good-ext not loaded: {ids:?}");
        assert!(!ids.iter().any(|id| id == "bad-ext"), "bad-ext must not load: {ids:?}");
    }

    #[tokio::test]
    async fn startup_discovers_installed_server_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let extensions_dir = temp.path().join("extensions");
        let extension_dir = extensions_dir.join("boot-ext");
        std::fs::create_dir_all(&extension_dir).unwrap();
        std::fs::write(extension_dir.join("extension.toml"), server_manifest("boot-ext")).unwrap();
        std::fs::write(
            extension_dir.join("main.js"),
            "export function activate(context) {}",
        )
        .unwrap();
        // A client-only sibling must NOT load on the server.
        let client_dir = extensions_dir.join("gui-ext");
        std::fs::create_dir_all(&client_dir).unwrap();
        std::fs::write(
            client_dir.join("extension.toml"),
            "id = \"gui-ext\"\nname = \"gui-ext\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"client\"\n",
        )
        .unwrap();
        std::fs::write(client_dir.join("main.js"), "export function activate(context) {}").unwrap();

        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, extensions_dir);
        let ids = host.loaded_extension_ids().await;
        assert!(ids.iter().any(|id| id == "boot-ext"), "boot-ext not loaded: {ids:?}");
        assert!(!ids.iter().any(|id| id == "gui-ext"), "client-only extension ran on server: {ids:?}");
    }
    #[tokio::test]
    async fn lifecycle_notifications_reach_server_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, mut subscriber) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions.clone(), temp.path().join("extensions"));
        let main_js = r#"
            export function activate(context) {
                const state = { title: 'initial' };
                const view = {
                    render: () => state.title
                        ? ({ type: 'span', text: state.title })
                        : null,
                };
                context.mux.subscribe('pane:title', (event) => {
                    state.title = event.title || '';
                    view.invalidate();
                });
                context.registerChromeView('event-view', view);
            }
        "#;

        host.install_extension(&install_request("event-ext", main_js))
            .await
            .unwrap();
        let initial = recv_chrome_for(&mut subscriber, "event-ext").await;
        assert!(String::from_utf8(initial.vdom_payload).unwrap().contains("initial"));

        host.bind_sessions(&sessions);
        sessions
            .read()
            .first()
            .expect("test session exists")
            .broadcast_lifecycle(Notification {
                event: Some(mux_protocol::notification::Event::PaneTitleChanged(
                    mux_protocol::PaneTitleChanged {
                        pane_id: "pane-1".into(),
                        title: "updated".into(),
                    },
                )),
            });

        let update = recv_chrome_for(&mut subscriber, "event-ext").await;
        let payload = String::from_utf8(update.vdom_payload).unwrap();
        assert!(payload.contains("updated"), "event payload was {payload}");

        sessions
            .read()
            .first()
            .expect("test session exists")
            .broadcast_lifecycle(Notification {
                event: Some(mux_protocol::notification::Event::PaneTitleChanged(
                    mux_protocol::PaneTitleChanged {
                        pane_id: "pane-1".into(),
                        title: String::new(),
                    },
                )),
            });
        let removed = recv_chrome_for(&mut subscriber, "event-ext").await;
        assert!(
            removed.vdom_payload.is_empty(),
            "removed view must be sent as a tombstone"
        );
    }
}
