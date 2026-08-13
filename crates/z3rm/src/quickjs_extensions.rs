//! §5.2 QuickJS extension loader — scans the extension roots and loads
//! JS extensions via QuickJS on a dedicated OS thread.
//!
//! Per spec §5.2: "QuickJS runtime on a dedicated OS thread. The extension
//! host must not run on the GPUI render thread. Extensions communicate with
//! the UI via async channels; a hung extension freezes only itself."
//!
//! §5.4/§5.5 wiring: the host thread owns the live QuickJS runtimes, renders
//! their chrome views into VDOM JSON and pushes the parsed trees to every
//! [`ExtensionStatusBar`] through an async channel. Rendering is invalidation
//! driven — mux notifications are forwarded into the extensions as events, and
//! a render only happens when an extension actually asked for one.
//!
//! §5.4 mux access: QuickJS runs off the GPUI thread and therefore cannot hold
//! a GPUI `Entity`, so `context.mux.*` calls travel through [`MuxHostBridge`],
//! a synchronous [`quickjs_runtime::HostBridge`] that blocks the extension
//! thread on the async `MuxDomain` RPC with a short timeout.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use extension_host::vdom_bridge::{self, CommandInvocation, VDomChild, VDomNode};
use futures::{AsyncReadExt as _, StreamExt as _};
use gpui::{Action, AppContext as _, Global, Keystroke, SharedString, Task};
use http_client::HttpClient as _;
use parking_lot::Mutex;
use quickjs_runtime::{
    DiscoveredExtension, ExtensionCapabilities, ExtensionLimits, ExtensionRunResult,
    ExtensionRunner, ExtensionSide, FilesystemAccess, HostBridge, LiveExtension,
};
use reqwest_client::ReqwestClient;

/// §5.4 Upper bound on a single blocking mux RPC issued from the extension
/// thread. The extension host is a dedicated thread, so blocking here can only
/// stall extensions — never the render loop (§5.2) — but an unbounded wait
/// would wedge the chrome forever if the daemon hangs.
const MUX_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the host waits for the mux connection to appear in `AppState`
/// before giving up on installing the bridge. Extensions still run without it
/// (§15.7: core commands must not depend on the extension host, and vice
/// versa); they just cannot reach the mux.
const MUX_BRIDGE_WAIT_INTERVAL: Duration = Duration::from_millis(250);
const MUX_BRIDGE_WAIT_ATTEMPTS: usize = 240;

/// Pending extension chrome accepted from the host before any workspace
/// (and thus any status bar) exists. The host stores its merged VDOM here;
/// each new [`ExtensionStatusBar`] drains it on construction, and live updates
/// push directly into existing status bars once one is registered.
#[derive(Default)]
struct AcceptedVdom(pub Arc<Mutex<Vec<VDomNode>>>);

impl Global for AcceptedVdom {}

/// A loaded extension with its metadata and run result.
pub struct LoadedExtension {
    pub id: String,
    pub name: String,
    pub side: ExtensionSide,
    pub result: ExtensionRunResult,
}

/// §5.2 Load every client-side extension reachable from `extensions_dir` plus
/// the built-in roots.
///
/// Returns loaded extensions with their run results. Extensions that fail to
/// load are logged and skipped (a hung/broken extension must not crash the app).
pub fn load_client_extensions(extensions_dir: &Path) -> Vec<LoadedExtension> {
    let roots = quickjs_runtime::extension_roots(extensions_dir);
    quickjs_runtime::discover_client_extensions(&roots)
        .into_iter()
        .map(|extension| {
            let runner = ExtensionRunner::for_manifest(&extension.manifest);
            let result =
                runner.load_extension(&extension.manifest.id, &extension.source, "activate");
            if result.result.is_ok() {
                tracing::info!(id = %extension.manifest.id, "extension loaded successfully");
            } else {
                tracing::warn!(
                    id = %extension.manifest.id,
                    error = ?result.result,
                    "extension loaded with errors"
                );
            }
            LoadedExtension {
                id: extension.manifest.id.clone(),
                name: extension.manifest.name.clone(),
                side: extension.manifest.side,
                result,
            }
        })
        .collect()
}

/// §5.4 Collect the status-bar VDOM trees offered by loaded extensions.
///
/// Each extension's `activate()` may return a VDOM via `context.render(vdom)`
/// or by registering a `status-bar` chrome view. The runtime surfaces that
/// payload as `ExtensionRunResult::vdom_json`; here we parse it into typed
/// [`VDomNode`]s to hand to the status bar. Malformed VDOM is logged and
/// skipped so one bad extension can never poison the chrome.
///
/// Pure and synchronous — intended to be the deterministic extension result
/// path under test.
pub fn collect_status_bar_vdom(loaded: &[LoadedExtension]) -> Vec<VDomNode> {
    let mut nodes = Vec::new();
    for ext in loaded {
        let Some(json) = ext.result.vdom_json.as_deref() else {
            continue;
        };
        match parse_vdom_json(json) {
            Ok(node) => nodes.push(node),
            Err(error) => tracing::warn!(id = %ext.id, %error, "extension VDOM rejected"),
        }
    }
    nodes
}

fn parse_vdom_json(json: &str) -> Result<VDomNode> {
    // §5.2 bound the serialized payload before serde allocates a parse tree;
    // an oversized payload is rejected wholesale rather than partially
    // accepted into the live chrome caches.
    if json.len() > vdom_bridge::MAX_VDOM_PAYLOAD_BYTES {
        bail!(
            "extension VDOM payload of {} bytes exceeds limit of {}",
            json.len(),
            vdom_bridge::MAX_VDOM_PAYLOAD_BYTES
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(json).context("extension VDOM JSON invalid")?;
    vdom_bridge::parse_vdom(&value).context("extension VDOM parse failed")
}

/// §5.5 Publish collected VDOM into the app-global chrome state so freshly

/// Keep client-side QuickJS chrome and server-rendered chrome in one display
/// order. Server views are keyed by extension and view so an update replaces
/// the previous render instead of accumulating duplicate nodes.
fn merge_chrome_nodes(
    local_nodes: &[VDomNode],
    server_nodes: &BTreeMap<(String, String), VDomNode>,
) -> Vec<VDomNode> {
    let mut merged = Vec::with_capacity(local_nodes.len() + server_nodes.len());
    for node in local_nodes {
        let mut node = node.clone();
        // Local extensions are never trusted to name a server extension/view.
        vdom_bridge::strip_server_origin(&mut node);
        merged.push(node);
    }
    merged.extend(server_nodes.values().cloned());
    merged
}

fn apply_server_chrome_node(
    server_nodes: &mut BTreeMap<(String, String), VDomNode>,
    update: mux_protocol::ExtensionChromeUpdate,
) -> Result<()> {
    let extension_id = update.extension_id;
    let view_id = update.view_id;
    let key = (extension_id.clone(), view_id.clone());
    if update.vdom_payload.is_empty() {
        server_nodes.remove(&key);
        return Ok(());
    }
    let json = std::str::from_utf8(&update.vdom_payload)
        .context("server extension VDOM payload is not UTF-8")?;
    let mut node = parse_vdom_json(json).context("server extension VDOM rejected")?;
    // §5.7 Stamp provenance: every onClick/onChange descriptor in this tree
    // is bound to the (extension_id, view_id) the daemon published the view
    // under. Clicks on this chrome route back to the daemon's host thread —
    // never to client-side extensions. The stamp overwrites any origin the
    // extension itself shipped.
    vdom_bridge::stamp_server_origin(&mut node, &extension_id, &view_id);
    server_nodes.insert(key, node);
    Ok(())
}
/// created status bars inherit it.
fn publish_vdom(cx: &mut gpui::App, nodes: Vec<VDomNode>) {
    if cx.try_global::<AcceptedVdom>().is_none() {
        cx.set_global(AcceptedVdom::default());
    }
    let accepted = cx.global::<AcceptedVdom>();
    let slot = accepted.0.clone();
    let mut guard = slot.lock();
    *guard = nodes;
}

/// §5.5 Read pending VDOM from the app-global chrome state so a freshly
/// created [`ExtensionStatusBar`] can render it. Peeks (clones) rather than
/// drains so every workspace inherits the initial chrome; a later reload
/// republishes a fresh set.
pub fn take_pending_vdom(cx: &gpui::App) -> Vec<VDomNode> {
    cx.try_global::<AcceptedVdom>()
        .map(|accepted| accepted.0.lock().clone())
        .unwrap_or_default()
}

/// Initialize the app-global chrome state. Called early so [take_pending_vdom]
/// never finds an absent global even if the host resolves before a workspace
/// is observed.
pub fn init(cx: &mut gpui::App) {
    cx.set_global(AcceptedVdom::default());
}

/// §5.2 Initialize the QuickJS extension system at startup.
pub fn init_extensions(cx: &mut gpui::App) {
    init(cx);
    let extensions_dir = paths::extensions_dir().clone();
    let controller = cx.new(|cx| {
        let mut controller = ExtensionHostController::new();
        controller.start(&extensions_dir, cx);
        controller
    });
    cx.set_global(GlobalHostController(controller));
    // §16.7 Native surfaces for extension commands: the global command
    // palette lists them, and declared chords dispatch through the app's
    // keystroke path with native bindings always winning.
    install_command_palette_interception(cx);
    install_global_shortcuts(cx);
}

// ---------------------------------------------------------------------------
// §5.4 mux host bridge
// ---------------------------------------------------------------------------

/// State the bridge derives from mux notifications so synchronous JS calls can
/// answer `focusedPane` / `currentSession` without an extra round trip.
#[derive(Default)]
struct MuxBridgeState {
    focused_pane: Option<String>,
    session_name: Option<String>,
    /// pane id → tab id, learned from `PaneAdded`; lets `tab:title` events
    /// carry the pane the tab-bar must focus when a tab is clicked.
    pane_tabs: HashMap<String, String>,
    pane_titles: HashMap<String, String>,
}

/// §5.4 Synchronous bridge from extension JS to the authoritative `MuxDomain`.
///
/// The extension API is synchronous (`context.mux.listSessions()` returns an
/// array, not a promise) and every built-in relies on that shape, so the bridge
/// blocks instead of returning promises: adding a microtask pump would change
/// the published API for no user-visible gain. Blocking is safe because it
/// happens on the dedicated extension thread (spec §5.2), and every call is
/// bounded by [`MUX_CALL_TIMEOUT`].
pub struct MuxHostBridge {
    domain: Arc<mux::MuxDomain>,
    state: Arc<Mutex<MuxBridgeState>>,
    /// §5.6 扩展声明的文件系统范围: 桥按声明范围构造 (每个扩展装载时拿到自己
    /// 的桥), `filesystem.*` 只对该范围 (home 或权威工作根) 内的路径放行。
    filesystem: FilesystemAccess,
    /// §5.6 Home 约束根 (默认 `dirs::home_dir()`); 测试注入临时目录。
    home: Option<PathBuf>,
    /// §5.6 Cwd 约束根 (默认宿主进程当前工作目录, 即 `workspace.getPath` 报告
    /// 的权威根); 测试注入临时目录。
    cwd: Option<PathBuf>,
}

impl MuxHostBridge {
    fn new(
        domain: Arc<mux::MuxDomain>,
        state: Arc<Mutex<MuxBridgeState>>,
        filesystem: FilesystemAccess,
    ) -> Self {
        Self {
            domain,
            state,
            filesystem,
            home: None,
            cwd: None,
        }
    }

    /// §5.6 把扩展请求的路径约束到声明范围内 (见
    /// [`quickjs_runtime::confine_to_root`])。
    ///
    /// 范围语义与服务器桥一致: `Cwd` 只允许权威工作区/当前工作根内的路径,
    /// `Home` 只允许主目录内的路径——`cwd` 声明不能因此获得主目录访问权, home
    /// 也不能逃出主目录。`None` (未声明) fail closed。
    fn confine(&self, path: &str) -> Result<PathBuf> {
        let root = match self.filesystem {
            FilesystemAccess::None => {
                bail!("filesystem access is not granted to this extension");
            }
            FilesystemAccess::Cwd => match &self.cwd {
                Some(cwd) => cwd.clone(),
                None => std::env::current_dir()
                    .context("host working directory unavailable")?,
            },
            FilesystemAccess::Home => match &self.home {
                Some(home) => home.clone(),
                None => dirs::home_dir().context("host home directory unavailable")?,
            },
        };
        quickjs_runtime::confine_to_root(&root, path)
    }

    fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        self.run_for(future, MUX_CALL_TIMEOUT, "mux call")
    }

    fn run_for<T>(
        &self,
        future: impl Future<Output = Result<T>>,
        timeout: Duration,
        operation: &'static str,
    ) -> Result<T> {
        smol::block_on(smol::future::or(future, async move {
            smol::Timer::after(timeout).await;
            Err(anyhow!("{operation} timed out after {timeout:?}"))
        }))
    }
}

fn argument(args: &serde_json::Value, index: usize) -> Option<&serde_json::Value> {
    args.get(index).filter(|value| !value.is_null())
}

fn required_string(args: &serde_json::Value, index: usize, method: &str) -> Result<String> {
    argument(args, index)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("`{method}` requires a string argument at position {index}"))
}

fn optional_string(args: &serde_json::Value, index: usize) -> Option<String> {
    argument(args, index)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn optional_u32(args: &serde_json::Value, index: usize) -> Option<u32> {
    argument(args, index)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(u32::MAX as u64) as u32)
}

fn session_json(session: &mux_protocol::SessionInfo) -> serde_json::Value {
    // `clients` is the field name the built-in session manager reads; keeping
    // the Rust-side name would render "undefined attached".
    serde_json::json!({
        "id": session.id,
        "name": session.name,
        "cwd": session.cwd,
        "clients": session.attached_clients,
        "createdTimestamp": session.created_timestamp,
    })
}

fn split_direction(value: &str) -> Result<mux_protocol::split_node::SplitDirection> {
    match value {
        "right" | "left" | "horizontal" | "left-right" => {
            Ok(mux_protocol::split_node::SplitDirection::LeftRight)
        }
        "down" | "up" | "vertical" | "top-bottom" => {
            Ok(mux_protocol::split_node::SplitDirection::TopBottom)
        }
        other => bail!("unknown split direction: {other}"),
    }
}

fn scrollback_text(response: &mux_protocol::FetchScrollbackResponse) -> String {
    response
        .lines
        .iter()
        .map(|line| {
            line.cells
                .iter()
                .map(|cell| cell.char.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl HostBridge for MuxHostBridge {
    fn call(&self, method: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
        match method {
            "mux.listSessions" => {
                let sessions = self.run(self.domain.list_sessions())?;
                Ok(serde_json::Value::Array(
                    sessions.iter().map(session_json).collect(),
                ))
            }
            "mux.currentSession" => {
                let Some(session_id) = self.domain.last_attached_session_id() else {
                    return Ok(serde_json::Value::Null);
                };
                let sessions = self.run(self.domain.list_sessions())?;
                Ok(sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(session_json)
                    .unwrap_or(serde_json::Value::Null))
            }
            "mux.focusedPane" => {
                let state = self.state.lock();
                Ok(match &state.focused_pane {
                    Some(pane_id) => serde_json::json!({
                        "id": pane_id,
                        "paneId": pane_id,
                        "title": state.pane_titles.get(pane_id).cloned().unwrap_or_default(),
                        "tabId": state.pane_tabs.get(pane_id).cloned(),
                        "sessionName": state.session_name.clone().unwrap_or_default(),
                    }),
                    None => serde_json::Value::Null,
                })
            }
            "mux.createSession" => {
                let name = required_string(args, 0, method)?;
                let cwd = optional_string(args, 1)
                    .map(PathBuf::from)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("/"));
                let id = self.run(self.domain.create_session(&name, &cwd))?;
                Ok(serde_json::json!(id))
            }
            "mux.killSession" => {
                let id = required_string(args, 0, method)?;
                self.run(self.domain.kill_session(&id))?;
                Ok(serde_json::json!(true))
            }
            "mux.attach" => {
                let id = required_string(args, 0, method)?;
                // §3.3 Attaches with this domain's own window id (Plan 32); the
                // extension host shares the connection of the window it runs in.
                self.run(self.domain.attach(&id, mux::AttachMode::Shared))?;
                Ok(serde_json::json!(true))
            }
            "mux.detach" => {
                self.run(self.domain.detach())?;
                Ok(serde_json::json!(true))
            }
            "mux.focusPane" => {
                let pane = required_string(args, 0, method)?;
                self.run(self.domain.focus_pane(&pane))?;
                self.state.lock().focused_pane = Some(pane);
                Ok(serde_json::json!(true))
            }
            "mux.splitPane" => {
                let direction = split_direction(&required_string(args, 0, method)?)?;
                let pane = match optional_string(args, 1) {
                    Some(pane) => pane,
                    None => self
                        .state
                        .lock()
                        .focused_pane
                        .clone()
                        .context("mux.splitPane needs a pane id and no pane is focused")?,
                };
                let new_pane = self.run(self.domain.split_pane(&pane, direction))?;
                Ok(serde_json::json!(new_pane))
            }
            "mux.closePane" => {
                let pane = required_string(args, 0, method)?;
                self.run(self.domain.close_pane(&pane))?;
                Ok(serde_json::json!(true))
            }
            "mux.sendInput" | "terminal.write" => {
                let pane = required_string(args, 0, method)?;
                let data = required_string(args, 1, method)?;
                self.run(self.domain.send_input(&pane, data.as_bytes()))?;
                Ok(serde_json::json!(true))
            }
            "mux.capturePane" | "terminal.capture" => {
                let pane = required_string(args, 0, method)?;
                let lines = optional_u32(args, 1).unwrap_or(100);
                let response = self.run(self.domain.fetch_scrollback(&pane, 0, 1, lines))?;
                Ok(serde_json::json!(scrollback_text(&response)))
            }
            "mux.resizePane" => {
                let pane = required_string(args, 0, method)?;
                let cols = optional_u32(args, 1).context("mux.resizePane requires cols")?;
                let rows = optional_u32(args, 2).context("mux.resizePane requires rows")?;
                self.run(self.domain.resize_pane(&pane, cols, rows))?;
                Ok(serde_json::json!(true))
            }
            "mux.setPaneTitle" => {
                let pane = required_string(args, 0, method)?;
                let title = required_string(args, 1, method)?;
                self.run(self.domain.set_pane_title(&pane, &title))?;
                Ok(serde_json::json!(true))
            }
            "mux.applyLayout" => {
                // The mux protocol has no layout-apply request (§9 only exposes
                // ResizeLayout), so this fails loudly instead of silently
                // pretending the preset was restored.
                bail!(
                    "mux.applyLayout is not supported: the mux protocol has no apply-layout request"
                )
            }
            "settings.get" => {
                let key = required_string(args, 0, method)?;
                read_setting(&key)
            }
            "settings.set" => {
                let key = required_string(args, 0, method)?;
                let value = args.get(1).cloned().unwrap_or(serde_json::Value::Null);
                write_setting(&key, value)
            }
            "workspace.getPath" => {
                let cwd = std::env::current_dir()
                    .context("reading the host working directory for workspace.getPath")?;
                Ok(serde_json::json!(cwd.to_string_lossy().to_string()))
            }
            "filesystem.readTextFile" => {
                let path = required_string(args, 0, method)?;
                let path = self.confine(&path)?;
                let metadata = std::fs::metadata(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                if metadata.len() > quickjs_runtime::MAX_EXTENSION_FILE_READ {
                    bail!(
                        "file is too large for an extension to read (limit {} bytes): {}",
                        quickjs_runtime::MAX_EXTENSION_FILE_READ,
                        path.display()
                    );
                }
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Ok(serde_json::json!(text))
            }
            "filesystem.readDir" => {
                let path = required_string(args, 0, method)?;
                let path = self.confine(&path)?;
                let mut entries = Vec::new();
                for entry in std::fs::read_dir(&path)
                    .with_context(|| format!("listing {}", path.display()))?
                {
                    let entry = entry.with_context(|| format!("listing {}", path.display()))?;
                    let kind = match entry.file_type() {
                        Ok(kind) if kind.is_dir() => "dir",
                        Ok(kind) if kind.is_symlink() => "symlink",
                        _ => "file",
                    };
                    entries.push(serde_json::json!({
                        "name": entry.file_name().to_string_lossy(),
                        "kind": kind,
                    }));
                    if entries.len() >= quickjs_runtime::MAX_EXTENSION_DIR_ENTRIES {
                        break;
                    }
                }
                Ok(serde_json::Value::Array(entries))
            }
            "network.fetch" => {
                let url = required_string(args, 0, method)?;
                if url.len() > quickjs_runtime::MAX_EXTENSION_URL_LEN {
                    bail!(
                        "network URL exceeds {} bytes",
                        quickjs_runtime::MAX_EXTENSION_URL_LEN
                    );
                }
                let options = args.get(1).cloned().unwrap_or_else(|| serde_json::json!({}));
                let timeout = quickjs_runtime::parse_extension_timeout(
                    &options,
                    quickjs_runtime::EXTENSION_FETCH_TIMEOUT,
                    quickjs_runtime::EXTENSION_FETCH_TIMEOUT_MAX_MS,
                )?;
                let method_name = options
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("GET");
                let request_method = http_client::Method::from_bytes(method_name.as_bytes())
                    .with_context(|| format!("invalid HTTP method: {method_name}"))?;
                let uri: http_client::Uri = url.parse().with_context(|| format!("invalid URL: {url}"))?;
                let body = options
                    .get("body")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec();
                if body.len() > quickjs_runtime::MAX_EXTENSION_FILE_READ as usize {
                    bail!("network request body exceeds {} bytes", quickjs_runtime::MAX_EXTENSION_FILE_READ);
                }
                let request = http_client::Request::builder()
                    .method(request_method)
                    .uri(uri)
                    .body(http_client::AsyncBody::from(body))
                    .context("building network request")?;
                let client = ReqwestClient::new();
                let response = self.run_for(
                    async { client.send(request).await.map_err(anyhow::Error::from) },
                    timeout,
                    "network.fetch",
                )?;
                let (parts, body) = response.into_parts();
                let headers = parts
                    .headers
                    .iter()
                    .filter_map(|(name, value)| {
                        Some((
                            name.as_str().to_owned(),
                            serde_json::Value::String(value.to_str().ok()?.to_owned()),
                        ))
                    })
                    .collect::<serde_json::Map<_, _>>();
                let response_body = self.run_for(
                    async move {
                        let mut bytes = Vec::new();
                        let mut body = body.take(quickjs_runtime::MAX_EXTENSION_FILE_READ + 1);
                        body.read_to_end(&mut bytes).await.map_err(anyhow::Error::from)?;
                        if bytes.len() > quickjs_runtime::MAX_EXTENSION_FILE_READ as usize {
                            bail!("network response exceeds {} bytes", quickjs_runtime::MAX_EXTENSION_FILE_READ);
                        }
                        Ok::<_, anyhow::Error>(bytes)
                    },
                    timeout,
                    "network.fetch response body",
                )?;
                Ok(serde_json::json!({
                    "status": parts.status.as_u16(),
                    "headers": headers,
                    "body": String::from_utf8_lossy(&response_body),
                }))
            }
            "process.spawn" => {
                let command = required_string(args, 0, method)?;
                let arguments = args
                    .get(1)
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .map(|value| {
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .context("process arguments must be strings")
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let options = args.get(2).cloned().unwrap_or_else(|| serde_json::json!({}));
                let timeout = quickjs_runtime::parse_extension_timeout(
                    &options,
                    quickjs_runtime::EXTENSION_PROCESS_TIMEOUT,
                    quickjs_runtime::EXTENSION_PROCESS_TIMEOUT_MAX_MS,
                )?;
                let output = quickjs_runtime::run_extension_process(&command, &arguments, timeout)?;
                Ok(serde_json::json!({
                    "status": output.status.code(),
                    "success": output.status.success(),
                    "stdout": String::from_utf8_lossy(&output.stdout),
                    "stderr": String::from_utf8_lossy(&output.stderr),
                }))
            }
            other => bail!("unknown host method: {other}"),
        }
    }
}

/// §5.6 `settings` capability: dotted-path lookup into the user settings file.
fn read_setting(key: &str) -> Result<serde_json::Value> {
    quickjs_runtime::validate_settings_key(key)?;
    let path = paths::settings_file();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Value::Null);
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if bytes.len() as u64 > quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES {
        bail!(
            "settings document exceeds {} bytes",
            quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES
        );
    }
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    let mut cursor = &document;
    for segment in key.split('.') {
        match cursor.get(segment) {
            Some(next) => cursor = next,
            None => return Ok(serde_json::Value::Null),
        }
    }
    Ok(cursor.clone())
}

/// §5.6 `settings.set`: update one dotted JSON key atomically.
fn write_setting(key: &str, value: serde_json::Value) -> Result<serde_json::Value> {
    quickjs_runtime::validate_settings_key(key)?;
    let value_bytes = serde_json::to_vec(&value).context("serializing settings value")?;
    if value_bytes.len() > quickjs_runtime::MAX_EXTENSION_SETTINGS_VALUE_BYTES {
        bail!(
            "settings value exceeds {} bytes",
            quickjs_runtime::MAX_EXTENSION_SETTINGS_VALUE_BYTES
        );
    }
    let path = paths::settings_file();
    let mut document = match std::fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() as u64 > quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES {
                bail!(
                    "settings document exceeds {} bytes",
                    quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES
                );
            }
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut cursor = &mut document;
    for segment in key.split('.') {
        if !cursor.is_object() {
            *cursor = serde_json::json!({});
        }
        let object = cursor
            .as_object_mut()
            .context("settings document became non-object")?;
        cursor = object
            .entry(segment.to_owned())
            .or_insert(serde_json::Value::Null);
    }
    *cursor = value.clone();

    let encoded = serde_json::to_vec_pretty(&document).context("serializing settings")?;
    if encoded.len() > quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES as usize {
        bail!(
            "settings document exceeds {} bytes",
            quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES
        );
    }
    let parent = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating settings directory {}", parent.display()))?;
    let temporary = parent.join(format!(".settings.json.{}.tmp", std::process::id()));
    std::fs::write(&temporary, encoded)
        .with_context(|| format!("writing temporary settings file {}", temporary.display()))?;
    std::fs::File::open(&temporary)?.sync_all()?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("committing settings file {}", path.display()))?;
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// §3.4 mux notification → extension event mapping
// ---------------------------------------------------------------------------

fn direction_name(direction: i32) -> &'static str {
    if direction == mux_protocol::split_node::SplitDirection::LeftRight as i32 {
        "left-right"
    } else if direction == mux_protocol::split_node::SplitDirection::TopBottom as i32 {
        "top-bottom"
    } else {
        "unspecified"
    }
}

fn layout_node_json(node: &mux_protocol::LayoutNode) -> serde_json::Value {
    match &node.node {
        Some(mux_protocol::layout_node::Node::Pane(leaf)) => serde_json::json!({
            "id": node.id,
            "type": "pane",
            "paneId": leaf.pane_id,
        }),
        Some(mux_protocol::layout_node::Node::Split(split)) => serde_json::json!({
            "id": node.id,
            "type": "split",
            "direction": direction_name(split.direction),
            "ratios": split.ratios,
            "children": split.children.iter().map(layout_node_json).collect::<Vec<_>>(),
        }),
        None => serde_json::json!({ "id": node.id, "type": "empty" }),
    }
}

/// §3.4 Translate one mux notification into the extension-facing events the
/// built-ins subscribe to. Returns `(event name, JSON payload)` pairs; a
/// notification can expand into more than one event (a title change on the
/// focused pane also refreshes `pane:focus`).
fn notification_events(
    notification: &mux_protocol::Notification,
    state: &Mutex<MuxBridgeState>,
) -> Vec<(String, serde_json::Value)> {
    use mux_protocol::notification::Event;

    let Some(event) = notification.event.as_ref() else {
        return Vec::new();
    };

    let focus_payload = |state: &MuxBridgeState, pane_id: &str| {
        serde_json::json!({
            "paneId": pane_id,
            "id": pane_id,
            "title": state.pane_titles.get(pane_id).cloned().unwrap_or_default(),
            "tabId": state.pane_tabs.get(pane_id).cloned(),
            "sessionName": state.session_name.clone().unwrap_or_default(),
        })
    };

    match event {
        Event::PaneFocused(focused) => {
            let mut state = state.lock();
            state.focused_pane = Some(focused.pane_id.clone());
            vec![("pane:focus".into(), focus_payload(&state, &focused.pane_id))]
        }
        Event::PaneTitleChanged(changed) => {
            let mut state = state.lock();
            state
                .pane_titles
                .insert(changed.pane_id.clone(), changed.title.clone());
            let mut events = vec![(
                "pane:title".to_string(),
                serde_json::json!({ "paneId": changed.pane_id, "title": changed.title }),
            )];
            if state.focused_pane.as_deref() == Some(changed.pane_id.as_str()) {
                events.push(("pane:focus".into(), focus_payload(&state, &changed.pane_id)));
            }
            events
        }
        Event::TabTitleChanged(changed) => {
            let state = state.lock();
            // The tab-bar focuses a tab by focusing one of its panes, so the
            // pane→tab map learned from PaneAdded is attached here.
            let pane_id = state
                .pane_tabs
                .iter()
                .find(|(_, tab_id)| tab_id.as_str() == changed.tab_id)
                .map(|(pane_id, _)| pane_id.clone());
            let active = state
                .focused_pane
                .as_ref()
                .and_then(|pane| state.pane_tabs.get(pane))
                .is_some_and(|tab| tab.as_str() == changed.tab_id);
            vec![(
                "tab:title".into(),
                serde_json::json!({
                    "tabId": changed.tab_id,
                    "title": changed.title,
                    "paneId": pane_id,
                    "active": active,
                }),
            )]
        }
        Event::SessionLayoutChanged(changed) => {
            if let Some(snapshot) = &changed.snapshot {
                hydration_events(snapshot, state)
            } else {
                let layout = changed
                    .layout
                    .as_ref()
                    .and_then(|tree| tree.root.as_ref())
                    .map(layout_node_json)
                    .unwrap_or(serde_json::Value::Null);
                vec![("session:layout".into(), layout)]
            }
        }
        Event::PaneAdded(added) => {
            state
                .lock()
                .pane_tabs
                .insert(added.pane_id.clone(), added.tab_id.clone());
            vec![(
                "pane:added".into(),
                serde_json::json!({ "paneId": added.pane_id, "tabId": added.tab_id }),
            )]
        }
        Event::PaneRemoved(removed) => {
            let mut state = state.lock();
            state.pane_tabs.remove(&removed.pane_id);
            state.pane_titles.remove(&removed.pane_id);
            if state.focused_pane.as_deref() == Some(removed.pane_id.as_str()) {
                state.focused_pane = None;
            }
            vec![(
                "pane:removed".into(),
                serde_json::json!({ "paneId": removed.pane_id, "exitCode": removed.exit_code }),
            )]
        }
        _ => Vec::new(),
    }
}
/// Expand an authoritative attach/reconnect snapshot into the same event
/// vocabulary used by incremental notifications. The state map is populated
/// before focus payloads are built, so focused titles and tab membership are
/// available even when no incremental event preceded the snapshot.
fn hydration_events(
    snapshot: &mux_protocol::SessionSnapshot,
    state: &Mutex<MuxBridgeState>,
) -> Vec<(String, serde_json::Value)> {
    let mut state = state.lock();
    let focused_pane = (!snapshot.focused_pane_id.is_empty())
        .then(|| snapshot.focused_pane_id.clone());
    let focus_changed = state.focused_pane != focused_pane;
    state.focused_pane = focused_pane;
    state.pane_tabs.clear();
    state.pane_titles.clear();
    for tab in &snapshot.tabs {
        for pane in &tab.panes {
            state.pane_tabs.insert(pane.id.clone(), tab.id.clone());
            state.pane_titles.insert(pane.id.clone(), pane.title.clone());
        }
    }

    let mut events = vec![(
        "session:layout".to_string(),
        snapshot
            .layout
            .as_ref()
            .and_then(|tree| tree.root.as_ref())
            .map(layout_node_json)
            .unwrap_or(serde_json::Value::Null),
    )];
    for tab in &snapshot.tabs {
        let first_pane = tab.panes.first().map(|pane| pane.id.clone());
        events.push((
            "tab:title".to_string(),
            serde_json::json!({
                "tabId": tab.id,
                "title": tab.title,
                "paneId": first_pane,
                "active": snapshot.focused_tab_id == tab.id,
            }),
        ));
        for pane in &tab.panes {
            events.push((
                "pane:added".to_string(),
                serde_json::json!({ "paneId": pane.id, "tabId": tab.id }),
            ));
            events.push((
                "pane:title".to_string(),
                serde_json::json!({ "paneId": pane.id, "title": pane.title }),
            ));
        }
    }
    if focus_changed {
        if let Some(pane_id) = state.focused_pane.clone() {
            events.push((
                "pane:focus".to_string(),
                serde_json::json!({
                    "paneId": pane_id,
                    "id": pane_id,
                    "title": state.pane_titles.get(&pane_id).cloned().unwrap_or_default(),
                    "tabId": state.pane_tabs.get(&pane_id).cloned(),
                    "sessionName": state.session_name.clone().unwrap_or_default(),
                }),
            ));
        }
    }
    events
}

// ---------------------------------------------------------------------------
// §5.6 First-install consent store
//
// Extensions only run after the user approved what their manifest asks for
// (browser-extension style). Consent is keyed by extension id plus the exact
// policy fingerprint of the security-relevant manifest fields, so an update
// that changes capabilities, limits, side or version re-prompts. Each record
// stores an explicit Approved/Denied state — there is no sentinel value — and
// legacy or malformed records fail closed into pending, never activating.
// ---------------------------------------------------------------------------

/// §5.6 The user's decision for one exact policy fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentState {
    Approved,
    Denied,
}

/// §5.6 One persisted first-install consent decision. The record is keyed by
/// extension id plus the exact policy fingerprint that was decided, so it only
/// ever matches the manifest it was made against: a changed manifest
/// re-prompts regardless of the prior Approved/Denied state.
///
/// The record is serializable through [`Self::to_json`] / [`Self::from_json`]
/// as `{"id", "policy_fingerprint", "state"}` with an explicit
/// `"approved"/"denied"` state — no sentinel values.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsentRecord {
    pub id: String,
    pub policy_fingerprint: String,
    pub state: ConsentState,
}

impl ConsentRecord {
    pub fn approved(id: impl Into<String>, policy_fingerprint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            policy_fingerprint: policy_fingerprint.into(),
            state: ConsentState::Approved,
        }
    }

    pub fn denied(id: impl Into<String>, policy_fingerprint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            policy_fingerprint: policy_fingerprint.into(),
            state: ConsentState::Denied,
        }
    }

    fn state_name(state: ConsentState) -> &'static str {
        match state {
            ConsentState::Approved => "approved",
            ConsentState::Denied => "denied",
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "policy_fingerprint": self.policy_fingerprint,
            "state": Self::state_name(self.state),
        })
    }

    /// Parse a record from its JSON shape. Returns `None` for any record that
    /// does not carry the explicit state and policy fingerprint the current
    /// format requires — including legacy records that stored a bare numeric
    /// fingerprint — so such records fail closed into pending.
    fn from_json(value: &serde_json::Value) -> Option<ConsentRecord> {
        let id = value.get("id")?.as_str()?.to_string();
        let policy_fingerprint = value.get("policy_fingerprint")?.as_str()?.to_string();
        let state = match value.get("state")?.as_str()? {
            "approved" => ConsentState::Approved,
            "denied" => ConsentState::Denied,
            _ => return None,
        };
        Some(ConsentRecord {
            id,
            policy_fingerprint,
            state,
        })
    }
}

/// §5.6 One extension waiting for the user's first-install decision.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub id: String,
    pub version: String,
    pub capabilities_summary: String,
    pub policy_fingerprint: String,
}

fn consent_file_path() -> PathBuf {
    paths::config_dir().join("extension-consent.json")
}

/// Load consent records as `id → ConsentRecord`. An absent or corrupt file is
/// treated as empty (fail closed: nothing runs unapproved, nothing crashes).
/// Records that do not carry the explicit state and policy fingerprint the
/// current format requires — including legacy records that stored a bare
/// numeric fingerprint — are skipped and fall through to pending.
fn load_consent_records(path: &Path) -> HashMap<String, ConsentRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "extension consent file unreadable; treating as empty"
            );
            return HashMap::new();
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "extension consent file corrupt; treating as empty"
            );
            return HashMap::new();
        }
    };
    let Some(records) = value.as_array() else {
        tracing::warn!(
            path = %path.display(),
            "extension consent file is not an array; treating as empty"
        );
        return HashMap::new();
    };
    let mut consented = HashMap::new();
    for record in records {
        let Some(record) = ConsentRecord::from_json(record) else {
            tracing::warn!(
                path = %path.display(),
                %record,
                "skipping malformed extension consent record"
            );
            continue;
        };
        consented.insert(record.id.clone(), record);
    }
    consented
}

/// Persist consent records as `[{"id", "policy_fingerprint", "state"}]`,
/// sorted by id for stable output. Writes are temp-file + rename so a crash
/// mid-write cannot corrupt the store. Errors are returned to the caller:
/// pending and host state must not be mutated when the store could not be
/// written.
fn save_consent_records(path: &Path, records: &HashMap<String, ConsentRecord>) -> Result<()> {
    let mut ids: Vec<&String> = records.keys().collect();
    ids.sort();
    let records: Vec<serde_json::Value> = ids.iter().map(|id| records[*id].to_json()).collect();
    let serialized = serde_json::to_string_pretty(&records)
        .context("serializing extension consent records failed")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating consent directory {}", parent.display()))?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized)
        .with_context(|| format!("writing extension consent file {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("committing extension consent file {}", path.display()))?;
    Ok(())
}

/// §5.6 Canonical policy fingerprint: delegates to the shared
/// [`quickjs_runtime::ExtensionManifest::policy_fingerprint`] so the client
/// consent store and the daemon approval ledger always compute byte-identical
/// fingerprints from one implementation.
fn consent_fingerprint(extension: &DiscoveredExtension) -> String {
    extension.manifest.policy_fingerprint()
}

/// Human-readable capability list for the first-install prompt.
fn capabilities_summary(capabilities: &ExtensionCapabilities) -> String {
    let mut granted: Vec<&str> = Vec::new();
    if capabilities.terminal {
        granted.push("terminal");
    }
    if capabilities.mux {
        granted.push("mux");
    }
    if capabilities.workspace {
        granted.push("workspace");
    }
    if capabilities.settings {
        granted.push("settings");
    }
    if capabilities.network {
        granted.push("network");
    }
    if capabilities.process_spawn {
        granted.push("process spawn");
    }
    match capabilities.filesystem {
        FilesystemAccess::None => {}
        FilesystemAccess::Cwd => granted.push("filesystem (working directory)"),
        FilesystemAccess::Home => granted.push("filesystem (home directory)"),
    }
    if granted.is_empty() {
        "none".to_string()
    } else {
        granted.join(", ")
    }
}

fn pending_approval_for(extension: &DiscoveredExtension) -> PendingApproval {
    PendingApproval {
        id: extension.manifest.id.clone(),
        version: extension.manifest.version.clone(),
        capabilities_summary: capabilities_summary(&extension.manifest.capabilities),
        policy_fingerprint: consent_fingerprint(extension),
    }
}


// ---------------------------------------------------------------------------
// §5.2 Dedicated-thread extension host actor.
//
// Owns LiveExtensions on a single std::thread, satisfying the §5.2
// "dedicated OS thread" constraint. The host thread receives HostCommand
// messages via mpsc and pushes rendered chrome back over an async channel.
// All QuickJS ctx.with calls happen only on this thread.
// ---------------------------------------------------------------------------

enum HostCommand {
    /// §5.4 Install the mux bridge once the daemon connection exists. The
    /// bridge is built per extension with its declared filesystem scope, so
    /// the command carries the connection parts rather than one shared bridge.
    InstallBridge {
        domain: Arc<mux::MuxDomain>,
        state: Arc<Mutex<MuxBridgeState>>,
    },
    Emit {
        event: String,
        payload: String,
    },
    /// §16.7 Execute a command on exactly the extension that owns it. The
    /// client-side registry resolves `owner` before the message is sent; the
    /// host thread never broadcasts a command to other runtimes.
    ExecuteCommand {
        owner: Option<String>,
        command: String,
        arguments: String,
    },
    /// Force a full re-render regardless of invalidation state.
    Render,
    /// §5.4 Re-invoke only the display-list renderer methods; the resulting
    /// draw ops refresh the cached regions without re-rendering or
    /// re-serializing the full VDOM.
    RenderDisplayLists,
    /// §5.6 The user approved these pending extensions: activate them.
    Approve { ids: Vec<String> },
    /// §5.6 The user denied these pending extensions: drop them.
    Deny { ids: Vec<String> },
    Shutdown,
}

pub struct ExtensionHostController {
    command_sender: Option<std::sync::mpsc::Sender<HostCommand>>,
    host_thread: Option<std::thread::JoinHandle<()>>,
    /// Applies chrome the host thread pushes; replaces the old 1Hz poll.
    chrome_task: Option<gpui::Task<()>>,
    /// Applies display-list refreshes the host thread pushes; mirrors
    /// `chrome_task` so ticking regions repaint without VDOM reconciliation.
    display_list_task: Option<gpui::Task<()>>,
    /// Invalidates display-list clock views without running on the render thread.
    clock_task: Option<gpui::Task<()>>,
    /// Forwards mux notifications into the extensions as events.
    mux_task: Option<gpui::Task<()>>,
    /// §16.9 The mux connection used to route server-chrome clicks back to
    /// the authoritative daemon-side extension host. `None` until
    /// `start_mux_task` observes the domain; without it a server-chrome
    /// action is logged and dropped (fail closed) — never executed against
    /// client-side extensions.
    mux_domain: Option<Arc<mux::MuxDomain>>,
    status_bars:
        parking_lot::Mutex<Vec<gpui::WeakEntity<crate::extension_status_bar::ExtensionStatusBar>>>,
    /// Last local render and authoritative server views are kept separately so
    /// either source can update without duplicating the other.
    local_chrome: Vec<VDomNode>,
    server_chrome: BTreeMap<(String, String), VDomNode>,
    /// §5.6 Extensions waiting for the user's first-install decision.
    pending_approvals: Vec<PendingApproval>,
    /// Applies the host's pending-approval pushes; mirrors `chrome_task`.
    pending_task: Option<gpui::Task<()>>,
    /// §16.7 Client-side command ownership (id -> owning extension) plus the
    /// normalized chord -> action keymaps the pane resolvers read.
    commands: CommandRegistry,
    /// §16.7 Chord -> action snapshot shared with every MuxPaneView
    /// shortcut resolver; refreshed whenever activation reports land.
    keymap_snapshot: Arc<Mutex<BTreeMap<String, String>>>,
    /// Applies registry reports pushed by the host thread, mirroring
    /// `pending_task`.
    registry_task: Option<gpui::Task<()>>,
    /// §16.7 Synthetic chrome announcing registry conflicts (command id or
    /// chord collisions, or chords shadowed by native bindings), appended
    /// after the live chrome so the rejected registration is never silent.
    registry_notices: Vec<VDomNode>,
    /// §5.6 Controller-global prompt claim: at most one window may present
    /// the first-install prompt for the pending batch at a time.
    prompt_claimed: bool,
    /// Consent store location. Defaults to the config dir; tests redirect it
    /// so they never touch (or share) the real user's consent file.
    consent_file: PathBuf,
}

pub struct GlobalHostController(pub gpui::Entity<ExtensionHostController>);
impl gpui::Global for GlobalHostController {}

/// §5.4 A display-list region's fresh draw ops, produced by re-invoking only
/// the registered renderer methods on the host thread. Applied to status bars
/// without touching the VDOM set, so a ticking clock never invalidates the
/// surrounding chrome.
pub struct DisplayListUpdate {
    pub region_id: String,
    pub ops: Vec<vdom_bridge::DrawOp>,
}

/// §5.4 Parse one refreshed region's drawOps JSON with the same native bounds
/// as the VDOM path: the serialized payload and the op count are capped so a
/// pathological renderer output is rejected before it is painted.
fn parse_display_list_json(json: &str) -> Result<Vec<vdom_bridge::DrawOp>> {
    if json.len() > vdom_bridge::MAX_VDOM_PAYLOAD_BYTES {
        bail!(
            "display list payload of {} bytes exceeds limit of {}",
            json.len(),
            vdom_bridge::MAX_VDOM_PAYLOAD_BYTES
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(json).context("display list JSON invalid")?;
    vdom_bridge::parse_display_list(&value).context("display list parse failed")
}

/// §5.4 A live extension plus the host-side state that spec §5.6 requires: an
/// extension that blows its CPU budget is suspended rather than left to keep
/// burning the host thread on every subsequent event.
struct HostedExtension {
    live: LiveExtension,
    suspended: bool,
    /// §5.6 Why the extension was suspended, surfaced through the chrome
    /// notice so the user sees an actionable reason. `None` while active.
    suspension_reason: Option<&'static str>,
}

impl HostedExtension {
    /// §5.6 "Resource limits are enforced at runtime — exceeding them results
    /// in extension suspension." Suspension lasts for the process lifetime;
    /// the chrome falls back to the native GPUI baseline (§5.1).
    ///
    /// Called after every host → extension interaction (render and each
    /// command), so a runaway handler is suspended before the next command
    /// even if nothing requested a re-render.
    fn note_resource_violations(&mut self) {
        // Extensions report internally caught exceptions (render/event
        // handlers) through the error list; drain it here on every path so
        // nothing is silently dropped. "out of memory" in that list is a
        // resource violation the JS layer swallowed — it must suspend too.
        match self.live.take_errors() {
            Ok(errors) => {
                for error in errors {
                    tracing::warn!(id = %self.live.id(), %error, "extension reported an error");
                }
            }
            Err(error) => {
                tracing::warn!(id = %self.live.id(), %error, "draining extension errors failed");
            }
        }
        if self.live.take_cpu_interrupted() {
            self.suspended = true;
            self.suspension_reason = Some("CPU budget exceeded");
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its CPU budget and was suspended"
            );
            return;
        }
        if self.live.take_memory_violated() {
            self.suspended = true;
            self.suspension_reason = Some("memory budget exceeded");
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its memory budget and was suspended"
            );
        }
        // §5.6 IO quota rejection is flagged in Rust at the token bucket, so
        // it survives an extension's JS try/catch; the flag is the only
        // reliable signal that the extension exceeded its `io_rate_limit`.
        if self.live.take_io_violated() {
            self.suspended = true;
            self.suspension_reason = Some("io rate limit exceeded");
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its IO rate limit and was suspended"
            );
        }
    }
}

/// §16.7 One command an extension registered during activation.
#[derive(Debug, Clone)]
struct RegisteredCommand {
    id: String,
    label: String,
}
/// §16.7 A registration that could not take effect, in terms a user can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryConflict {
    /// The extension whose registration lost.
    pub extension_id: String,
    /// What was rejected and why.
    pub detail: String,
}



/// §16.7 One keymap binding an extension declared during activation.
#[derive(Debug, Clone)]
struct RegisteredKeymap {
    chord: String,
    command: String,
}

/// §16.7 Activation report for one extension: the commands and keymaps it
/// registered, so the client-side ownership registry can be rebuilt after
/// every activation or approval without querying the host thread.
#[derive(Debug, Clone)]
struct RegistryReport {
    extension_id: String,
    commands: Vec<RegisteredCommand>,
    keymaps: Vec<RegisteredKeymap>,
}



/// §16.7 Parse the host's `[{id, label}]` command list. Entries without an
/// id are skipped; a malformed payload degrades to an empty list (logged),
/// never a failed report.
fn parse_registered_commands(json: &str, extension_id: &str) -> Vec<RegisteredCommand> {
    let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        tracing::warn!(id = %extension_id, "extension command list malformed");
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(|id| {
                    let label = entry
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .filter(|label| !label.is_empty())
                        .unwrap_or(id);
                    RegisteredCommand {
                        id: id.to_string(),
                        label: label.to_string(),
                    }
                })
        })
        .collect()
}

/// §16.7 Parse the host's `[{chord, command}]` keymap list. Entries missing
/// either field are skipped; a malformed payload degrades to an empty list
/// (logged), never a failed report.
fn parse_registered_keymaps(json: &str, extension_id: &str) -> Vec<RegisteredKeymap> {
    let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        tracing::warn!(id = %extension_id, "extension keymap list malformed");
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let chord = entry.get("chord").and_then(serde_json::Value::as_str)?;
            let command = entry.get("command").and_then(serde_json::Value::as_str)?;
            Some(RegisteredKeymap {
                chord: chord.to_string(),
                command: command.to_string(),
            })
        })
        .collect()
}

impl RegistryReport {
    /// Collect one report from a live extension. A broken or unreadable
    /// registration list degrades to an empty list (logged), never to a
    /// silent drop of the extension's other registrations.
    fn from_live(hosted: &HostedExtension) -> RegistryReport {
        let extension_id = hosted.live.id().to_string();
        let commands = match hosted.live.list_commands() {
            Ok(json) => parse_registered_commands(&json, &extension_id),
            Err(error) => {
                tracing::warn!(id = %extension_id, %error, "extension command list failed");
                Vec::new()
            }
        };
        let keymaps = match hosted.live.list_keymaps() {
            Ok(json) => parse_registered_keymaps(&json, &extension_id),
            Err(error) => {
                tracing::warn!(id = %extension_id, %error, "extension keymap list failed");
                Vec::new()
            }
        };
        RegistryReport {
            extension_id,
            commands,
            keymaps,
        }
    }
}

/// §16.7 Collect activation reports from every live, non-suspended extension.
fn registry_reports(live_extensions: &[HostedExtension]) -> Vec<RegistryReport> {
    live_extensions
        .iter()
        .filter(|hosted| !hosted.suspended)
        .map(RegistryReport::from_live)
        .collect()
}
// ---------------------------------------------------------------------------
// §16.7 Command ownership registry and external directory sharing
// ---------------------------------------------------------------------------

/// §16.7 Build the `[{id, label}]` external directory for one extension:
/// every command the *other* live extensions registered. The id is namespaced
/// so a click resolves to exactly one owner, and the label carries the source
/// extension so discovery surfaces can show ownership.
fn external_command_directory(
    reports: &[RegistryReport],
    for_extension: &str,
) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for report in reports {
        if report.extension_id == for_extension {
            continue;
        }
        for command in &report.commands {
            // Namespaced, not bare: the palette entry is what comes back on
            // click, and two extensions may register the same bare id. A bare
            // id would resolve through the first-wins index and run whichever
            // extension registered first, not the one the user picked.
            entries.push(serde_json::json!({
                "id": format!("{}.{}", report.extension_id, command.id),
                "label": format!("{} — {}", command.label, report.extension_id),
            }));
        }
    }
    entries
}

/// §16.7 Push the merged external directory into every live extension so
/// `context.commands.list()` exposes all enabled extensions' commands.
fn install_external_directories(live_extensions: &mut [HostedExtension], reports: &[RegistryReport]) {
    let entries_json: Vec<(String, String)> = live_extensions
        .iter()
        .filter(|hosted| !hosted.suspended)
        .map(|hosted| {
            let extension_id = hosted.live.id().to_string();
            let entries = external_command_directory(reports, &extension_id);
            let json =
                serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
            (extension_id, json)
        })
        .collect();
    for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended) {
        let extension_id = hosted.live.id().to_string();
        if let Some((_, json)) = entries_json.iter().find(|(id, _)| id == &extension_id) {
            if let Err(error) = hosted.live.install_external_commands(json) {
                tracing::warn!(id = %extension_id, %error, "installing external command directory failed");
            }
        }
    }
}

#[derive(Default)]
struct CommandRegistry {
    /// namespaced command id -> owning extension id
    owners: BTreeMap<String, String>,
    /// namespaced command id -> display label
    labels: BTreeMap<String, String>,
    /// bare command id -> owning extension id (first registration wins)
    bare_owners: BTreeMap<String, String>,
    /// normalized chord -> command id as declared by the owner
    keymaps: BTreeMap<String, String>,
    /// normalized chord -> owning extension id (first registration wins)
    chord_owners: BTreeMap<String, String>,
    /// parsed keystroke -> command id, so the app-wide interceptor matches
    /// real keystrokes without re-parsing every chord on every keypress
    shortcut_entries: HashMap<Keystroke, String>,
}

/// Drop `key_char` so a declared chord matches the keystroke the platform
/// actually reports.
///
/// `Keystroke` hashes `key_char`, but a parsed chord never carries one while
/// macOS fills it in for every key without ctrl/cmd/fn — and unconditionally
/// for space, tab and enter. Keying on the whole struct therefore missed
/// exactly the chords the user is most likely to declare.
fn chord_lookup_key(keystroke: &Keystroke) -> Keystroke {
    Keystroke {
        modifiers: keystroke.modifiers,
        key: keystroke.key.clone(),
        key_char: None,
    }
}

/// One command the client-side directory exposes to discovery surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCommandEntry {
    /// The extension that registered the command.
    pub extension_id: String,
    /// The bare command id the extension declared.
    pub command_id: String,
    /// The namespaced form (`<extension>.<command>`), always unique.
    pub namespaced_id: String,
    /// The display label declared by the extension.
    pub label: String,
}

/// §16.7 Normalize a declared chord to gpui's hyphen-separated form so it
/// can be matched against a parsed keystroke (`ctrl+shift+p` ->
/// `ctrl-shift-p`).
fn normalize_chord(chord: &str) -> String {
    chord
        .split(['+', '-'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

impl CommandRegistry {
    /// Apply one extension's activation report. Returns the registrations
    /// rejected because another extension already owns the id or chord.
    fn apply_report(&mut self, report: &RegistryReport) -> Vec<RegistryConflict> {
        let mut rejected = Vec::new();
        for command in &report.commands {
            let namespaced = format!("{}.{}", report.extension_id, command.id);
            match self.bare_owners.entry(command.id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(report.extension_id.clone());
                    self.owners.insert(namespaced.clone(), report.extension_id.clone());
                    self.labels.insert(namespaced, command.label.clone());
                }
                // The same extension re-registering is idempotent.
                std::collections::btree_map::Entry::Occupied(entry)
                    if entry.get() == &report.extension_id => {}
                // Collision: a different extension already owns the bare id,
                // so only that one keeps it. The namespaced form is unique by
                // construction and stays dispatchable — §16.7 asks that the
                // user's pick decides, not the registration order.
                std::collections::btree_map::Entry::Occupied(entry) => {
                    self.owners
                        .insert(namespaced.clone(), report.extension_id.clone());
                    self.labels.insert(namespaced, command.label.clone());
                    rejected.push(RegistryConflict {
                        extension_id: report.extension_id.clone(),
                        detail: format!(
                            "command id \"{}\" is already owned by extension \"{}\"; \
                             use \"{}.{}\" to reach this one",
                            command.id,
                            entry.get(),
                            report.extension_id,
                            command.id
                        ),
                    });
                }
            }
        }
        for keymap in &report.keymaps {
            let chord = normalize_chord(&keymap.chord);
            match self.chord_owners.entry(chord.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(report.extension_id.clone());
                    // The resolver maps the chord to the owning extension's
                    // command id; the parsed entry feeds the app-wide
                    // keystroke interceptor without re-parsing on keypress.
                    self.keymaps.insert(chord.clone(), keymap.command.clone());
                    if let Ok(keystroke) = Keystroke::parse(&chord) {
                        self.shortcut_entries
                            .insert(chord_lookup_key(&keystroke), keymap.command.clone());
                    }
                }
                std::collections::btree_map::Entry::Occupied(entry)
                    if entry.get() == &report.extension_id => {
                    self.keymaps.insert(chord.clone(), keymap.command.clone());
                    if let Ok(keystroke) = Keystroke::parse(&chord) {
                        self.shortcut_entries
                            .insert(chord_lookup_key(&keystroke), keymap.command.clone());
                    }
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    rejected.push(RegistryConflict {
                        extension_id: report.extension_id.clone(),
                        detail: format!(
                            "chord \"{}\" is already bound by extension \"{}\"",
                            keymap.chord,
                            entry.get()
                        ),
                    });
                }
            }
        }
        rejected
    }

    /// Apply a full snapshot of activation reports, replacing prior
    /// ownership (an extension that unregistered at runtime stops owning
    /// its commands). Returns the registrations rejected for colliding
    /// with a different extension's registration.
    fn apply_reports(&mut self, reports: &[RegistryReport]) -> Vec<RegistryConflict> {
        *self = CommandRegistry::default();
        let mut rejected = Vec::new();
        for report in reports {
            rejected.extend(self.apply_report(report));
        }
        rejected
    }

    /// Resolve the extension that owns `command`: namespaced ids resolve
    /// directly, bare ids through the first-wins index.
    fn resolve_owner(&self, command: &str) -> Option<&str> {
        self.owners
            .get(command)
            .map(String::as_str)
            .or_else(|| self.bare_owners.get(command).map(String::as_str))
    }

    /// The resolver snapshot: normalized chord -> command id.
    fn keymap_snapshot(&self) -> BTreeMap<String, String> {
        self.keymaps.clone()
    }

    /// Every currently owned command, namespaced, with its display label.
    fn command_directory(&self) -> Vec<ExtensionCommandEntry> {
        self.owners
            .iter()
            .map(|(namespaced_id, extension_id)| ExtensionCommandEntry {
                extension_id: extension_id.clone(),
                command_id: namespaced_id
                    .strip_prefix(&format!("{extension_id}."))
                    .unwrap_or(namespaced_id)
                    .to_string(),
                namespaced_id: namespaced_id.clone(),
                label: self
                    .labels
                    .get(namespaced_id)
                    .cloned()
                    .unwrap_or_else(|| namespaced_id.clone()),
            })
            .collect()
    }

    /// Resolve a real keystroke against the declared chords, returning the
    /// owning extension and the command it bound to the chord.
    fn resolve_keystroke(
        &self,
        keystroke: &Keystroke,
    ) -> Option<(String, String)> {
        self.shortcut_entries
            .get(&chord_lookup_key(keystroke))
            .and_then(|command| {
                self.resolve_owner(command)
                    .map(|owner| (owner.to_string(), command.clone()))
            })
    }
}

/// §16.7 Execute a command on exactly the extension the client-side
/// registry resolved as its owner. Returns the number of runtimes that
/// executed it (0 or 1): the owner may be gone or suspended, or the command
/// may have been unregistered at runtime.
fn execute_for_owner(
    live_extensions: &mut [HostedExtension],
    owner: Option<&str>,
    command: &str,
    arguments: &str,
) -> usize {
    let Some(hosted) = live_extensions
        .iter()
        .find(|hosted| Some(hosted.live.id()) == owner)
    else {
        tracing::warn!(
            ?owner,
            %command,
            "no extension matches the command owner; dispatch dropped"
        );
        return 0;
    };
    if hosted.suspended {
        tracing::warn!(
            id = %hosted.live.id(),
            %command,
            "command dropped: owning extension is suspended"
        );
        return 0;
    }
    match hosted.live.execute_command(command, arguments) {
        Ok(true) => 1,
        Ok(false) => {
            tracing::warn!(
                id = %hosted.live.id(),
                %command,
                "command not registered in the owning extension"
            );
            0
        }
        Err(error) => {
            tracing::warn!(
                id = %hosted.live.id(),
                %command,
                %error,
                "extension command failed"
            );
            0
        }
    }
}

// ---------------------------------------------------------------------------
// §16.7 Global command surface: palette listing and keystroke dispatch.
//
// One action type carries every extension command through the native
// discovery and keybinding systems. The extension identity travels in the
// action itself, so dispatch is exact even when two extensions declare the
// same command id — the registry's first-wins rule guarantees at most one
// owner is ever listed, and execution targets that owner by id.
// ---------------------------------------------------------------------------

/// Invokes a command registered by an extension. Produced by the command
/// palette interceptor and by extension-declared chords; handled by the
/// workspace, which routes it to the owning extension's host thread.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = extension)]
#[serde(deny_unknown_fields)]
pub struct InvokeExtensionCommand {
    /// The extension that registered the command.
    pub extension_id: String,
    /// The bare command id the extension declared.
    pub command: String,
    /// JSON arguments to pass to the command handler.
    #[serde(default)]
    pub arguments: String,
}

/// §16.7 Pure interception step: fuzzy-match `query` against the command
/// directory and produce palette items whose action is an exact
/// `InvokeExtensionCommand`. Kept free of GPUI state so ownership and
/// collision behavior are unit-testable.
pub fn palette_interception(
    query: &str,
    directory: &[ExtensionCommandEntry],
) -> command_palette_hooks::CommandInterceptResult {
    use command_palette_hooks::{CommandInterceptItem, CommandInterceptResult};

    let candidates: Vec<fuzzy_nucleo::StringMatchCandidate> = directory
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            fuzzy_nucleo::StringMatchCandidate::new(
                index,
                format!("{} — {}", entry.label, entry.extension_id),
            )
        })
        .collect();
    let matches = fuzzy_nucleo::match_strings(
        &candidates,
        query,
        fuzzy_nucleo::Case::Ignore,
        fuzzy_nucleo::LengthPenalty::Off,
        100,
    );
    let results = matches
        .into_iter()
        .filter_map(|string_match| {
            let entry = directory.get(string_match.candidate_id)?;
            Some(CommandInterceptItem {
                action: Box::new(InvokeExtensionCommand {
                    extension_id: entry.extension_id.clone(),
                    command: entry.command_id.clone(),
                    arguments: String::new(),
                }),
                string: format!("{} — {}", entry.label, entry.extension_id),
                positions: string_match.positions,
            })
        })
        .collect();
    CommandInterceptResult {
        results,
        exclusive: false,
    }
}

/// §16.7 Install the command palette interceptor. Reads the live directory
/// on every keystroke, so disable/unload/suspend/reload is reflected in the
/// palette immediately rather than on the next host push.
pub fn install_command_palette_interception(cx: &mut gpui::App) {
    command_palette_hooks::GlobalCommandPaletteInterceptor::set(
        cx,
        move |query, _workspace, cx| {
            let directory = cx
                .try_global::<GlobalHostController>()
                .map(|host| host.0.read(cx).command_directory())
                .unwrap_or_default();
            Task::ready(palette_interception(&query, &directory))
        },
    );
}

/// §16.7 Install the app-wide keystroke interceptor for extension chords.
///
/// Precedence is fail-safe: a keystroke that matches any native binding —
/// including a pending multi-key chord — is left untouched, so extension
/// chords can never shadow split-pane, focus-pane, attach, settings, or
/// kill-session keybindings. Only a chord with no native consumer at all
/// dispatches its owning extension's command, and propagation stops so the
/// pane-level shortcut resolver does not fire a second time.
pub fn install_global_shortcuts(cx: &mut gpui::App) {
    // The subscription unsubscribes on drop, so discarding it here would
    // uninstall the interceptor on the same line that installs it.
    cx.intercept_keystrokes(move |event, _window, cx| {
        let Some(host) = cx.try_global::<GlobalHostController>() else {
            return;
        };
        let Some((extension_id, command)) = host.0.read(cx).resolve_shortcut(&event.keystroke)
        else {
            return;
        };
        // Native bindings win: exact single-key bindings via
        // `all_bindings_for_input`, plus any longer chord that this
        // keystroke could be the prefix of (`bindings_for_input`'s pending
        // flag), so an extension chord never amputates a native sequence.
        if !cx
            .all_bindings_for_input(std::slice::from_ref(&event.keystroke))
            .is_empty()
        {
            return;
        }
        let context_stack = event.context_stack.clone();
        let keymap = cx.key_bindings();
        if keymap
            .borrow()
            .bindings_for_input(std::slice::from_ref(&event.keystroke), &context_stack)
            .1
        {
            return;
        }
        host.0.read(cx).invoke_command(&extension_id, &command, "{}");
        cx.stop_propagation();
    })
    .detach();
}

/// Render every live extension and hand the VDOM JSON back, logging (never
/// discarding) whatever each extension reported as an internal error.
fn render_live_extensions(live_extensions: &mut [HostedExtension]) -> Vec<VDomNode> {
    let mut nodes = Vec::new();
    for hosted in live_extensions.iter_mut() {
        if hosted.suspended {
            continue;
        }

        let extension_id = hosted.live.id().to_string();
        let mut rendered_nodes = Vec::new();
        match hosted.live.render_all_views() {
            Ok(views) => {
                for json in views {
                    match parse_vdom_json(&json) {
                        Ok(node) => rendered_nodes.push(node),
                        Err(error) => {
                            tracing::warn!(id = %extension_id, %error, "extension VDOM rejected")
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(id = %extension_id, %error, "extension render failed");
            }
        }

        // Check violations before publishing this frame. If the extension
        // exceeded a limit while rendering, its frame is discarded so native
        // chrome can replace it immediately instead of displaying the last
        // untrusted view.
        hosted.note_resource_violations();
        if !hosted.suspended {
            nodes.extend(rendered_nodes);
        }
    }
    nodes
}

/// §5.4 Render only when an extension asked for it (`view.invalidate()` /
/// `context.render()`), then push the merged chrome to the GPUI side.
fn push_chrome_if_dirty(
    live_extensions: &mut [HostedExtension],
    sender: &futures::channel::mpsc::UnboundedSender<Vec<VDomNode>>,
    force: bool,
) -> bool {
    let dirty = force
        || live_extensions.iter().any(|hosted| {
            !hosted.suspended && hosted.live.needs_render().unwrap_or_else(|error| {
            tracing::warn!(id = %hosted.live.id(), %error, "extension invalidation check failed");
            false
        })
        });
    if !dirty {
        return true;
    }
    let mut nodes = render_live_extensions(live_extensions);
    // §5.6 Suspension must not be silent: every chrome push appends a
    // synthetic notice per suspended extension after the live chrome.
    nodes.extend(suspension_notices(live_extensions));
    match sender.unbounded_send(nodes) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, "extension chrome receiver dropped");
            false
        }
    }
}

/// §5.6 Synthetic chrome nodes announcing suspended extensions, appended
/// after the live chrome so the user sees why an extension's views vanished.
/// Each notice names the extension and the resource reason it was suspended
/// for (CPU/memory budget or IO rate limit).
fn suspension_notices(live_extensions: &[HostedExtension]) -> Vec<VDomNode> {
    live_extensions
        .iter()
        .filter(|hosted| hosted.suspended)
        .map(|hosted| VDomNode {
            element_type: "div".to_string(),
            props: BTreeMap::new(),
            style: BTreeMap::new(),
            children: vec![VDomChild::Text(format!(
                "{} suspended ({})",
                hosted.live.id(),
                hosted.suspension_reason.unwrap_or("resource limit")
            ))],
        })
        .collect()
}

/// §5.6 Deliver one authorized host event to every non-suspended extension,
/// returning the total number of handler invocations that ran. Capability
/// filtering happens before entering JavaScript, so an extension cannot
/// subscribe around a manifest denial. A suspended extension is skipped here
/// (and in every other dispatch point) for the rest of the process lifetime.
fn deliver_event(live_extensions: &mut [HostedExtension], event: &str, payload: &str) -> usize {
    let mut delivered = 0;
    for hosted in live_extensions.iter_mut().filter(|hosted| {
        !hosted.suspended && hosted.live.capabilities().allows_host_event(event)
    }) {
        match hosted.live.emit_event(event, payload) {
            Ok(count) => delivered += count,
            Err(error) => {
                tracing::warn!(id = %hosted.live.id(), %event, %error, "extension emit failed");
            }
        }
    }
    delivered
}
impl ExtensionHostController {
    pub fn new() -> Self {
        Self {
            command_sender: None,
            host_thread: None,
            chrome_task: None,
            display_list_task: None,
            clock_task: None,
            mux_task: None,
            mux_domain: None,
            status_bars: parking_lot::Mutex::new(Vec::new()),
            local_chrome: Vec::new(),
            server_chrome: BTreeMap::new(),
            pending_approvals: Vec::new(),
            pending_task: None,
            commands: CommandRegistry::default(),
            keymap_snapshot: Arc::new(Mutex::new(BTreeMap::new())),
            registry_task: None,
            registry_notices: Vec::new(),
            prompt_claimed: false,
            consent_file: consent_file_path(),
        }
    }

    pub fn start(&mut self, extensions_dir: &Path, cx: &mut gpui::Context<Self>) {
        self.start_with_roots(quickjs_runtime::extension_roots(extensions_dir), cx);
    }

    fn start_with_roots(&mut self, roots: Vec<std::path::PathBuf>, cx: &mut gpui::Context<Self>) {
        let (command_sender, command_receiver) = std::sync::mpsc::channel::<HostCommand>();
        let (chrome_sender, chrome_receiver) = futures::channel::mpsc::unbounded::<Vec<VDomNode>>();
        let (display_list_sender, display_list_receiver) =
            futures::channel::mpsc::unbounded::<Vec<DisplayListUpdate>>();
        let (pending_sender, pending_receiver) =
            futures::channel::mpsc::unbounded::<Vec<PendingApproval>>();
        let (registry_sender, registry_receiver) =
            futures::channel::mpsc::unbounded::<Vec<RegistryReport>>();
        let consent_file = self.consent_file.clone();

        let host_thread = std::thread::Builder::new()
            .name("quickjs-ext-host".into())
            .spawn(move || {
                let discovered = quickjs_runtime::discover_client_extensions(&roots);
                if discovered.is_empty() {
                    tracing::warn!(?roots, "no client extensions found");
                }
                // §5.6 Split by consent: an extension activates only when its
                // policy fingerprint matches a stored Approved record exactly,
                // and is suppressed only by an exact Denied record. A manifest
                // that differs from either prior decision — or a legacy/
                // malformed record that failed to load — lands in pending and
                // runs nothing until the user decides again.
                let consent = load_consent_records(&consent_file);
                let mut pending: Vec<DiscoveredExtension> = Vec::new();
                let mut consented: Vec<DiscoveredExtension> = Vec::new();
                for extension in discovered {
                    let fingerprint = consent_fingerprint(&extension);
                    match consent.get(&extension.manifest.id) {
                        Some(record)
                            if record.state == ConsentState::Approved
                                && record.policy_fingerprint == fingerprint =>
                        {
                            consented.push(extension);
                        }
                        Some(record)
                            if record.state == ConsentState::Denied
                                && record.policy_fingerprint == fingerprint =>
                        {
                            tracing::info!(
                                id = %extension.manifest.id,
                                "extension stays disabled: the user denied this exact manifest"
                            );
                        }
                        _ => pending.push(extension),
                    }
                }
                if !pending.is_empty()
                    && pending_sender
                        .unbounded_send(pending.iter().map(pending_approval_for).collect())
                        .is_err()
                {
                    return;
                }
                let mut live_extensions = activate_extensions(consented);
                let mut installed_mux: Option<(Arc<mux::MuxDomain>, Arc<Mutex<MuxBridgeState>>)> = None;

                // First paint: extensions register their chrome during
                // activate, so publish it before waiting for any event.
                if !push_chrome_if_dirty(&mut live_extensions, &chrome_sender, true) {
                    return;
                }
                // §16.7 Ship the activation reports so the client-side
                // command ownership registry and pane shortcut resolvers
                // see what was registered, and push the merged external
                // directory into every runtime so `commands.list()`
                // exposes all enabled extensions' commands.
                let reports = registry_reports(&live_extensions);
                if registry_sender.unbounded_send(reports.clone()).is_err() {
                    return;
                }
                install_external_directories(&mut live_extensions, &reports);

                loop {
                    let command = match command_receiver.recv() {
                        Ok(command) => command,
                        Err(_) => break,
                    };
                    let mut force_render = false;
                    match command {
                        HostCommand::InstallBridge { domain, state } => {
                            installed_mux = Some((domain.clone(), state.clone()));
                            // §5.6 每个扩展按自己声明的文件系统范围构造专属桥:
                            // `cwd` 声明不会被授予主目录访问权, `home` 声明不会
                            // 获得工作区外的路径。范围在桥构造时固化。
                            for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended) {
                                let bridge: Arc<dyn HostBridge> = Arc::new(MuxHostBridge::new(
                                    domain.clone(),
                                    state.clone(),
                                    hosted.live.capabilities().filesystem,
                                ));
                                if let Err(error) = hosted.live.install_bridge(bridge) {
                                    tracing::warn!(id = %hosted.live.id(), %error, "installing mux bridge failed");
                                }
                            }
                        }
                        HostCommand::Emit { event, payload } => {
                            deliver_event(&mut live_extensions, &event, &payload);
                        }
                        HostCommand::ExecuteCommand { owner, command, arguments } => {
                            execute_for_owner(
                                &mut live_extensions,
                                owner.as_deref(),
                                &command,
                                &arguments,
                            );
                        }
                        HostCommand::Render => {
                            if !push_chrome_if_dirty(&mut live_extensions, &chrome_sender, true) {
                                break;
                            }
                            continue;
                        }
                        HostCommand::RenderDisplayLists => {
                            // §5.4 refresh only the display-list regions: the
                            // tick re-invokes renderer methods and ships fresh
                            // draw ops, never the full VDOM. Fall through so
                            // the violation sweep still runs — renderer
                            // methods execute JS on the host thread too.
                            for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended)
                            {
                                let extension_id = hosted.live.id().to_string();
                                let mut updates = Vec::new();
                                match hosted.live.refresh_display_lists() {
                                    Ok(regions) => {
                                        for region in regions {
                                            match parse_display_list_json(&region.ops_json) {
                                                Ok(ops) => updates.push(DisplayListUpdate {
                                                    region_id: region.region_id,
                                                    ops,
                                                }),
                                                Err(error) => tracing::warn!(
                                                    id = %extension_id,
                                                    region = %region.region_id,
                                                    %error,
                                                    "extension display list refresh rejected"
                                                ),
                                            }
                                        }
                                    }
                                    Err(error) => tracing::warn!(
                                        id = %extension_id,
                                        %error,
                                        "extension display list refresh failed"
                                    ),
                                }
                                if !updates.is_empty()
                                    && display_list_sender.unbounded_send(updates).is_err()
                                {
                                    tracing::debug!(
                                        "extension controller dropped; ending display list forwarding"
                                    );
                                    break;
                                }
                            }
                        }
                        HostCommand::Approve { ids } => {
                            let (approved, remaining): (Vec<DiscoveredExtension>, Vec<DiscoveredExtension>) =
                                std::mem::take(&mut pending)
                                    .into_iter()
                                    .partition(|extension| ids.contains(&extension.manifest.id));
                            pending = remaining;
                            if !approved.is_empty() {
                                let existing = live_extensions.len();
                                live_extensions.extend(activate_extensions(approved));
                                // A bridge installed before the approval must
                                // reach the newly activated extensions too,
                                // each built with its own declared scope.
                                if let Some((domain, state)) = &installed_mux {
                                    for hosted in &live_extensions[existing..] {
                                        let bridge: Arc<dyn HostBridge> = Arc::new(MuxHostBridge::new(
                                            domain.clone(),
                                            state.clone(),
                                            hosted.live.capabilities().filesystem,
                                        ));
                                        if let Err(error) = hosted.live.install_bridge(bridge) {
                                            tracing::warn!(id = %hosted.live.id(), %error, "installing mux bridge into approved extension failed");
                                        }
                                    }
                                }
                                force_render = true;
                                // §16.7 Newly approved extensions register
                                // commands and keymaps during activate; the
                                // full-snapshot report keeps ownership, the
                                // resolver view, and the merged external
                                // directories consistent.
                                let reports = registry_reports(&live_extensions);
                                if registry_sender.unbounded_send(reports.clone()).is_err() {
                                    break;
                                }
                                install_external_directories(&mut live_extensions, &reports);
                            }
                        }
                        HostCommand::Deny { ids } => {
                            pending.retain(|extension| !ids.contains(&extension.manifest.id));
                        }
                        HostCommand::Shutdown => break,
                    }
                    // §5.6 A runaway handler is suspended before the next
                    // command, even if nothing requested a re-render.
                    let suspended_before = live_extensions
                        .iter()
                        .filter(|hosted| hosted.suspended)
                        .count();
                    for hosted in live_extensions.iter_mut() {
                        hosted.note_resource_violations();
                    }
                    let newly_suspended = live_extensions
                        .iter()
                        .filter(|hosted| hosted.suspended)
                        .count()
                        > suspended_before;
                    // §16.7 A suspended extension must stop offering commands:
                    // republish the snapshot so its entries leave the native
                    // palette, the chord index, and every other runtime's
                    // external directory instead of failing at dispatch time.
                    if newly_suspended {
                        let reports = registry_reports(&live_extensions);
                        if registry_sender.unbounded_send(reports.clone()).is_err() {
                            break;
                        }
                        install_external_directories(&mut live_extensions, &reports);
                    }
                    // §5.6 Force the push when a suspension first occurs so
                    // the user notice appears without waiting for invalidation.
                    if !push_chrome_if_dirty(
                        &mut live_extensions,
                        &chrome_sender,
                        force_render || newly_suspended,
                    ) {
                        break;
                    }
                }
            });

        match host_thread {
            Ok(host_thread) => {
                self.command_sender = Some(command_sender.clone());
                self.host_thread = Some(host_thread);
                self.start_chrome_task(chrome_receiver, cx);
                self.start_display_list_task(display_list_receiver, cx);
                self.start_pending_task(pending_receiver, cx);
                self.start_registry_task(registry_receiver, cx);
                self.start_clock_task(cx);
                self.start_mux_task(cx);
            }
            Err(error) => {
                tracing::error!(%error, "failed to start QuickJS extension host");
            }
        }
    }

    /// §5.5 Apply chrome pushed by the host thread. Event driven: the task
    /// parks on the channel instead of polling on a timer.
    fn publish_chrome(&mut self, cx: &mut gpui::Context<Self>) {
        let mut nodes = merge_chrome_nodes(&self.local_chrome, &self.server_chrome);
        nodes.extend(self.registry_notices.clone());
        self.status_bars.lock().retain(|status_bar| {
            let Some(status_bar) = status_bar.upgrade() else {
                return false;
            };
            let nodes_for_bar = nodes.clone();
            status_bar.update(cx, |status_bar, cx| {
                status_bar.set_vdom_nodes(nodes_for_bar, cx);
            });
            true
        });
        publish_vdom(cx, nodes);
    }

    fn apply_local_chrome(&mut self, nodes: Vec<VDomNode>, cx: &mut gpui::Context<Self>) {
        self.local_chrome = nodes;
        self.publish_chrome(cx);
    }

    fn apply_server_chrome_update(
        &mut self,
        update: mux_protocol::ExtensionChromeUpdate,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        apply_server_chrome_node(&mut self.server_chrome, update)?;
        self.publish_chrome(cx);
        Ok(())
    }

    fn start_chrome_task(
        &mut self,
        mut chrome_receiver: futures::channel::mpsc::UnboundedReceiver<Vec<VDomNode>>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.chrome_task = Some(cx.spawn(async move |this, cx| {
            while let Some(nodes) = chrome_receiver.next().await {
                let update = this.update(cx, |this, cx| {
                    this.apply_local_chrome(nodes, cx);
                });
                if let Err(error) = update {
                    tracing::debug!(%error, "extension controller dropped while applying chrome");
                    break;
                }
            }
        }));
    }

    /// §5.4 Apply display-list refreshes the host thread pushes, mirroring
    /// `start_chrome_task`: the task parks on the channel instead of polling,
    /// and each update replaces only the cached draw ops for its region.
    fn start_display_list_task(
        &mut self,
        mut display_list_receiver: futures::channel::mpsc::UnboundedReceiver<
            Vec<DisplayListUpdate>,
        >,
        cx: &mut gpui::Context<Self>,
    ) {
        self.display_list_task = Some(cx.spawn(async move |this, cx| {
            while let Some(updates) = display_list_receiver.next().await {
                let update = this.update(cx, |this, cx| {
                    this.apply_display_list_updates(updates, cx);
                });
                if let Err(error) = update {
                    tracing::debug!(
                        %error,
                        "extension controller dropped while applying display lists"
                    );
                    break;
                }
            }
        }));
    }

    /// §5.4 Publish refreshed draw ops to every live status bar. The VDOM set
    /// is untouched, so no full reconciliation happens; each region's cached
    /// ops are replaced and the status bar schedules its own repaint.
    fn apply_display_list_updates(
        &self,
        updates: Vec<DisplayListUpdate>,
        cx: &mut gpui::Context<Self>,
    ) {
        for update in updates {
            let region_id = update.region_id;
            let ops = update.ops;
            self.status_bars.lock().retain(|status_bar| {
                let Some(status_bar) = status_bar.upgrade() else {
                    return false;
                };
                status_bar.update(cx, |status_bar, cx| {
                    status_bar.set_display_list(&region_id, ops.clone(), cx);
                });
                true
            });
        }
    }
    /// §5.6 Apply pending-approval lists the host thread pushes, mirroring
    /// the chrome channel: the task parks on the channel, and each push
    /// replaces the controller's view of what awaits the user's decision.
    fn start_pending_task(
        &mut self,
        mut pending_receiver: futures::channel::mpsc::UnboundedReceiver<Vec<PendingApproval>>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.pending_task = Some(cx.spawn(async move |this, cx| {
            while let Some(approvals) = pending_receiver.next().await {
                let update = this.update(cx, |this, cx| {
                    this.apply_pending_approvals(approvals, cx);
                });
                if let Err(error) = update {
                    tracing::debug!(%error, "extension controller dropped while applying pending approvals");
                    break;
                }
            }
        }));
    }

    fn apply_pending_approvals(
        &mut self,
        approvals: Vec<PendingApproval>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.pending_approvals = approvals;
        cx.notify();
    }

    /// §16.7 Apply registry reports the host thread pushes, mirroring the
    /// pending-approval channel: the task parks on the channel, and each
    /// push replaces the controller's command ownership and keymap view.
    fn start_registry_task(
        &mut self,
        mut registry_receiver: futures::channel::mpsc::UnboundedReceiver<Vec<RegistryReport>>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.registry_task = Some(cx.spawn(async move |this, cx| {
            while let Some(reports) = registry_receiver.next().await {
                let update = this.update(cx, |this, cx| {
                    this.apply_registry_reports(reports, cx);
                });
                if let Err(error) = update {
                    tracing::debug!(
                        %error,
                        "extension controller dropped while applying registry reports"
                    );
                    break;
                }
            }
        }));
    }

    /// §16.7 Apply the host thread's activation reports: ownership is
    /// rebuilt from the full snapshot (first-wins, collisions rejected and
    /// surfaced as chrome notices), the shared keymap snapshot is refreshed
    /// for the pane shortcut resolvers, and chords that collide with a
    /// native binding are reported instead of silently firing twice.
    fn apply_registry_reports(&mut self, reports: Vec<RegistryReport>, cx: &mut gpui::Context<Self>) {
        let mut conflicts = self.commands.apply_reports(&reports);
        *self.keymap_snapshot.lock() = self.commands.keymap_snapshot();
        // Native bindings always win: a chord that matches anything already
        // in the app keymap can never fire the extension command, so the
        // registration is reported as a conflict rather than a dead chord.
        for (chord, command) in self.commands.keymap_snapshot() {
            if let Ok(keystroke) = Keystroke::parse(&chord) {
                if !cx.all_bindings_for_input(&[keystroke]).is_empty() {
                    let extension_id = self
                        .commands
                        .resolve_owner(&command)
                        .map(str::to_string)
                        .unwrap_or_else(|| "unknown".to_string());
                    conflicts.push(RegistryConflict {
                        extension_id,
                        detail: format!(
                            "chord \"{chord}\" is already used by a built-in keybinding"
                        ),
                    });
                }
            }
        }
        for conflict in &conflicts {
            tracing::warn!(extension = %conflict.extension_id, %conflict.detail, "extension registration rejected");
        }
        self.registry_notices = conflicts
            .iter()
            .map(|conflict| VDomNode {
                element_type: "div".to_string(),
                props: BTreeMap::new(),
                style: BTreeMap::new(),
                children: vec![VDomChild::Text(format!(
                    "{}: {}",
                    conflict.extension_id, conflict.detail
                ))],
            })
            .collect();
        self.publish_chrome(cx);
        cx.notify();
    }

    /// §16.7 Every currently owned command with its extension identity and
    /// display label, for the command palette and other discovery surfaces.
    pub fn command_directory(&self) -> Vec<ExtensionCommandEntry> {
        self.commands.command_directory()
    }

    /// §16.7 Resolve a real keystroke against the declared chords. Returns
    /// the owning extension and the command it bound to the chord, or
    /// `None` when no extension chord matches.
    pub fn resolve_shortcut(&self, keystroke: &Keystroke) -> Option<(String, String)> {
        self.commands.resolve_keystroke(keystroke)
    }

    /// §16.7 Execute a command on exactly the extension the caller names.
    /// The host thread never broadcasts: the owner filter is carried into
    /// `execute_for_owner`, so a name collision can never fan one invocation
    /// out to several extensions.
    pub fn invoke_command(&self, extension_id: &str, command: &str, arguments: &str) {
        self.send(HostCommand::ExecuteCommand {
            owner: Some(extension_id.to_string()),
            command: command.to_string(),
            arguments: arguments.to_string(),
        });
    }
    /// §5.6 Extensions waiting for the user's first-install decision.
    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        self.pending_approvals.clone()
    }

    /// §5.6 Record consent for the given pending extensions (keyed by the
    /// approved manifest's policy fingerprint) and tell the host to activate
    /// them. The proposed record set is persisted *before* pending entries are
    /// removed, observers are notified, or host commands are sent: on a write
    /// or rename failure the error is returned and pending/host state is left
    /// untouched.
    pub fn approve_extensions(&mut self, ids: &[String], cx: &mut gpui::Context<Self>) -> Result<()> {
        let mut records = load_consent_records(&self.consent_file);
        let mut approved = Vec::new();
        for approval in &self.pending_approvals {
            if ids.contains(&approval.id) {
                records.insert(
                    approval.id.clone(),
                    ConsentRecord::approved(&approval.id, &approval.policy_fingerprint),
                );
                approved.push(approval.id.clone());
            }
        }
        if approved.is_empty() {
            return Ok(());
        }
        save_consent_records(&self.consent_file, &records)?;
        self.pending_approvals
            .retain(|approval| !ids.contains(&approval.id));
        self.send(HostCommand::Approve { ids: approved });
        cx.notify();
        Ok(())
    }

    /// §5.6 Record an explicit denial of the given pending extensions' exact
    /// policy fingerprints so those manifests are never re-prompted, and tell
    /// the host to drop them. Persistence happens first, exactly as in
    /// [`Self::approve_extensions`]: on failure the error is returned and
    /// pending/host state is left untouched.
    pub fn deny_extensions(&mut self, ids: &[String]) -> Result<()> {
        let mut records = load_consent_records(&self.consent_file);
        let mut denied = Vec::new();
        for approval in &self.pending_approvals {
            if ids.contains(&approval.id) {
                records.insert(
                    approval.id.clone(),
                    ConsentRecord::denied(&approval.id, &approval.policy_fingerprint),
                );
                denied.push(approval.id.clone());
            }
        }
        if denied.is_empty() {
            return Ok(());
        }
        save_consent_records(&self.consent_file, &records)?;
        self.pending_approvals
            .retain(|approval| !ids.contains(&approval.id));
        self.send(HostCommand::Deny { ids: denied });
        Ok(())
    }

    /// §5.6 Atomically claim the right to prompt for the pending approvals.
    /// Returns false when another prompt already owns the claim; the claimant
    /// must release it once the prompt resolves — including on cancellation,
    /// error, or persistence failure — so a later check can prompt again.
    pub fn claim_pending_prompt(&mut self, _cx: &mut gpui::Context<Self>) -> bool {
        if self.prompt_claimed {
            return false;
        }
        self.prompt_claimed = true;
        true
    }

    /// §5.6 Release the prompt claim. Does not notify: a dismissed or failed
    /// prompt must not immediately re-prompt; the pending queue stays pending
    /// until the next pending push, window, or restart surfaces it again.
    pub fn release_pending_prompt(&mut self, _cx: &mut gpui::Context<Self>) {
        self.prompt_claimed = false;
    }

    fn start_clock_task(&mut self, cx: &mut gpui::Context<Self>) {
        self.clock_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                // §5.4 The tick refreshes display-list regions only; full
                // chrome renders stay invalidation driven. A full render is
                // still performed when an extension asks for one (events,
                // commands), so nothing is starved by dropping the old
                // force-render poll.
                if this
                    .update(cx, |this, _| this.send(HostCommand::RenderDisplayLists))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// §3.4/§5.4 Wait for the mux connection, install the host bridge and then
    /// forward mux notifications into the extensions as events.
    fn start_mux_task(&mut self, cx: &mut gpui::Context<Self>) {
        self.mux_task = Some(cx.spawn(async move |this, cx| {
            let mut domain = None;
            for _ in 0..MUX_BRIDGE_WAIT_ATTEMPTS {
                let candidate = cx.update(|cx| {
                    workspace::AppState::try_global(cx).and_then(|state| state.mux_domain.clone())
                });
                if let Some(candidate) = candidate {
                    domain = Some(candidate);
                    break;
                }
                cx.background_executor()
                    .timer(MUX_BRIDGE_WAIT_INTERVAL)
                    .await;
            }

            let Some(domain) = domain else {
                tracing::warn!(
                    "mux connection never became available; extensions run without mux access"
                );
                return;
            };

            // §16.9 Keep the domain for server-chrome action routing: clicks
            // on daemon-rendered chrome go back over this connection.
            if this
                .update(cx, |this, _| this.mux_domain = Some(domain.clone()))
                .is_err()
            {
                tracing::debug!("extension controller dropped before the mux domain was installed");
                return;
            }

            let state = Arc::new(Mutex::new(MuxBridgeState::default()));
            if let Some(session_id) = domain.last_attached_session_id()
                && let Ok(sessions) = domain.list_sessions().await
                && let Some(session) = sessions.iter().find(|session| session.id == session_id)
            {
                state.lock().session_name = Some(session.name.clone());
            }

            // §5.6 The bridge is built per extension with its declared
            // filesystem scope, so the host thread receives the connection
            // parts and constructs one scoped bridge per extension.
            if let Err(error) = this.read_with(cx, |this, _| {
                this.send(HostCommand::InstallBridge {
                    domain: domain.clone(),
                    state: state.clone(),
                });
            }) {
                tracing::debug!(%error, "extension controller dropped before the mux bridge was installed");
                return;
            }
            if let Some(snapshot) = domain.last_attached_snapshot() {
                for (event, payload) in hydration_events(&snapshot, &state) {
                    let payload = match serde_json::to_string(&payload) {
                        Ok(payload) => payload,
                        Err(error) => {
                            tracing::warn!(
                                %event,
                                %error,
                                "serializing initial extension hydration failed"
                            );
                            continue;
                        }
                    };
                    if let Err(error) =
                        this.read_with(cx, |this, _| this.emit_event(&event, &payload))
                    {
                        tracing::debug!(
                            %error,
                            "extension host dropped before initial mux hydration"
                        );
                        return;
                    }
                }
            }

            let notifications = domain.subscribe();

            while let Ok(notification) = notifications.recv().await {
                let server_update = match notification.event.as_ref() {
                    Some(mux_protocol::notification::Event::ExtensionChrome(update)) => {
                        Some(update.clone())
                    }
                    _ => None,
                };
                if let Some(update) = server_update {
                    let applied = this.update(cx, |this, cx| {
                        if let Err(error) = this.apply_server_chrome_update(update, cx) {
                            tracing::warn!(%error, "server extension chrome update rejected");
                        }
                    });
                    if let Err(error) = applied {
                        tracing::debug!(
                            %error,
                            "extension controller dropped; ending server chrome forwarding"
                        );
                        return;
                    }
                }
                for (event, payload) in notification_events(&notification, &state) {
                    let payload = match serde_json::to_string(&payload) {
                        Ok(payload) => payload,
                        Err(error) => {
                            tracing::warn!(%event, %error, "serializing extension event failed");
                            continue;
                        }
                    };
                    if let Err(error) =
                        this.read_with(cx, |this, _| this.emit_event(&event, &payload))
                    {
                        tracing::debug!(%error, "extension controller dropped; ending mux event forwarding");
                        return;
                    }
                }
            }
        }));
    }

    /// §3.4 Deliver a host event to every loaded extension.
    pub fn emit_event(&self, event: &str, payload: &str) {
        self.send(HostCommand::Emit {
            event: event.to_string(),
            payload: payload.to_string(),
        });
    }

    /// §5.7 Dispatch a command registered by an extension (VDOM `onClick`
    /// descriptors and extension keybindings both route through here).
    ///
    /// §16.7 Ownership: the id resolves through the registry to exactly one
    /// extension; a command no extension owns is rejected here (logged, not
    /// executed) instead of fanning out to every runtime. Native core
    /// commands never flow through this path, so they are unaffected when
    /// the host is absent.
    pub fn execute_command(&self, command: &str, arguments_json: &str) {
        let Some(owner) = self.commands.resolve_owner(command).map(str::to_string) else {
            tracing::warn!(
                %command,
                "no extension owns this command; dispatch rejected"
            );
            return;
        };
        // A namespaced id disambiguates the owner but is not what the owning
        // runtime registered, so hand it back the bare id it knows.
        let command = command
            .strip_prefix(&format!("{owner}."))
            .unwrap_or(command)
            .to_string();
        self.send(HostCommand::ExecuteCommand {
            owner: Some(owner),
            command,
            arguments: arguments_json.to_string(),
        });
    }

    /// §16.7 The extension that owns `command`, if any (namespaced or bare
    /// id). Exposed for the server-chrome routing path, which must never
    /// dispatch a server-origin command to a client extension.
    pub fn resolve_command_owner(&self, command: &str) -> Option<String> {
        self.commands.resolve_owner(command).map(str::to_string)
    }

    /// §16.7 Build the shortcut resolver handed to every `MuxPaneView`: a
    /// snapshot-backed chord -> action lookup that stays live as extensions
    /// register or are approved. Before any keymap report lands (or without
    /// a host) the snapshot is empty and nothing matches — the pane
    /// passthroughs exactly as it did before the resolver existed.
    pub fn extension_shortcut_resolver(
        &self,
    ) -> terminal_view::mux_pane::ExtensionShortcutResolver {
        let snapshot = self.keymap_snapshot.clone();
        std::sync::Arc::new(move |keystroke: &Keystroke| {
            let bindings = snapshot.lock();
            let pressed = chord_lookup_key(keystroke);
            let matched = bindings.iter().find(|(chord, _)| {
                Keystroke::parse(chord.as_str())
                    .map(|parsed| chord_lookup_key(&parsed) == pressed)
                    .unwrap_or_else(|_| chord.eq_ignore_ascii_case(&keystroke.to_string()))
            });
            matched.map(|(_, action)| SharedString::from(action.clone()))
        })
    }

    /// Force a chrome re-render (used after a workspace attaches a new status
    /// bar so it inherits the current chrome).
    pub fn request_render(&self) {
        self.send(HostCommand::Render);
    }

    /// §16.9 Route a chrome interaction from daemon-rendered chrome back to
    /// the authoritative server-side extension host. Fail closed: without a
    /// mux connection the action is logged and dropped — it is never
    /// executed against client-side extensions, even if one registers the
    /// same command id. Responses that come back rejected are logged with
    /// the daemon's contextual error.
    pub fn execute_server_command(
        &self,
        extension_id: &str,
        view_id: &str,
        command: &str,
        arguments: &str,
        cx: &mut gpui::App,
    ) {
        let Some(domain) = self.mux_domain.clone() else {
            tracing::warn!(
                extension_id,
                view_id,
                %command,
                "server chrome action dropped: no mux connection"
            );
            return;
        };
        let request = mux_protocol::ExtensionChromeActionRequest {
            extension_id: extension_id.to_string(),
            view_id: view_id.to_string(),
            command: command.to_string(),
            arguments: arguments.to_string(),
        };
        let extension_id = extension_id.to_string();
        let command = command.to_string();
        cx.background_executor()
            .spawn(async move {
                match domain
                    .send_request(mux_protocol::request::Body::ExtensionChromeAction(request))
                    .await
                {
                    Ok(response) => {
                        if let Some(
                            mux_protocol::response::Body::ExtensionChromeActionResult(result),
                        ) = response.body
                            && !result.accepted
                        {
                            tracing::warn!(
                                extension_id,
                                %command,
                                error = %result.error,
                                "server chrome action rejected by the daemon"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            extension_id,
                            %command,
                            "server chrome action transport failed"
                        );
                    }
                }
            })
            .detach();
    }

    fn send(&self, command: HostCommand) {
        // A missing sender means the host thread never started. Dropping the
        // command silently makes an extension look merely unresponsive.
        let Some(sender) = &self.command_sender else {
            tracing::warn!("QuickJS host is not running; command dropped");
            return;
        };
        if let Err(error) = sender.send(command) {
            tracing::warn!(%error, "failed to send command to QuickJS host");
        }
    }

    /// Register a chrome surface and hand it the callback that turns an
    /// `onClick` / `onChange` descriptor back into a command on the extension
    /// thread. Without it the bridge parses the descriptors and then drops
    /// them, so every control an extension renders is inert.
    pub fn add_status_bar(
        &self,
        status_bar: gpui::WeakEntity<crate::extension_status_bar::ExtensionStatusBar>,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(bar) = status_bar.upgrade() {
            let dispatch = Self::command_dispatch(cx.weak_entity());
            bar.update(cx, |bar, _| bar.set_dispatch(dispatch));
        }
        self.status_bars.lock().push(status_bar);
        self.request_render();
    }

    fn command_dispatch(this: gpui::WeakEntity<Self>) -> vdom_bridge::CommandDispatch {
        std::rc::Rc::new(
            move |invocation: CommandInvocation, _window: &mut gpui::Window, cx: &mut gpui::App| {
                let arguments = match serde_json::to_string(&invocation.args) {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            command = %invocation.command,
                            "chrome command arguments are not serializable"
                        );
                        return;
                    }
                };
                // §5.7 / §16.9 Server chrome first: an invocation stamped with
                // a server origin belongs to the daemon's extension host and
                // routes back there — never to client-side extensions, even
                // if a client extension registers the same command id.
                // `CommandInvocation::parse` guarantees a stamped origin has
                // side "server", so any `Some` here is a server interaction.
                if let Some(origin) = &invocation.origin {
                    if let Err(error) = this.update(cx, |this, cx| {
                        this.execute_server_command(
                            &origin.extension_id,
                            &origin.view_id,
                            &invocation.command,
                            &arguments,
                            cx,
                        );
                    }) {
                        tracing::debug!(
                            %error,
                            command = %invocation.command,
                            "extension host is gone; server chrome action dropped"
                        );
                    }
                    return;
                }
                if let Err(error) = this.update(cx, |this, _| {
                    this.execute_command(&invocation.command, &arguments);
                }) {
                    tracing::debug!(
                        %error,
                        command = %invocation.command,
                        "extension host is gone; chrome command dropped"
                    );
                }
            },
        )
    }
}

/// Activate every discovered extension, skipping (and logging) the ones that
/// throw — §15.7: a broken extension must not take the app down with it.
fn activate_extensions(discovered: Vec<DiscoveredExtension>) -> Vec<HostedExtension> {
    let mut live_extensions = Vec::new();
    for extension in discovered {
        let runner = ExtensionRunner::for_manifest(&extension.manifest);
        match runner.load_live(&extension.manifest.id, &extension.source, "activate") {
            Ok(live) => {
                tracing::info!(
                    id = %extension.manifest.id,
                    path = %extension.directory.display(),
                    "live extension loaded"
                );
                live_extensions.push(HostedExtension {
                    live,
                    suspended: false,
                    suspension_reason: None,
                });
            }
            Err(error) => {
                tracing::warn!(id = %extension.manifest.id, error = %format!("{error:#}"), "live extension load failed");
            }
        }
    }
    live_extensions
}

impl Drop for ExtensionHostController {
    fn drop(&mut self) {
        if let Some(sender) = &self.command_sender
            && let Err(error) = sender.send(HostCommand::Shutdown)
        {
            tracing::warn!(%error, "failed to stop QuickJS host");
        }
        if let Some(handle) = self.host_thread.take()
            && handle.join().is_err()
        {
            tracing::warn!("QuickJS host thread panicked during shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_status_bar::ExtensionStatusBar;
    use extension_host::vdom_bridge::{self, VDomChild};
    use futures::FutureExt as _;

    fn loaded_with_vdom(id: &str, json: Option<&str>) -> LoadedExtension {
        let result = ExtensionRunResult {
            extension_id: id.to_string(),
            result: Ok(()),
            duration: std::time::Duration::ZERO,
            cpu_exhausted: false,
            memory_exceeded: false,
            vdom_json: json.map(|s| s.to_string()),
        };
        LoadedExtension {
            id: id.to_string(),
            name: id.to_string(),
            side: ExtensionSide::Client,
            result,
        }
    }

    fn temporary_extension_dir(name: &str) -> Result<std::path::PathBuf> {
        let directory =
            std::env::temp_dir().join(format!("z3rm-quickjs-{name}-{}", nanoid::nanoid!()));
        std::fs::create_dir_all(&directory)?;
        Ok(directory)
    }

    fn notification(event: mux_protocol::notification::Event) -> mux_protocol::Notification {
        mux_protocol::Notification { event: Some(event) }
    }

    #[test]
    fn server_extension_is_filtered_before_script_read() -> Result<()> {
        let directory = temporary_extension_dir("server-side")?;
        std::fs::write(
            directory.join("extension.toml"),
            "name = \"server extension\"\n[runtime]\nside = \"server\"\n",
        )?;
        std::fs::write(directory.join("main.js"), "function activate() {}")?;

        let discovered =
            quickjs_runtime::discover_client_extensions(std::slice::from_ref(&directory));

        assert!(
            discovered.is_empty(),
            "server-side extensions must be skipped"
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn invalid_runtime_side_fails_closed() -> Result<()> {
        let directory = temporary_extension_dir("invalid-side")?;
        let manifest = directory.join("extension.toml");
        std::fs::write(
            &manifest,
            "name = \"invalid extension\"\n[runtime]\nside = \"browser\"\n",
        )?;

        let result = quickjs_runtime::parse_manifest(&manifest);

        assert!(result.is_err());
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    /// P0-1: a user-installed fork must shadow the built-in of the same id, and
    /// the built-in roots must still contribute the remaining extensions.
    #[test]
    fn user_extension_overrides_builtin_of_the_same_id() -> Result<()> {
        let user_directory = temporary_extension_dir("user-root")?;
        let fork = user_directory.join("z3rm-status-bar");
        std::fs::create_dir_all(&fork)?;
        std::fs::write(
            fork.join("extension.toml"),
            "[extension]\nname = \"forked status bar\"\n[runtime]\nside = \"client\"\n[capabilities]\nmux = true\n",
        )?;
        std::fs::write(fork.join("main.js"), "function activate(context) {}")?;

        let roots = quickjs_runtime::extension_roots(&user_directory);
        let discovered = quickjs_runtime::discover_client_extensions(&roots);

        let status_bar = discovered
            .iter()
            .find(|extension| extension.manifest.id == "z3rm-status-bar")
            .context("forked status bar must be discovered")?;
        assert_eq!(status_bar.manifest.name, "forked status bar");
        assert!(
            discovered
                .iter()
                .any(|extension| extension.manifest.id == "z3rm-tab-bar"),
            "built-in roots must still be scanned"
        );

        std::fs::remove_dir_all(user_directory)?;
        Ok(())
    }

    /// P0-1: the shipped built-ins must actually load through the host path
    /// that startup uses, not just through the runtime unit tests.
    #[test]
    fn builtin_extensions_load_through_the_host_path() -> Result<()> {
        let empty_user_directory = temporary_extension_dir("empty-user-root")?;
        let discovered = quickjs_runtime::discover_client_extensions(
            &quickjs_runtime::extension_roots(&empty_user_directory),
        );
        let mut live_extensions = activate_extensions(discovered);

        let ids: Vec<&str> = live_extensions
            .iter()
            .map(|hosted| hosted.live.id())
            .collect();
        for expected in [
            "z3rm-command-palette",
            "z3rm-layout-manager",
            "z3rm-session-manager",
            "z3rm-status-bar",
            "z3rm-tab-bar",
            "z3rm-which-key",
        ] {
            assert!(
                ids.contains(&expected),
                "{expected} did not activate: {ids:?}"
            );
        }

        // Chrome must be non-empty on first paint: status-bar and tab-bar
        // always render, on-demand overlays render null until opened.
        let nodes = render_live_extensions(&mut live_extensions);
        assert!(!nodes.is_empty(), "built-in chrome produced no VDOM");

        std::fs::remove_dir_all(empty_user_directory)?;
        Ok(())
    }

    /// §5.6: an extension that blows its CPU budget must be suspended, and the
    /// rest of the chrome must keep rendering.
    #[test]
    fn runaway_extension_is_suspended_and_stops_rendering() -> Result<()> {
        let root = temporary_extension_dir("runaway-root")?;
        let extension = root.join("runaway");
        std::fs::create_dir_all(&extension)?;
        std::fs::write(
            extension.join("extension.toml"),
            "[extension]\nname = \"runaway\"\n[runtime]\nside = \"client\"\n[resources]\ncpu_budget_ms = 1\n",
        )?;
        std::fs::write(
            extension.join("main.js"),
            r#"
            function activate(context) {
                context.registerChromeView('runaway', {
                    render: function() { return { type: 'span', children: ['alive'] }; }
                });
                context.on('spin', function() { while (true) {} });
            }
            "#,
        )?;

        let mut live_extensions = activate_extensions(quickjs_runtime::discover_client_extensions(
            std::slice::from_ref(&root),
        ));
        assert_eq!(live_extensions.len(), 1);
        assert!(
            !render_live_extensions(&mut live_extensions).is_empty(),
            "a healthy extension must render"
        );

        // QuickJS raises an uncatchable exception when the fuel interrupt
        // fires, so the runaway handler cannot swallow its own kill signal.
        assert!(
            live_extensions[0].live.emit_event("spin", "null").is_err(),
            "the runaway handler must be interrupted"
        );
        live_extensions[0].note_resource_violations();
        assert!(live_extensions[0].suspended, "CPU violation must suspend");
        assert!(
            render_live_extensions(&mut live_extensions).is_empty(),
            "a suspended extension must stop contributing chrome"
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// §5.6: an extension that blows its memory budget must be suspended, and
    /// the rest of the chrome must keep rendering. The OOM is caught by the
    /// bootstrap's render try/catch, so the violation must be detected through
    /// the drained error list rather than a Rust-level render failure.
    #[test]
    fn runaway_memory_extension_is_suspended_and_stops_rendering() -> Result<()> {
        let root = temporary_extension_dir("memory-runaway-root")?;
        let extension = root.join("memory-runaway");
        std::fs::create_dir_all(&extension)?;
        std::fs::write(
            extension.join("extension.toml"),
            "[extension]\nname = \"memory-runaway\"\n[runtime]\nside = \"client\"\n[resources]\nmemory_limit_mb = 1\n",
        )?;
        std::fs::write(
            extension.join("main.js"),
            r#"
            function activate(context) {
                context.registerChromeView('memory-runaway', {
                    render: function() {
                        var blocks = [];
                        for (var i = 0; i < 10000000; i++) { blocks.push(new Array(1000)); }
                        return { type: 'span', children: ['alive'] };
                    }
                });
            }
            "#,
        )?;

        let mut live_extensions = activate_extensions(quickjs_runtime::discover_client_extensions(
            std::slice::from_ref(&root),
        ));
        assert_eq!(live_extensions.len(), 1);
        assert!(
            !live_extensions[0].suspended,
            "a healthy extension must not be suspended before the violation"
        );

        // First paint: the runaway render hits the 1MB ceiling; the OOM is
        // swallowed by the JS try/catch but must still suspend the extension.
        let nodes = render_live_extensions(&mut live_extensions);
        assert!(nodes.is_empty(), "OOM 的渲染不得产生 chrome");
        assert!(
            live_extensions[0].suspended,
            "memory violation must suspend the extension"
        );
        assert!(
            render_live_extensions(&mut live_extensions).is_empty(),
            "a suspended extension must stop contributing chrome"
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// P0-3: mux notifications must translate into the events the built-ins
    /// subscribe to; an unmapped notification leaves the chrome permanently
    /// empty.
    #[test]
    fn mux_notifications_map_to_extension_events() {
        let state = Mutex::new(MuxBridgeState::default());
        state.lock().session_name = Some("work".to_string());

        let added = notification_events(
            &notification(mux_protocol::notification::Event::PaneAdded(
                mux_protocol::PaneAdded {
                    pane_id: "p1".into(),
                    tab_id: "t1".into(),
                },
            )),
            &state,
        );
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0, "pane:added");

        let titled = notification_events(
            &notification(mux_protocol::notification::Event::PaneTitleChanged(
                mux_protocol::PaneTitleChanged {
                    pane_id: "p1".into(),
                    title: "vim".into(),
                },
            )),
            &state,
        );
        assert_eq!(titled.len(), 1, "unfocused pane only emits pane:title");

        let focused = notification_events(
            &notification(mux_protocol::notification::Event::PaneFocused(
                mux_protocol::PaneFocused {
                    pane_id: "p1".into(),
                },
            )),
            &state,
        );
        assert_eq!(focused.len(), 1);
        assert_eq!(focused[0].0, "pane:focus");
        assert_eq!(focused[0].1["title"], "vim");
        assert_eq!(focused[0].1["sessionName"], "work");
        assert_eq!(focused[0].1["tabId"], "t1");

        let tab = notification_events(
            &notification(mux_protocol::notification::Event::TabTitleChanged(
                mux_protocol::TabTitleChanged {
                    tab_id: "t1".into(),
                    title: "build".into(),
                },
            )),
            &state,
        );
        assert_eq!(tab.len(), 1);
        assert_eq!(tab[0].0, "tab:title");
        assert_eq!(tab[0].1["paneId"], "p1");
        assert_eq!(tab[0].1["active"], true);

        let removed = notification_events(
            &notification(mux_protocol::notification::Event::PaneRemoved(
                mux_protocol::PaneRemoved {
                    pane_id: "p1".into(),
                    exit_code: 3,
                },
            )),
            &state,
        );
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, "pane:removed");
        assert_eq!(removed[0].1["exitCode"], 3);
        assert!(state.lock().focused_pane.is_none());
    }

    /// P0-3: the layout tree must reach the layout manager as JSON, not null.
    #[test]
    fn session_layout_notification_carries_the_layout_tree() {
        let state = Mutex::new(MuxBridgeState::default());
        let layout = mux_protocol::LayoutTree {
            root: Some(mux_protocol::LayoutNode {
                id: "root".into(),
                node: Some(mux_protocol::layout_node::Node::Split(
                    mux_protocol::SplitNode {
                        direction: mux_protocol::split_node::SplitDirection::LeftRight as i32,
                        ratios: vec![0.5, 0.5],
                        children: vec![
                            mux_protocol::LayoutNode {
                                id: "a".into(),
                                node: Some(mux_protocol::layout_node::Node::Pane(
                                    mux_protocol::PaneLeaf {
                                        pane_id: "p1".into(),
                                    },
                                )),
                            },
                            mux_protocol::LayoutNode {
                                id: "b".into(),
                                node: Some(mux_protocol::layout_node::Node::Pane(
                                    mux_protocol::PaneLeaf {
                                        pane_id: "p2".into(),
                                    },
                                )),
                            },
                        ],
                    },
                )),
            }),
        };

        let events = notification_events(
            &notification(mux_protocol::notification::Event::SessionLayoutChanged(
                mux_protocol::SessionLayoutChanged {
                    layout: Some(layout),
                    // §15.4 ordinary server layout notifications stay pure
                    // deltas; only the reconnect resync carries a snapshot.
                    snapshot: None,
                },
            )),
            &state,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "session:layout");
        assert_eq!(events[0].1["type"], "split");
        assert_eq!(events[0].1["direction"], "left-right");
        assert_eq!(events[0].1["children"][1]["paneId"], "p2");
    }

    fn snapshot_two_tabs() -> mux_protocol::SessionSnapshot {
        mux_protocol::SessionSnapshot {
            tabs: vec![
                mux_protocol::TabInfo {
                    id: "t1".into(),
                    title: "build".into(),
                    panes: vec![
                        mux_protocol::PaneInfo {
                            id: "p1".into(),
                            title: "cargo".into(),
                            ..Default::default()
                        },
                        mux_protocol::PaneInfo {
                            id: "p2".into(),
                            title: "vim".into(),
                            ..Default::default()
                        },
                    ],
                },
                mux_protocol::TabInfo {
                    id: "t2".into(),
                    title: "logs".into(),
                    panes: vec![mux_protocol::PaneInfo {
                        id: "p3".into(),
                        title: "journalctl".into(),
                        ..Default::default()
                    }],
                },
            ],
            focused_pane_id: "p2".into(),
            focused_tab_id: "t1".into(),
            ..Default::default()
        }
    }

    /// §15.4 A reconnect resync must reconcile the extension-visible mux
    /// state from the authoritative snapshot: every pane emitted once,
    /// every tab once, and exactly one focus event — with the focused
    /// pane's title resolved.
    #[test]
    fn snapshot_resync_emits_every_pane_tab_and_focus_once() {
        let state = Mutex::new(MuxBridgeState::default());
        state.lock().session_name = Some("work".to_string());
        let snapshot = snapshot_two_tabs();

        let events = notification_events(
            &notification(mux_protocol::notification::Event::SessionLayoutChanged(
                mux_protocol::SessionLayoutChanged {
                    layout: None,
                    snapshot: Some(snapshot),
                },
            )),
            &state,
        );

        let count = |name: &str| events.iter().filter(|(event, _)| event == name).count();
        assert_eq!(count("session:layout"), 1);
        assert_eq!(count("pane:added"), 3, "every pane must be emitted");
        assert_eq!(count("pane:title"), 3);
        assert_eq!(count("tab:title"), 2, "every tab must be emitted");
        assert_eq!(count("pane:focus"), 1, "exactly one focus event");

        // pane membership maps each pane to its owning tab.
        let p3_added = events
            .iter()
            .find(|(event, payload)| event == "pane:added" && payload["paneId"] == "p3")
            .expect("p3 pane:added event");
        assert_eq!(p3_added.1["tabId"], "t2");

        // The tab-bar upsert payload carries the first pane id and focus.
        let t1_title = events
            .iter()
            .find(|(event, payload)| event == "tab:title" && payload["tabId"] == "t1")
            .expect("t1 tab:title event");
        assert_eq!(t1_title.1["paneId"], "p1");
        assert_eq!(t1_title.1["active"], true);
        let t2_title = events
            .iter()
            .find(|(event, payload)| event == "tab:title" && payload["tabId"] == "t2")
            .expect("t2 tab:title event");
        assert_eq!(t2_title.1["active"], false);

        let focus = events
            .iter()
            .find(|(event, _)| event == "pane:focus")
            .expect("pane:focus event")
            .1
            .clone();
        assert_eq!(focus["paneId"], "p2");
        assert_eq!(focus["title"], "vim", "focused title from the snapshot");
        assert_eq!(focus["tabId"], "t1");

        // §15.4 at-least-once: re-delivering the same resync must not emit a
        // duplicate focus event (the state guard suppresses it).
        let repeated = notification_events(
            &notification(mux_protocol::notification::Event::SessionLayoutChanged(
                mux_protocol::SessionLayoutChanged {
                    layout: None,
                    snapshot: Some(snapshot_two_tabs()),
                },
            )),
            &state,
        );
        assert_eq!(
            repeated.iter().filter(|(event, _)| event == "pane:focus").count(),
            0,
            "an unchanged resync must not duplicate pane:focus"
        );
    }

    /// §3.4 / §15.4 Initial attach hydration: two pre-existing tabs must be
    /// emitted once each — not only the focused pane — with no duplicates.
    #[test]
    fn initial_hydration_emits_all_tabs_and_panes_once() {
        let state = Mutex::new(MuxBridgeState::default());
        state.lock().session_name = Some("work".to_string());
        let snapshot = snapshot_two_tabs();

        let events = hydration_events(&snapshot, &state);

        let count = |name: &str| events.iter().filter(|(event, _)| event == name).count();
        assert_eq!(count("tab:title"), 2, "both pre-existing tabs hydrated");
        assert_eq!(count("pane:added"), 3);
        assert_eq!(count("pane:title"), 3);
        assert_eq!(count("pane:focus"), 1);
        assert_eq!(count("session:layout"), 1);

        // No duplicated (event, payload) pair in the hydration batch.
        let mut seen = std::collections::HashSet::new();
        for (event, payload) in &events {
            assert!(
                seen.insert((event.clone(), payload.clone())),
                "duplicate hydration event {event}"
            );
        }

        // The bridge state must be populated by the same pass: extension RPCs
        // that read it (getFocusedPane, splitPane) see the hydrated session.
        let state = state.lock();
        assert_eq!(state.focused_pane.as_deref(), Some("p2"));
        assert_eq!(state.pane_tabs.get("p3").map(String::as_str), Some("t2"));
        assert_eq!(state.pane_titles.get("p1").map(String::as_str), Some("cargo"));
    }

    /// P0-3 end-to-end at the JS boundary: a mapped notification must actually
    /// change what the built-in status bar renders.
    #[test]
    fn pane_focus_notification_updates_builtin_status_bar_chrome() -> Result<()> {
        let empty_user_directory = temporary_extension_dir("focus-e2e")?;
        let discovered = quickjs_runtime::discover_client_extensions(
            &quickjs_runtime::extension_roots(&empty_user_directory),
        );
        let mut live_extensions = activate_extensions(discovered);

        let state = Mutex::new(MuxBridgeState::default());
        state.lock().session_name = Some("work".to_string());
        state.lock().pane_titles.insert("p1".into(), "vim".into());

        for (event, payload) in notification_events(
            &notification(mux_protocol::notification::Event::PaneFocused(
                mux_protocol::PaneFocused {
                    pane_id: "p1".into(),
                },
            )),
            &state,
        ) {
            for hosted in &live_extensions {
                hosted
                    .live
                    .emit_event(&event, &serde_json::to_string(&payload)?)?;
            }
        }

        let rendered = render_live_extensions(&mut live_extensions)
            .iter()
            .map(|node| vdom_bridge::vdom_to_text(node, 0))
            .collect::<String>();
        assert!(rendered.contains("vim"), "status bar text: {rendered}");
        assert!(rendered.contains("work"), "status bar text: {rendered}");

        std::fs::remove_dir_all(empty_user_directory)?;
        Ok(())
    }

    /// P0-5: the session payload field the JS reads is `clients`.
    #[test]
    fn session_json_uses_the_field_name_extensions_read() {
        let json = session_json(&mux_protocol::SessionInfo {
            id: "s1".into(),
            name: "work".into(),
            cwd: "/tmp".into(),
            created_timestamp: 7,
            attached_clients: 2,
        });
        assert_eq!(json["clients"], 2);
        assert!(
            json.get("attachedClients").is_none(),
            "the stale field name must be gone: {json}"
        );
    }

    #[test]
    fn split_direction_maps_extension_words_to_protocol_values() -> Result<()> {
        assert_eq!(
            split_direction("right")?,
            mux_protocol::split_node::SplitDirection::LeftRight
        );
        assert_eq!(
            split_direction("down")?,
            mux_protocol::split_node::SplitDirection::TopBottom
        );
        assert!(split_direction("sideways").is_err());
        Ok(())
    }

    #[test]
    fn scrollback_is_flattened_into_capture_text() {
        let response = mux_protocol::FetchScrollbackResponse {
            lines: vec![
                mux_protocol::RowChange {
                    row: 0,
                    cells: ["h", "i"]
                        .into_iter()
                        .map(|char| mux_protocol::Cell {
                            char: char.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                },
                mux_protocol::RowChange {
                    row: 1,
                    cells: vec![mux_protocol::Cell {
                        char: "!".to_string(),
                        ..Default::default()
                    }],
                },
            ],
            total_lines: 2,
            scrollback_version: 1,
        };
        assert_eq!(scrollback_text(&response), "hi\n!");
    }

    #[test]
    fn collect_parses_status_bar_vdom_from_activate_result() {
        // Deterministic extension result path: activate() returned a VDOM via
        // context.render(...); the runtime surfaced it as JSON. The collector
        // turns it into a typed VDomNode the status bar can render.
        let vdom = r#"{"type":"div","props":{"id":"status-bar"},"style":{"flexDirection":"row","gap":"8px"},"children":[{"type":"span","children":["zsh"]}]}"#;
        let loaded = vec![loaded_with_vdom("demo-statusbar", Some(vdom))];
        let nodes = collect_status_bar_vdom(&loaded);
        assert_eq!(nodes.len(), 1);
        let node = &nodes[0];
        assert_eq!(node.element_type, "div");
        assert_eq!(
            node.props.get("id").and_then(|v| v.as_str()),
            Some("status-bar")
        );
        assert_eq!(
            node.style.get("flexDirection").map(String::as_str),
            Some("row")
        );
        assert_eq!(node.children.len(), 1);
        // The outer div's only child is the span element, not text.
        let span = match &node.children[0] {
            VDomChild::Node(n) => n,
            VDomChild::Text(_) => panic!("expected the outer child to be a span node"),
        };
        assert_eq!(span.element_type, "span");
        // The span surfaces the deterministic "zsh" text child — the bytes the
        // status bar will ultimately render.
        assert_eq!(span.children.len(), 1);
        match &span.children[0] {
            VDomChild::Text(t) => assert_eq!(t, "zsh"),
            VDomChild::Node(_) => panic!("expected text child for the span"),
        }
    }

    #[test]
    fn collect_skips_extensions_without_vdom() {
        let loaded = vec![
            loaded_with_vdom("no-render", None),
            loaded_with_vdom("has-render", Some(r#"{"type":"div","children":["ok"]}"#)),
        ];
        let nodes = collect_status_bar_vdom(&loaded);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].element_type, "div");
    }

    #[test]
    fn collect_logs_and_skips_malformed_vdom_without_panicking() {
        let loaded = vec![
            loaded_with_vdom("broken", Some(r#"{"type":"div","children":}"#)),
            loaded_with_vdom("not-an-object", Some(r#""oops""#)),
            loaded_with_vdom("good", Some(r#"{"type":"span","children":["x"]}"#)),
        ];
        let nodes = collect_status_bar_vdom(&loaded);
        // Only the well-formed extension yields a node; the rest are logged + skipped.
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].element_type, "span");
    }

    #[test]
    fn status_bar_renders_nonempty_vdom_from_collector_output() {
        // End-to-end at the data layer: collector output → renderable bridge element.
        // Confirms the status bar's render path has something to show for a
        // deterministic extension result, without spinning up a GPUI window.
        let vdom = r#"{"type":"div","children":[{"type":"span","children":["hello"]}]}"#;
        let nodes = collect_status_bar_vdom(&vec![loaded_with_vdom("e", Some(vdom))]);
        assert_eq!(nodes.len(), 1);
        let text = vdom_bridge::vdom_to_text(&nodes[0], 0);
        assert!(
            text.contains("hello"),
            "bridge vdom_to_text must surface the span text: {text}"
        );
    }

    #[gpui::test]
    fn status_bar_setter_accepts_vdom_and_notifies(cx: &mut gpui::TestAppContext) {
        // Exercises the real setter + notify path: the host pushes the
        // deterministic collector output into a live ExtensionStatusBar entity,
        // and the view's state reflects it (which its next render will display).
        let vdom = r#"{"type":"div","children":["branch"]}"#;
        let nodes = collect_status_bar_vdom(&vec![loaded_with_vdom("git", Some(vdom))]);
        assert_eq!(nodes.len(), 1);

        let bar = cx.update(|cx| cx.new(|_| ExtensionStatusBar::new()));
        let before = cx.read(|cx| bar.read(cx).vdom_node_count());
        assert_eq!(before, 0);

        cx.update(|cx| bar.update(cx, |bar, cx| bar.set_vdom_nodes(nodes, cx)));

        let (count, element_type) = cx.read(|cx| {
            let bar = bar.read(cx);
            (
                bar.vdom_node_count(),
                bar.vdom_nodes().first().map(|n| n.element_type.clone()),
            )
        });
        assert_eq!(count, 1);
        assert_eq!(element_type.as_deref(), Some("div"));
    }

    /// A chrome extension whose only button carries an `onClick` descriptor and
    /// whose label reports the command's side effect, so the rendered text is
    /// proof that the click reached QuickJS.
    const CLICK_PROBE_JS: &str = r#"
        export function activate(context) {
            const state = { total: 0 };
            const View = {
                render() {
                    return {
                        type: 'button',
                        props: {
                            id: 'probe',
                            onClick: { command: 'probe.add', args: [7] },
                        },
                        style: { width: '400px', height: '200px' },
                        children: ['total=' + state.total],
                    };
                },
            };
            context.commands.register('probe.add', function(args) {
                state.total += (args && args.length) ? args[0] : 1;
                View.invalidate();
            });
            context.registerChromeView('status-bar', View);
        }
    "#;

    /// Pump the foreground executor until `condition` holds. The extension host
    /// runs on a real OS thread, so simulated time cannot advance it; the loop
    /// yields wall clock between drains.
    fn wait_for(
        cx: &mut gpui::TestAppContext,
        what: &str,
        mut condition: impl FnMut(&mut gpui::TestAppContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if condition(cx) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn status_bar_text(
        cx: &mut gpui::TestAppContext,
        bar: &gpui::Entity<ExtensionStatusBar>,
    ) -> String {
        cx.read(|cx| {
            bar.read(cx)
                .vdom_nodes()
                .iter()
                .map(|node| vdom_bridge::vdom_to_text(node, 0))
                .collect::<String>()
        })
    }

    /// The full chrome interaction loop, end to end: a QuickJS extension renders
    /// a button carrying an `onClick` descriptor, the bridge turns it into a
    /// GPUI click handler, a real mouse click dispatches the command back to the
    /// host thread, and the extension's own re-render reports the new state.
    ///
    /// Every link is the shipping one — no hand-built VDOM and no directly
    /// invoked dispatch closure — because each of them has silently regressed
    /// before while the individual pieces still had passing unit tests.
    // `#[gpui::test]` discards the function's return value, so a `Result` body
    // would swallow every `?`; failures have to panic to be seen.
    #[gpui::test]
    fn chrome_button_click_executes_the_extension_command(cx: &mut gpui::TestAppContext) {
        // §5.2 puts the extension host on its own OS thread, so the scheduler's
        // determinism check has to be relaxed for it to touch the app at all.
        cx.background_executor.allow_parking();

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        // Extensions are discovered as subdirectories of a root, so the probe
        // needs its own directory inside the temporary root.
        let root = temporary_extension_dir("click-probe").expect("create extension root");
        let directory = root.join("click-probe");
        std::fs::create_dir_all(&directory).expect("create extension directory");
        std::fs::write(
            directory.join("extension.toml"),
            "[extension]\nname = \"click-probe\"\n[runtime]\nside = \"client\"\n",
        )
        .expect("write manifest");
        std::fs::write(directory.join("main.js"), CLICK_PROBE_JS).expect("write extension source");

        // Only the probe is loaded: the built-in roots would render their own
        // chrome ahead of it and move the button out from under the click.
        let discovered = quickjs_runtime::discover_client_extensions(std::slice::from_ref(&root));
        assert_eq!(
            discovered.len(),
            1,
            "probe extension was not discovered under {}",
            root.display()
        );
        // §5.6 consent gate: pre-approve the probe so this test exercises the
        // chrome click loop rather than the first-install prompt. The consent
        // file lives next to the temp root so the real user's store is never
        // touched and parallel tests cannot race on it.
        let consent_file = root.join("extension-consent.json");
        let mut consent_records = HashMap::new();
        consent_records.insert(
            "click-probe".to_string(),
            ConsentRecord::approved("click-probe", consent_fingerprint(&discovered[0])),
        );
        save_consent_records(&consent_file, &consent_records).expect("write consent records");

        let host = cx.update(|cx| {
            cx.new(|cx| {
                let mut host = ExtensionHostController::new();
                host.consent_file = consent_file.clone();
                host.start_with_roots(vec![root.clone()], cx);
                host
            })
        });

        let window = cx.add_window(|_, _| ExtensionStatusBar::new());
        let bar = window.root(cx).expect("status bar window root");
        cx.update(|cx| host.update(cx, |host, cx| host.add_status_bar(bar.downgrade(), cx)));

        wait_for(cx, "the probe's first chrome push", |cx| {
            status_bar_text(cx, &bar).contains("total=0")
        });

        let mut window_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        window_cx.run_until_parked();
        window_cx.simulate_click(
            gpui::point(gpui::px(10.0), gpui::px(10.0)),
            gpui::Modifiers::none(),
        );

        wait_for(cx, "the clicked command to re-render the chrome", |cx| {
            status_bar_text(cx, &bar).contains("total=7")
        });

        // The semantic button retains focus after the click, so Enter must
        // activate the same command through the keyboard path.
        window_cx.simulate_keystrokes("enter");
        wait_for(
            cx,
            "the keyboard-activated command to re-render the chrome",
            |cx| status_bar_text(cx, &bar).contains("total=14"),
        );

        std::fs::remove_dir_all(&root).expect("remove extension root");
    }

    /// §5.4 end to end: a clock-like display-list view refreshes through the
    /// region-scoped channel — the host thread re-invokes only the renderer
    /// method, the controller applies the fresh draw ops to the status bar,
    /// and the VDOM set (and thus the surrounding chrome) is never touched.
    #[gpui::test]
    fn clock_like_display_list_refreshes_without_vdom_invalidation(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("dl-clock").expect("create extension root");
        let directory = root.join("clock");
        std::fs::create_dir_all(&directory).expect("create extension directory");
        std::fs::write(
            directory.join("extension.toml"),
            "[extension]\nname = \"clock\"\n[runtime]\nside = \"client\"\n",
        )
        .expect("write manifest");
        std::fs::write(
            directory.join("main.js"),
            r#"
            function activate(context) {
                var ticks = 0;
                context.registerChromeView('status-bar', {
                    render: function() {
                        return {
                            type: 'display-list',
                            props: { id: 'clock', renderer: 'renderClock' }
                        };
                    },
                    renderClock: function() {
                        ticks++;
                        return [{ op: 'drawText', text: String(ticks), x: 0, y: 0 }];
                    }
                });
            }
            "#,
        )
        .expect("write extension source");

        // §5.6 consent gate: pre-approve the clock so it activates on start.
        let discovered =
            quickjs_runtime::discover_client_extensions(std::slice::from_ref(&root));
        assert_eq!(discovered.len(), 1, "clock extension was not discovered");
        let consent_file = root.join("extension-consent.json");
        let mut consent_records = HashMap::new();
        consent_records.insert(
            "clock".to_string(),
            ConsentRecord::approved("clock", consent_fingerprint(&discovered[0])),
        );
        save_consent_records(&consent_file, &consent_records).expect("write consent records");

        let host = start_consent_host(cx, &root, &consent_file);
        let bar = cx.update(|cx| cx.new(|_| ExtensionStatusBar::new()));
        cx.update(|cx| host.update(cx, |host, cx| host.add_status_bar(bar.downgrade(), cx)));

        // The first chrome push carries the display-list node; the renderer's
        // drawOps were attached during that render.
        wait_for(cx, "the first chrome push with the clock region", |cx| {
            cx.read(|cx| {
                bar.read(cx)
                    .vdom_nodes()
                    .first()
                    .is_some_and(|node| node.element_type == "display-list")
            })
        });
        let vdom_before = chrome_text(cx, &host);

        // One refresh tick: the host re-invokes renderClock and the fresh ops
        // land in the status bar's renderer cache.
        cx.update(|cx| host.update(cx, |host, _| host.send(HostCommand::RenderDisplayLists)));
        wait_for(cx, "the refreshed clock ops to reach the status bar", |cx| {
            cx.read(|cx| bar.read(cx).display_list_ops("clock").is_some())
        });

        // §5.4 the refresh must not have invalidated the chrome: the VDOM set
        // is byte-identical and the region keeps ticking on later refreshes.
        assert_eq!(
            chrome_text(cx, &host),
            vdom_before,
            "a display-list refresh must not re-render the chrome VDOM"
        );
        // Capture the tick value refresh #1 produced; the absolute number is
        // racy (the add_status_bar render may have run renderClock once more),
        // so prove re-evaluation by requiring the next refresh to change it.
        let first_text = cx
            .read(|cx| {
                bar.read(cx)
                    .display_list_ops("clock")
                    .and_then(|ops| match &ops[0] {
                        vdom_bridge::DrawOp::DrawText { text, .. } => Some(text.clone()),
                        _ => None,
                    })
            })
            .expect("first refresh must carry a drawText op");

        cx.update(|cx| host.update(cx, |host, _| host.send(HostCommand::RenderDisplayLists)));
        wait_for(cx, "the clock to tick to a new value", |cx| {
            cx.read(|cx| {
                bar.read(cx)
                    .display_list_ops("clock")
                    .is_some_and(|ops| {
                        matches!(&ops[0], vdom_bridge::DrawOp::DrawText { text, .. } if text != &first_text)
                    })
            })
        });

        std::fs::remove_dir_all(root).expect("remove extension root");
    }
    #[test]
    fn server_chrome_update_replaces_and_removes_view() {
        let mut server = BTreeMap::new();
        let key = ("server-ext".to_string(), "status".to_string());
        let update = |_id: &str, payload: Vec<u8>| mux_protocol::ExtensionChromeUpdate {
            extension_id: key.0.clone(),
            view_id: key.1.clone(),
            vdom_payload: payload,
        };

        apply_server_chrome_node(
            &mut server,
            update("old", br#"{"type":"span","props":{"id":"old"}}"#.to_vec()),
        )
        .expect("initial server chrome must parse");
        apply_server_chrome_node(
            &mut server,
            update("new", br#"{"type":"span","props":{"id":"new"}}"#.to_vec()),
        )
        .expect("replacement server chrome must parse");
        assert_eq!(
            server.get(&key).and_then(|node| node.props.get("id")),
            Some(&serde_json::json!("new"))
        );

        apply_server_chrome_node(&mut server, update("removed", Vec::new()))
            .expect("server chrome removal must succeed");
        assert!(server.is_empty());
    }

    /// §5.2: an oversized VDOM payload is rejected at the JSON boundary before
    /// serde allocates a parse tree.
    #[test]
    fn parse_vdom_json_rejects_oversized_payload() {
        let oversized = format!(
            "{{\"type\":\"div\",\"children\":[\"{}\"]}}",
            "x".repeat(vdom_bridge::MAX_VDOM_PAYLOAD_BYTES)
        );
        let error = parse_vdom_json(&oversized).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("payload of"),
            "expected payload rejection, got: {message}"
        );
    }

    /// §5.2: an oversized server chrome update must fail closed without
    /// partially mutating the live server-chrome cache.
    #[test]
    fn oversized_server_chrome_update_leaves_cache_untouched() {
        let mut server = BTreeMap::new();
        let update = mux_protocol::ExtensionChromeUpdate {
            extension_id: "server-ext".to_string(),
            view_id: "status".to_string(),
            vdom_payload: vec![b'x'; vdom_bridge::MAX_VDOM_PAYLOAD_BYTES + 1],
        };
        let error = apply_server_chrome_node(&mut server, update).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("payload of"),
            "expected payload rejection, got: {message}"
        );
        assert!(
            server.is_empty(),
            "a rejected server update must not mutate the cache"
        );
    }

    /// §5.2: a refreshed display list is capped on serialized size and op
    /// count before it can reach the renderer's cache.
    #[test]
    fn display_list_json_rejects_oversized_payloads() {
        let oversized = format!(
            "[{{\"op\":\"drawText\",\"text\":\"{}\",\"x\":0,\"y\":0}}]",
            "x".repeat(vdom_bridge::MAX_VDOM_PAYLOAD_BYTES)
        );
        let error = parse_display_list_json(&oversized).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("payload of"),
            "expected payload rejection, got: {message}"
        );

        let too_many_ops = format!(
            "[{}]",
            (0..=vdom_bridge::MAX_DISPLAY_LIST_OPS)
                .map(|_| "{\"op\":\"drawText\",\"text\":\"t\",\"x\":0,\"y\":0}".to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let error = parse_display_list_json(&too_many_ops).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("draw ops"),
            "expected op-count rejection, got: {message}"
        );
    }
    #[test]
    fn server_chrome_nodes_merge_after_local_nodes() {
        let local = VDomNode {
            element_type: "span".into(),
            props: [("id".to_string(), serde_json::json!("local"))]
                .into_iter()
                .collect(),
            style: Default::default(),
            children: vec![VDomChild::Text("local".into())],
        };
        let remote = VDomNode {
            element_type: "span".into(),
            props: [("id".to_string(), serde_json::json!("remote"))]
                .into_iter()
                .collect(),
            style: Default::default(),
            children: vec![VDomChild::Text("remote".into())],
        };
        let mut server = BTreeMap::new();
        server.insert(("server-ext".to_string(), "status".to_string()), remote);
        let merged = merge_chrome_nodes(&[local], &server);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].props.get("id"), Some(&serde_json::json!("local")));
        assert_eq!(
            merged[1].props.get("id"),
            Some(&serde_json::json!("remote"))
        );
    }

    // ------------------------------------------------------------------
    // §5.6 consent gate + suspension notice
    // ------------------------------------------------------------------

    /// A minimal chrome extension rendering one identifiable span, so tests
    /// can observe which extensions actually activated and rendered.
    fn write_probe_extension(root: &Path, id: &str, capabilities_toml: &str) {
        let directory = root.join(id);
        std::fs::create_dir_all(&directory).expect("create extension directory");
        std::fs::write(
            directory.join("extension.toml"),
            format!(
                "[extension]\nname = \"{id}\"\nversion = \"1.0.0\"\n[runtime]\nside = \"client\"\n{capabilities_toml}"
            ),
        )
        .expect("write manifest");
        std::fs::write(
            directory.join("main.js"),
            format!(
                r#"
                function activate(context) {{
                    context.registerChromeView('{id}', {{
                        render: function() {{ return {{ type: 'span', children: ['{id}-chrome'] }}; }}
                    }});
                }}
                "#
            ),
        )
        .expect("write extension source");
    }

    fn start_consent_host(
        cx: &mut gpui::TestAppContext,
        root: &Path,
        consent_file: &Path,
    ) -> gpui::Entity<ExtensionHostController> {
        let root = root.to_path_buf();
        let consent_file = consent_file.to_path_buf();
        cx.update(|cx| {
            cx.new(|cx| {
                let mut host = ExtensionHostController::new();
                host.consent_file = consent_file;
                host.start_with_roots(vec![root], cx);
                host
            })
        })
    }

    fn chrome_text(
        cx: &mut gpui::TestAppContext,
        host: &gpui::Entity<ExtensionHostController>,
    ) -> String {
        cx.read(|cx| {
            host.read(cx)
                .local_chrome
                .iter()
                .map(|node| vdom_bridge::vdom_to_text(node, 0))
                .collect::<String>()
        })
    }

    /// §5.6 An unconsented extension must not activate: it shows up as a
    /// pending approval, contributes no chrome, and only renders after the
    /// user approves it.
    #[gpui::test]
    fn unconsented_extension_is_pending_not_activated(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-pending").expect("create extension root");
        write_probe_extension(&root, "probe", "[capabilities]\nmux = true\n");
        write_probe_extension(&root, "canary", "");

        // Pre-consent only the canary; the probe must wait for the user.
        let consent_file = root.join("extension-consent.json");
        let discovered =
            quickjs_runtime::discover_client_extensions(std::slice::from_ref(&root));
        let canary = discovered
            .iter()
            .find(|extension| extension.manifest.id == "canary")
            .expect("canary discovered");
        let mut records = HashMap::new();
        records.insert(
            "canary".to_string(),
            ConsentRecord::approved("canary", consent_fingerprint(canary)),
        );
        save_consent_records(&consent_file, &records).expect("write consent records");

        let host = start_consent_host(cx, &root, &consent_file);

        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| {
                let pending = host.read(cx).pending_approvals();
                pending.len() == 1 && pending[0].id == "probe"
            })
        });
        cx.read(|cx| {
            let approval = &host.read(cx).pending_approvals()[0];
            assert_eq!(approval.version, "1.0.0", "approval carries the version");
            assert_eq!(
                approval.capabilities_summary, "mux",
                "approval carries the capability list the prompt shows"
            );
        });

        // The consented canary renders; the pending probe contributes nothing.
        wait_for(cx, "the canary chrome", |cx| {
            chrome_text(cx, &host).contains("canary-chrome")
        });
        assert!(
            !chrome_text(cx, &host).contains("probe-chrome"),
            "an unconsented extension must not render"
        );

        // Approving activates the probe and persists its consent.
        cx.update(|cx| {
            host.update(cx, |host, cx| {
                host.approve_extensions(&["probe".to_string()], cx)
                    .expect("approve persists")
            })
        });
        wait_for(cx, "the probe chrome after approval", |cx| {
            chrome_text(cx, &host).contains("probe-chrome")
        });
        let probe = discovered
            .iter()
            .find(|extension| extension.manifest.id == "probe")
            .expect("probe discovered");
        let records = load_consent_records(&consent_file);
        let record = records
            .get("probe")
            .expect("approval must persist a consent record");
        assert_eq!(
            record.state,
            ConsentState::Approved,
            "approval must persist the approved state"
        );
        assert_eq!(
            record.policy_fingerprint, consent_fingerprint(probe),
            "approval must persist the exact approved policy fingerprint"
        );

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 Consent survives a restart: an approved extension activates
    /// immediately on the next start without landing in pending approvals.
    #[gpui::test]
    fn consent_persists_across_restart(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-restart").expect("create extension root");
        write_probe_extension(&root, "probe", "");
        let consent_file = root.join("extension-consent.json");

        let host = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| host.read(cx).pending_approvals().len() == 1)
        });
        cx.update(|cx| {
            host.update(cx, |host, cx| {
                host.approve_extensions(&["probe".to_string()], cx)
                    .expect("approve persists")
            })
        });
        wait_for(cx, "the probe chrome after approval", |cx| {
            chrome_text(cx, &host).contains("probe-chrome")
        });

        // Restart: a fresh controller over the same roots and consent store
        // must activate the probe without prompting again.
        let restarted = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe chrome after restart", |cx| {
            chrome_text(cx, &restarted).contains("probe-chrome")
        });
        cx.read(|cx| {
            assert!(
                restarted.read(cx).pending_approvals().is_empty(),
                "consented extensions must not re-prompt after restart"
            );
        });

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 A denied extension records an explicit Denied decision: it renders
    /// nothing and the exact same manifest is never re-prompted on later starts.
    #[gpui::test]
    fn denied_extension_not_reprompted(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-deny").expect("create extension root");
        write_probe_extension(&root, "probe", "");
        write_probe_extension(&root, "canary", "");

        let consent_file = root.join("extension-consent.json");
        let discovered =
            quickjs_runtime::discover_client_extensions(std::slice::from_ref(&root));
        let canary = discovered
            .iter()
            .find(|extension| extension.manifest.id == "canary")
            .expect("canary discovered");
        let mut records = HashMap::new();
        records.insert(
            "canary".to_string(),
            ConsentRecord::approved("canary", consent_fingerprint(canary)),
        );
        save_consent_records(&consent_file, &records).expect("write consent records");

        let host = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| host.read(cx).pending_approvals().len() == 1)
        });
        cx.update(|cx| {
            host.update(cx, |host, _| {
                host.deny_extensions(&["probe".to_string()])
                    .expect("deny persists")
            })
        });
        cx.read(|cx| {
            assert!(
                host.read(cx).pending_approvals().is_empty(),
                "denying removes the approval"
            );
            let records = load_consent_records(&consent_file);
            let record = records
                .get("probe")
                .expect("denial must persist a consent record");
            assert_eq!(
                record.state,
                ConsentState::Denied,
                "denial must persist the denied state"
            );
        });

        // Restart: the denied manifest must not re-prompt and must not render,
        // while the consented canary still paints.
        let restarted = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the canary chrome after restart", |cx| {
            chrome_text(cx, &restarted).contains("canary-chrome")
        });
        cx.read(|cx| {
            assert!(
                restarted.read(cx).pending_approvals().is_empty(),
                "a denied extension must not be re-prompted"
            );
        });
        assert!(
            !chrome_text(cx, &restarted).contains("probe-chrome"),
            "a denied extension must not render"
        );

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 A denied extension whose manifest later changes (capabilities,
    /// version, limits, side) must be re-prompted, not silently suppressed by
    /// an id-wide sentinel. The decision is keyed by the exact policy that was
    /// decided; a different policy invalidates it regardless of prior allow/deny.
    #[gpui::test]
    fn denied_manifest_change_reprompts(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-deny-reprompt").expect("create extension root");
        write_probe_extension(&root, "probe", "");

        let consent_file = root.join("extension-consent.json");
        let host = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| host.read(cx).pending_approvals().len() == 1)
        });
        let denied_fingerprint = cx.read(|cx| {
            host.read(cx).pending_approvals()[0]
                .policy_fingerprint
                .clone()
        });
        cx.update(|cx| {
            host.update(cx, |host, _| {
                host.deny_extensions(&["probe".to_string()])
                    .expect("deny persists")
            })
        });
        cx.read(|cx| {
            let records = load_consent_records(&consent_file);
            let record = records.get("probe").expect("denial persisted");
            assert_eq!(
                record.state,
                ConsentState::Denied,
                "denial must persist the denied state"
            );
            assert_eq!(
                record.policy_fingerprint, denied_fingerprint,
                "denial must persist the exact decided policy fingerprint"
            );
        });

        // Change the manifest: capabilities and version both differ. The new
        // policy fingerprint must not match the denied one, so the extension
        // re-enters pending approvals on the next start instead of staying
        // silently disabled.
        let probe_dir = root.join("probe");
        std::fs::write(
            probe_dir.join("extension.toml"),
            "[extension]\nname = \"probe\"\nversion = \"2.0.0\"\n[runtime]\nside = \"client\"\n[capabilities]\nmux = true\n",
        )
        .expect("rewrite manifest with changed policy");

        let restarted = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the changed probe to re-enter pending approvals", |cx| {
            cx.read(|cx| {
                let pending = restarted.read(cx).pending_approvals();
                pending.len() == 1 && pending[0].id == "probe"
            })
        });
        cx.read(|cx| {
            let record = load_consent_records(&consent_file)
                .get("probe")
                .expect("the prior denial record is still present")
                .clone();
            assert_eq!(
                record.state,
                ConsentState::Denied,
                "the prior denial record must remain denied"
            );
        });
        assert!(
            !chrome_text(cx, &restarted).contains("probe-chrome"),
            "a re-prompted (still undecided) extension must not render"
        );

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 An approved extension whose manifest later changes must be
    /// re-prompted: the stored approval names the exact policy that was
    /// decided, and a manifest with a different fingerprint is pending again
    /// instead of silently activating under the stale approval.
    #[gpui::test]
    fn approved_manifest_change_reprompts(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-approve-reprompt").expect("create extension root");
        write_probe_extension(&root, "probe", "");

        let consent_file = root.join("extension-consent.json");
        let host = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| host.read(cx).pending_approvals().len() == 1)
        });
        cx.update(|cx| {
            host.update(cx, |host, cx| {
                host.approve_extensions(&["probe".to_string()], cx)
                    .expect("approve persists")
            })
        });
        wait_for(cx, "the probe chrome after approval", |cx| {
            chrome_text(cx, &host).contains("probe-chrome")
        });

        // Change the manifest: capabilities and version both differ from what
        // was approved, so the stored approval must no longer match.
        let probe_dir = root.join("probe");
        std::fs::write(
            probe_dir.join("extension.toml"),
            "[extension]\nname = \"probe\"\nversion = \"2.0.0\"\n[runtime]\nside = \"client\"\n[capabilities]\nmux = true\n",
        )
        .expect("rewrite manifest with changed policy");

        // Restart: the changed manifest must not auto-activate under the stale
        // approval; it re-enters pending approvals until the user decides again.
        let restarted = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the changed probe to re-enter pending approvals", |cx| {
            cx.read(|cx| {
                let pending = restarted.read(cx).pending_approvals();
                pending.len() == 1 && pending[0].id == "probe"
            })
        });
        assert!(
            !chrome_text(cx, &restarted).contains("probe-chrome"),
            "a manifest changed since approval must not auto-activate"
        );

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 A persistence failure during approve/deny must surface as an error
    /// and must not activate the extension nor drop it from pending approvals.
    /// Saving is atomic and fallible; the controller refuses to mutate pending
    /// state or notify the host when the store cannot be written.
    #[gpui::test]
    fn persistence_failure_keeps_pending_and_blocks_activation(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-persist-fail").expect("create extension root");
        write_probe_extension(&root, "probe", "");

        // Make the consent store unwritable: a file (not a directory) at the
        // parent path so the atomic temp-file + rename commit cannot succeed.
        let blocker = root.join("blocker");
        std::fs::write(&blocker, "block").expect("write blocker file");
        let consent_file = blocker.join("extension-consent.json");

        let host = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| host.read(cx).pending_approvals().len() == 1)
        });

        let approve_result = cx.update(|cx| {
            host.update(cx, |host, cx| {
                host.approve_extensions(&["probe".to_string()], cx)
            })
        });
        assert!(
            approve_result.is_err(),
            "approve must return an error when the consent store is unwritable"
        );
        cx.read(|cx| {
            assert_eq!(
                host.read(cx).pending_approvals().len(),
                1,
                "a failed approve must leave pending approvals intact"
            );
        });
        // Give the host thread a beat; nothing must activate.
        wait_for(cx, "no activation occurs", |cx| {
            !chrome_text(cx, &host).contains("probe-chrome")
        });
        assert!(
            !chrome_text(cx, &host).contains("probe-chrome"),
            "an extension whose consent could not be persisted must not activate"
        );

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 The prompt claim is global to the controller so two workspaces
    /// cannot race the same pending batch: only one caller can claim at a
    /// time, and releasing makes the batch claimable again.
    #[gpui::test]
    fn prompt_claim_is_global_and_releasable(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        let root = temporary_extension_dir("consent-claim").expect("create extension root");
        write_probe_extension(&root, "probe", "");
        let consent_file = root.join("extension-consent.json");

        let host = start_consent_host(cx, &root, &consent_file);
        wait_for(cx, "the probe to land in pending approvals", |cx| {
            cx.read(|cx| host.read(cx).pending_approvals().len() == 1)
        });

        let first_claim = cx.update(|cx| {
            host.update(cx, |host, cx| host.claim_pending_prompt(cx))
        });
        assert!(first_claim, "the first claimant must succeed");
        let second_claim = cx.update(|cx| {
            host.update(cx, |host, cx| host.claim_pending_prompt(cx))
        });
        assert!(
            !second_claim,
            "a second claimant must be refused while the prompt is in flight"
        );
        cx.update(|cx| host.update(cx, |host, cx| host.release_pending_prompt(cx)));
        let third_claim = cx.update(|cx| {
            host.update(cx, |host, cx| host.claim_pending_prompt(cx))
        });
        assert!(
            third_claim,
            "releasing the claim must make the pending batch claimable again"
        );

        std::fs::remove_dir_all(root).expect("remove extension root");
    }

    /// §5.6 Suspension must notify the user: once an extension violates a
    /// resource limit, the next forced chrome push carries a synthetic notice
    /// naming it (reuses the runaway CPU setup).
    #[test]
    fn suspended_extension_notice_in_chrome() -> Result<()> {
        let root = temporary_extension_dir("suspended-notice-root")?;
        let extension = root.join("runaway");
        std::fs::create_dir_all(&extension)?;
        std::fs::write(
            extension.join("extension.toml"),
            "[extension]\nname = \"runaway\"\n[runtime]\nside = \"client\"\n[resources]\ncpu_budget_ms = 1\n",
        )?;
        std::fs::write(
            extension.join("main.js"),
            r#"
            function activate(context) {
                context.registerChromeView('runaway', {
                    render: function() { return { type: 'span', children: ['alive'] }; }
                });
                context.on('spin', function() { while (true) {} });
            }
            "#,
        )?;

        let mut live_extensions = activate_extensions(quickjs_runtime::discover_client_extensions(
            std::slice::from_ref(&root),
        ));
        assert_eq!(live_extensions.len(), 1);
        assert!(
            !render_live_extensions(&mut live_extensions).is_empty(),
            "a healthy extension must render"
        );
        assert!(
            live_extensions[0].live.emit_event("spin", "null").is_err(),
            "the runaway handler must be interrupted"
        );
        live_extensions[0].note_resource_violations();
        assert!(live_extensions[0].suspended, "CPU violation must suspend");

        // The forced push after suspension must carry the user-visible notice.
        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<Vec<VDomNode>>();
        assert!(push_chrome_if_dirty(&mut live_extensions, &sender, true));
        let nodes = futures::StreamExt::next(&mut receiver)
            .now_or_never()
            .flatten()
            .context("forced chrome push never arrived")?;
        let text = nodes
            .iter()
            .map(|node| vdom_bridge::vdom_to_text(node, 0))
            .collect::<String>();
        assert!(
            text.contains("suspended"),
            "status bar vdom text must announce the suspension: {text}"
        );
        assert!(
            text.contains("runaway"),
            "the notice must name the suspended extension: {text}"
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// Host bridge that admits every call and counts them, so tests can
    /// observe how many calls the runtime's IO token bucket let through.
    #[derive(Default)]
    struct CountingBridge {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingBridge {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl HostBridge for CountingBridge {
        fn call(&self, _method: &str, _args: &serde_json::Value) -> Result<serde_json::Value> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(serde_json::json!(true))
        }
    }

    /// Load one probe extension with the given manifest `io_rate_limit` and a
    /// counting bridge, returning the supervisor-side handle and the bridge
    /// (whose counter reveals how many host calls the token bucket admitted).
    fn load_io_probe(
        root: &Path,
        id: &str,
        io_rate_limit: f64,
        main_js: &str,
    ) -> Result<(HostedExtension, Arc<CountingBridge>)> {
        let directory = root.join(id);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("extension.toml"),
            format!(
                "[extension]\nname = \"{id}\"\n[runtime]\nside = \"client\"\n[capabilities]\nmux = true\n[resources]\nio_rate_limit = {io_rate_limit}\n"
            ),
        )?;
        std::fs::write(directory.join("main.js"), main_js)?;
        let roots = [root.to_path_buf()];
        let discovered = quickjs_runtime::discover_client_extensions(&roots);
        let extension = discovered
            .iter()
            .find(|extension| extension.manifest.id == id)
            .context("probe extension not discovered")?;
        let bridge = Arc::new(CountingBridge::default());
        let runner = quickjs_runtime::ExtensionRunner::for_manifest(&extension.manifest)
            .with_bridge(bridge.clone());
        let live = runner.load_live(&extension.manifest.id, &extension.source, "activate")?;
        Ok((
            HostedExtension {
                live,
                suspended: false,
                suspension_reason: None,
            },
            bridge,
        ))
    }

    /// Calls past the burst capacity (2/s → capacity 4) are rejected in Rust;
    /// the JS catches every exception, so only the persistent violation flag
    /// can prove the quota was crossed.
    const IO_OVER_LIMIT_JS: &str = r#"
        function activate(context) {
            context.on('tick', function() {});
            for (var i = 0; i < 8; i++) {
                try { context.mux.focusPane('p' + i); } catch (error) {}
            }
        }
    "#;

    /// §5.6: an extension whose host calls were rejected by its
    /// `io_rate_limit` must be suspended even though the JS caught the
    /// exceptions — and the suspension carries a visible reason, denies all
    /// further work, and is not bypassed when the token bucket refills.
    #[test]
    fn io_rate_limit_suspends_extension_and_denies_further_work() -> Result<()> {
        let root = temporary_extension_dir("io-supervisor-root")?;
        let (hosted, bridge) = load_io_probe(&root, "io-limit", 2.0, IO_OVER_LIMIT_JS)?;
        assert_eq!(bridge.calls(), 4, "burst capacity (2× rate) admits 4 calls");
        let mut live_extensions = vec![hosted];

        // Work within the limit is accepted: the active extension still
        // receives host events.
        assert_eq!(
            deliver_event(&mut live_extensions, "tick", "null"),
            1,
            "an active extension must receive events"
        );

        live_extensions[0].note_resource_violations();
        assert!(
            live_extensions[0].suspended,
            "IO violation must suspend the extension"
        );
        assert_eq!(
            live_extensions[0].suspension_reason,
            Some("io rate limit exceeded"),
            "suspension must carry an actionable reason"
        );

        // Subsequent work is denied at every dispatch point.
        assert_eq!(
            deliver_event(&mut live_extensions, "tick", "null"),
            0,
            "a suspended extension must not receive further events"
        );
        assert!(
            render_live_extensions(&mut live_extensions).is_empty(),
            "a suspended extension must stop contributing chrome"
        );

        // The reason surfaces through the existing chrome notice.
        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<Vec<VDomNode>>();
        assert!(push_chrome_if_dirty(&mut live_extensions, &sender, true));
        let nodes = futures::StreamExt::next(&mut receiver)
            .now_or_never()
            .flatten()
            .context("forced chrome push never arrived")?;
        let text = nodes
            .iter()
            .map(|node| vdom_bridge::vdom_to_text(node, 0))
            .collect::<String>();
        assert!(text.contains("suspended"), "notice text: {text}");
        assert!(text.contains("io-limit"), "notice must name the extension: {text}");
        assert!(
            text.contains("io rate limit"),
            "notice must carry the reason: {text}"
        );

        // Refilling the token bucket must not resurrect the extension.
        std::thread::sleep(Duration::from_millis(2500));
        live_extensions[0].note_resource_violations();
        assert!(
            live_extensions[0].suspended,
            "suspension is permanent; refill is not a bypass"
        );
        assert_eq!(
            deliver_event(&mut live_extensions, "tick", "null"),
            0,
            "a suspended extension stays cut off from further work"
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// §5.6: `io_rate_limit = 0` means unlimited (same contract as the memory
    /// and CPU limits) — every host call is admitted and nothing suspends.
    #[test]
    fn io_rate_limit_zero_never_suspends() -> Result<()> {
        let root = temporary_extension_dir("io-unlimited-root")?;
        let (hosted, bridge) = load_io_probe(
            &root,
            "io-unlimited",
            0.0,
            r#"
            function activate(context) {
                context.on('tick', function() {});
                for (var i = 0; i < 100; i++) {
                    try { context.mux.focusPane('p' + i); } catch (error) {}
                }
            }
            "#,
        )?;
        assert_eq!(
            bridge.calls(),
            100,
            "io_rate_limit = 0 must admit every host call"
        );
        let mut live_extensions = vec![hosted];
        live_extensions[0].note_resource_violations();
        assert!(
            !live_extensions[0].suspended,
            "unlimited IO must never suspend the extension"
        );
        assert_eq!(
            deliver_event(&mut live_extensions, "tick", "null"),
            1,
            "an unlimited extension must keep receiving events"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// Host bridge admitting every call with a canned value, recording the
    /// method names so tests can prove which declared capabilities reached
    /// the bridge through the manifest → context → hostCall path.
    #[derive(Default)]
    struct DeclaredCapsBridge {
        methods: Mutex<Vec<String>>,
    }

    impl DeclaredCapsBridge {
        fn methods(&self) -> Vec<String> {
            self.methods.lock().clone()
        }
    }

    impl HostBridge for DeclaredCapsBridge {
        fn call(&self, method: &str, _args: &serde_json::Value) -> Result<serde_json::Value> {
            self.methods.lock().push(method.to_string());
            match method {
                "workspace.getPath" => Ok(serde_json::json!("/work")),
                "filesystem.readTextFile" => Ok(serde_json::json!("text")),
                "filesystem.readDir" => Ok(serde_json::json!([])),
                "network.fetch" => Ok(serde_json::json!({ "status": 200 })),
                "process.spawn" => Ok(serde_json::json!("pid")),
                other => bail!("unexpected host method: {other}"),
            }
        }
    }

    /// §5.6: 客户端 manifest 声明 workspace/filesystem/network/process_spawn
    /// 后, 调用必须真正到达宿主桥 (declared-but-reachable), 返回值回到扩展;
    /// 未声明的能力在到达桥之前被拒绝 (JS requireCapability + Rust allows)。
    #[test]
    fn client_declared_capabilities_reach_the_host_bridge() -> Result<()> {
        let root = temporary_extension_dir("declared-caps")?;
        let directory = root.join("caps");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("extension.toml"),
            "[extension]\nname = \"caps\"\n[runtime]\nside = \"client\"\n[capabilities]\nworkspace = true\nfilesystem = \"home\"\nnetwork = true\nprocess_spawn = true\n",
        )?;
        std::fs::write(
            directory.join("main.js"),
            r#"
            function activate(context) {
                context.registerChromeView('caps', {
                    render: function() {
                        return { type: 'span', children: [
                            context.workspace.getPath() + '|' +
                            context.filesystem.readTextFile('/x') + '|' +
                            String(context.network.fetch('http://example.test').status) + '|' +
                            context.process.spawn('echo')
                        ] };
                    }
                });
            }
            "#,
        )?;

        let discovered = quickjs_runtime::discover_client_extensions(std::slice::from_ref(&root));
        let extension = discovered
            .iter()
            .find(|extension| extension.manifest.id == "caps")
            .context("caps extension not discovered")?;
        let bridge = Arc::new(DeclaredCapsBridge::default());
        let runner = quickjs_runtime::ExtensionRunner::for_manifest(&extension.manifest)
            .with_bridge(bridge.clone());
        let live = runner.load_live(&extension.manifest.id, &extension.source, "activate")?;
        let vdom = live.render_now()?.context("caps vdom")?;
        assert!(
            vdom.contains("/work|text|200|pid"),
            "声明能力的返回值必须回到扩展: vdom={vdom}"
        );
        assert_eq!(
            bridge.methods(),
            vec![
                "workspace.getPath",
                "filesystem.readTextFile",
                "network.fetch",
                "process.spawn",
            ],
            "manifest 声明的能力必须到达宿主桥"
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// §16.7: command ownership is first-wins; a second extension claiming
    /// the same id is rejected (both bare and namespaced forms of the
    /// colliding registration are unreachable), and re-applying the same
    /// snapshot keeps the original owner.
    #[test]
    fn registry_first_wins_and_rejects_collisions() {
        let mut registry = CommandRegistry::default();
        let report_a = RegistryReport {
            extension_id: "ext-a".to_string(),
            commands: vec![
                RegisteredCommand {
                    id: "noop".to_string(),
                    label: "Noop".to_string(),
                },
                RegisteredCommand {
                    id: "unique".to_string(),
                    label: "Unique".to_string(),
                },
            ],
            keymaps: vec![RegisteredKeymap {
                chord: "ctrl+shift+p".to_string(),
                command: "noop".to_string(),
            }],
        };
        let report_b = RegistryReport {
            extension_id: "ext-b".to_string(),
            commands: vec![RegisteredCommand {
                id: "noop".to_string(),
                label: "Noop Two".to_string(),
            }],
            keymaps: Vec::new(),
        };

        assert_eq!(registry.apply_report(&report_a), Vec::<RegistryConflict>::new());
        assert_eq!(
            registry.apply_report(&report_b),
            vec![RegistryConflict {
                extension_id: "ext-b".to_string(),
                detail: "command id \"noop\" is already owned by extension \"ext-a\"; use \"ext-b.noop\" to reach this one"
                    .to_string(),
            }],
            "colliding namespaced id must be reported as rejected"
        );
        assert_eq!(registry.resolve_owner("noop"), Some("ext-a"));
        assert_eq!(registry.resolve_owner("ext-a.noop"), Some("ext-a"));
        assert_eq!(registry.resolve_owner("ext-a.unique"), Some("ext-a"));
        assert_eq!(
            registry.resolve_owner("ext-b.noop"),
            Some("ext-b"),
            "the bare id is taken, but the namespaced id must still reach its owner"
        );
        assert_eq!(registry.resolve_owner("missing"), None);

        // Full-snapshot re-application (host approval flow) is idempotent.
        assert_eq!(
            registry.apply_reports(&[report_a, report_b]),
            vec![RegistryConflict {
                extension_id: "ext-b".to_string(),
                detail: "command id \"noop\" is already owned by extension \"ext-a\"; use \"ext-b.noop\" to reach this one"
                    .to_string(),
            }]
        );
        assert_eq!(registry.resolve_owner("noop"), Some("ext-a"));
    }

    /// §16.7: declared chords use `+` separators in JS manifests but gpui
    /// keystrokes use `-`; the registry must normalize so the pane resolver
    /// snapshot matches real keystrokes.
    #[test]
    fn registry_normalizes_chords_for_the_resolver() {
        let mut registry = CommandRegistry::default();
        registry.apply_report(&RegistryReport {
            extension_id: "palette".to_string(),
            commands: vec![RegisteredCommand {
                id: "z3rm.command-palette.open".to_string(),
                label: "Open Palette".to_string(),
            }],
            keymaps: vec![RegisteredKeymap {
                chord: "ctrl+shift+p".to_string(),
                command: "z3rm.command-palette.open".to_string(),
            }],
        });
        let snapshot = registry.keymap_snapshot();
        assert_eq!(
            snapshot.get("ctrl-shift-p").map(String::as_str),
            Some("z3rm.command-palette.open")
        );
        assert_eq!(
            snapshot.get("ctrl+shift+p"),
            None,
            "the declared chord must be normalized to hyphen form"
        );
        assert_eq!(normalize_chord("ctrl+shift+p"), "ctrl-shift-p");
        assert_eq!(normalize_chord("ctrl-shift-p"), "ctrl-shift-p");
        assert_eq!(normalize_chord("ctrl++p"), "ctrl-p");
    }

    /// §16.7: the resolver handed to panes matches a real keystroke against
    /// the normalized chord snapshot and returns the declared action; an
    /// unbound keystroke falls through (None), preserving native routing.
    #[test]
    fn extension_shortcut_resolver_matches_normalized_chords() {
        let mut controller = ExtensionHostController::new();
        controller.commands.apply_report(&RegistryReport {
            extension_id: "palette".to_string(),
            commands: vec![RegisteredCommand {
                id: "z3rm.command-palette.open".to_string(),
                label: "Open Palette".to_string(),
            }],
            keymaps: vec![RegisteredKeymap {
                chord: "ctrl+shift+p".to_string(),
                command: "z3rm.command-palette.open".to_string(),
            }],
        });
        *controller.keymap_snapshot.lock() = controller.commands.keymap_snapshot();
        let resolver = controller.extension_shortcut_resolver();

        let bound = Keystroke::parse("ctrl-shift-p").expect("parse ctrl-shift-p");
        assert_eq!(
            resolver(&bound).map(String::from),
            Some("z3rm.command-palette.open".to_string())
        );
        let unbound = Keystroke::parse("ctrl-a").expect("parse ctrl-a");
        assert_eq!(resolver(&unbound), None);
    }

    /// §16.7: labels the runtime ships (`{id, label}`) survive into the
    /// directory; a missing label falls back to the command id.
    #[test]
    fn command_labels_survive_parsing_with_id_fallback() {
        let parsed = parse_registered_commands(
            r#"[{"id":"open","label":"Open Palette"},{"id":"bare"}]"#,
            "ext",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "open");
        assert_eq!(parsed[0].label, "Open Palette");
        assert_eq!(parsed[1].id, "bare");
        assert_eq!(parsed[1].label, "bare");
    }

    /// §16.7: two extensions declaring the same chord get a readable
    /// conflict, and only the first owner's chord stays dispatchable.
    #[test]
    fn chord_collisions_reject_the_later_extension() {
        let mut registry = CommandRegistry::default();
        let report_a = RegistryReport {
            extension_id: "ext-a".to_string(),
            commands: vec![RegisteredCommand {
                id: "one".to_string(),
                label: "One".to_string(),
            }],
            keymaps: vec![RegisteredKeymap {
                chord: "ctrl+alt+e".to_string(),
                command: "one".to_string(),
            }],
        };
        let report_b = RegistryReport {
            extension_id: "ext-b".to_string(),
            commands: vec![RegisteredCommand {
                id: "two".to_string(),
                label: "Two".to_string(),
            }],
            keymaps: vec![RegisteredKeymap {
                chord: "ctrl+alt+e".to_string(),
                command: "two".to_string(),
            }],
        };
        assert_eq!(registry.apply_report(&report_a), Vec::<RegistryConflict>::new());
        assert_eq!(
            registry.apply_report(&report_b),
            vec![RegistryConflict {
                extension_id: "ext-b".to_string(),
                detail: "chord \"ctrl+alt+e\" is already bound by extension \"ext-a\"".to_string(),
            }]
        );
        assert_eq!(registry.keymap_snapshot().get("ctrl-alt-e").map(String::as_str), Some("one"));
    }

    /// §16.7: the directory exposes owner, bare id, namespaced id, and
    /// label for every owned command.
    #[test]
    fn directory_carries_owner_and_label() {
        let mut registry = CommandRegistry::default();
        registry.apply_report(&RegistryReport {
            extension_id: "ext-a".to_string(),
            commands: vec![RegisteredCommand {
                id: "noop".to_string(),
                label: "Noop".to_string(),
            }],
            keymaps: Vec::new(),
        });
        let directory = registry.command_directory();
        assert_eq!(directory.len(), 1);
        assert_eq!(
            directory[0],
            ExtensionCommandEntry {
                extension_id: "ext-a".to_string(),
                command_id: "noop".to_string(),
                namespaced_id: "ext-a.noop".to_string(),
                label: "Noop".to_string(),
            }
        );
    }

    /// §16.7: palette interception fuzzy-matches the directory, highlights
    /// matched positions, and produces an action that names exactly the
    /// owning extension.
    #[test]
    fn palette_interception_returns_owner_actions() {
        let directory = vec![ExtensionCommandEntry {
            extension_id: "palette".to_string(),
            command_id: "open".to_string(),
            namespaced_id: "palette.open".to_string(),
            label: "Open Palette".to_string(),
        }];
        let result = palette_interception("open", &directory);
        assert_eq!(result.results.len(), 1);
        let item = &result.results[0];
        assert_eq!(item.string, "Open Palette — palette");
        assert!(!item.positions.is_empty(), "matched characters must be highlighted");
        let action = item
            .action
            .as_any()
            .downcast_ref::<InvokeExtensionCommand>()
            .expect("palette items carry InvokeExtensionCommand");
        assert_eq!(action.extension_id, "palette");
        assert_eq!(action.command, "open");

        // A non-matching query and an empty directory both yield nothing,
        // leaving the native action list untouched (exclusive stays false).
        assert!(palette_interception("zzz", &directory).results.is_empty());
        assert!(palette_interception("", &[]).results.is_empty());
        assert!(!result.exclusive);
    }

    /// §16.7: chord resolution matches a real keystroke to its owning
    /// extension and command; unbound keystrokes never resolve.
    #[test]
    fn chord_resolution_reports_the_owner() {
        let mut registry = CommandRegistry::default();
        registry.apply_report(&RegistryReport {
            extension_id: "ext-a".to_string(),
            commands: vec![RegisteredCommand {
                id: "one".to_string(),
                label: "One".to_string(),
            }],
            keymaps: vec![RegisteredKeymap {
                chord: "ctrl+alt+e".to_string(),
                command: "one".to_string(),
            }],
        });
        let bound = Keystroke::parse("ctrl-alt-e").expect("parse bound chord");
        assert_eq!(
            registry.resolve_keystroke(&bound),
            Some(("ext-a".to_string(), "one".to_string()))
        );
        let unbound = Keystroke::parse("ctrl-alt-y").expect("parse unbound chord");
        assert_eq!(registry.resolve_keystroke(&unbound), None);
    }

    /// The platform fills `key_char` on keys typed without ctrl/cmd/fn, and
    /// unconditionally on space/tab/enter, while a parsed chord never carries
    /// one. Hashing the whole `Keystroke` therefore missed exactly the chords
    /// an extension is most likely to declare.
    #[test]
    fn chord_resolution_ignores_the_typed_character() {
        let mut registry = CommandRegistry::default();
        registry.apply_report(&RegistryReport {
            extension_id: "ext-a".to_string(),
            commands: vec![RegisteredCommand {
                id: "fire".to_string(),
                label: "Fire".to_string(),
            }],
            keymaps: vec![RegisteredKeymap {
                chord: "alt-space".to_string(),
                command: "fire".to_string(),
            }],
        });

        let mut pressed = Keystroke::parse("alt-space").expect("parse chord");
        pressed.key_char = Some(" ".to_string());
        assert_eq!(
            registry.resolve_keystroke(&pressed),
            Some(("ext-a".to_string(), "fire".to_string()))
        );
    }

    /// Two extensions may register the same bare command id. The directory a
    /// runtime shows must therefore carry ids that resolve to exactly one
    /// owner, or clicking the entry labelled with one extension runs the
    /// other's command.
    #[test]
    fn external_directory_ids_resolve_to_one_owner() {
        let reports = vec![
            RegistryReport {
                extension_id: "ext-a".to_string(),
                commands: vec![RegisteredCommand {
                    id: "dupe".to_string(),
                    label: "A Thing".to_string(),
                }],
                keymaps: Vec::new(),
            },
            RegistryReport {
                extension_id: "ext-b".to_string(),
                commands: vec![RegisteredCommand {
                    id: "dupe".to_string(),
                    label: "B Thing".to_string(),
                }],
                keymaps: Vec::new(),
            },
        ];
        let mut registry = CommandRegistry::default();
        registry.apply_reports(&reports);

        let for_a = external_command_directory(&reports, "ext-a");
        let id_b = for_a[0]["id"].as_str().expect("directory id");
        assert_eq!(id_b, "ext-b.dupe");
        assert_eq!(registry.resolve_owner(id_b), Some("ext-b"));

        let for_b = external_command_directory(&reports, "ext-b");
        let id_a = for_b[0]["id"].as_str().expect("directory id");
        assert_eq!(registry.resolve_owner(id_a), Some("ext-a"));
    }

    /// §16.7: the app-wide keystroke interceptor leaves a keystroke alone
    /// when a native binding owns it — an extension chord must never
    /// shadow a core action — and lets an unclaimed keystroke reach the
    /// focused view normally.
    #[gpui::test]
    async fn global_shortcuts_never_shadow_native_bindings(cx: &mut gpui::TestAppContext) {
        gpui::actions!(extension_shortcut_tests, [NativeProbe]);

        struct KeystrokeProbeView {
            received: std::rc::Rc<std::cell::Cell<bool>>,
            focus_handle: gpui::FocusHandle,
        }
        impl gpui::Focusable for KeystrokeProbeView {
            fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
                self.focus_handle.clone()
            }
        }
        impl gpui::Render for KeystrokeProbeView {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                use gpui::{InteractiveElement as _, StatefulInteractiveElement as _};
                let received = self.received.clone();
                let focus_handle = self.focus_handle.clone();
                gpui::div()
                    .id("keystroke-probe")
                    .track_focus(&focus_handle)
                    .on_key_down(cx.listener(
                        move |_this, _event: &gpui::KeyDownEvent, _window, _cx| {
                            received.set(true);
                        },
                    ))
            }
        }

        let controller = cx.new(|_| {
            let mut controller = ExtensionHostController::new();
            controller.commands.apply_report(&RegistryReport {
                extension_id: "ext-a".to_string(),
                commands: vec![RegisteredCommand {
                    id: "fire".to_string(),
                    label: "Fire".to_string(),
                }],
                keymaps: vec![RegisteredKeymap {
                    chord: "ctrl+alt+e".to_string(),
                    command: "fire".to_string(),
                }],
            });
            *controller.keymap_snapshot.lock() = controller.commands.keymap_snapshot();
            controller
        });
        let native_fired = std::rc::Rc::new(std::cell::Cell::new(false));
        let native_flag = native_fired.clone();
        cx.update(|cx| {
            cx.set_global(GlobalHostController(controller));
            install_global_shortcuts(cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-alt-e",
                NativeProbe,
                None,
            )]);
            cx.on_action(move |_: &NativeProbe, _| {
                native_flag.set(true);
            });
        });

        let received = std::rc::Rc::new(std::cell::Cell::new(false));
        let received_for_view = received.clone();
        let window_handle = cx.add_window(|_window, cx| KeystrokeProbeView {
            received: received_for_view,
            focus_handle: cx.focus_handle(),
        });
        let any_window: gpui::AnyWindowHandle = window_handle.into();
        {
            use gpui::Focusable as _;
            let root = window_handle
                .root(cx)
                .expect("probe window root view must exist");
            let focus_handle = cx.read(|cx| root.read(cx).focus_handle(cx).clone());
            cx.update_window(any_window, |_, window, cx| focus_handle.focus(window, cx))
                .expect("focus the probe view");
        }
        let keystroke = Keystroke::parse("ctrl-alt-e").expect("parse shared chord");

        // Native binding owns the chord: the extension interceptor must not
        // consume it, so the native action fires.
        cx.dispatch_keystroke(any_window, keystroke);
        assert!(
            native_fired.get(),
            "a native binding must win over an extension chord"
        );

        // A chord no native binding uses must still reach the focused view —
        // interceptor consumption only happens for extension chords, and this
        // registry holds only ctrl-alt-e.
        native_fired.set(false);
        let free_keystroke = Keystroke::parse("ctrl-alt-y").expect("parse free chord");
        cx.dispatch_keystroke(any_window, free_keystroke);
        assert!(!native_fired.get());
        assert!(
            received.get(),
            "an unclaimed keystroke must still reach the focused view"
        );
    }

    /// The interceptor must survive installation. `intercept_keystrokes`
    /// returns a `Subscription` that unsubscribes on drop, so discarding it
    /// removed the interceptor on the same line that installed it and no
    /// extension chord ever fired.
    #[gpui::test]
    async fn global_shortcuts_dispatch_a_chord_no_native_binding_claims(
        cx: &mut gpui::TestAppContext,
    ) {
        let (command_sender, command_receiver) = std::sync::mpsc::channel::<HostCommand>();
        let controller = cx.new(|_| {
            let mut controller = ExtensionHostController::new();
            controller.command_sender = Some(command_sender);
            controller.commands.apply_report(&RegistryReport {
                extension_id: "ext-a".to_string(),
                commands: vec![RegisteredCommand {
                    id: "fire".to_string(),
                    label: "Fire".to_string(),
                }],
                keymaps: vec![RegisteredKeymap {
                    chord: "ctrl+alt+e".to_string(),
                    command: "fire".to_string(),
                }],
            });
            controller
        });
        cx.update(|cx| {
            cx.set_global(GlobalHostController(controller));
            install_global_shortcuts(cx);
        });

        let window_handle = cx.add_window(|_window, _cx| gpui::Empty);
        let any_window: gpui::AnyWindowHandle = window_handle.into();
        cx.dispatch_keystroke(
            any_window,
            Keystroke::parse("ctrl-alt-e").expect("parse extension chord"),
        );

        match command_receiver.try_recv() {
            Ok(HostCommand::ExecuteCommand { owner, command, .. }) => {
                assert_eq!(owner.as_deref(), Some("ext-a"));
                assert_eq!(command, "fire");
            }
            Ok(_) => panic!("the chord dispatched the wrong host command"),
            Err(_) => panic!(
                "the extension chord did not dispatch: the keystroke interceptor is not installed"
            ),
        }
    }


    /// §16.7: `execute_for_owner` runs exactly one runtime — the resolved
    /// owner — even when another extension registered the same bare command
    /// id; a missing, suspended, or non-registering owner runs nothing.
    #[test]
    fn execute_for_owner_runs_exactly_the_owning_extension() -> Result<()> {
        let root = temporary_extension_dir("registry-single-owner")?;
        let (owner, _) = load_io_probe(
            &root,
            "owner",
            100.0,
            r#"
            function activate(context) {
                context.renderState = 'idle';
                context.commands.register('dupe', function() { context.renderState = 'owner-ran'; });
                context.registerChromeView('main', {
                    render: function() { return { type: 'span', children: [context.renderState] }; }
                });
            }
            "#,
        )?;
        let (other, _) = load_io_probe(
            &root,
            "other",
            100.0,
            r#"
            function activate(context) {
                context.renderState = 'idle';
                context.commands.register('dupe', function() { context.renderState = 'other-ran'; });
                context.registerChromeView('main', {
                    render: function() { return { type: 'span', children: [context.renderState] }; }
                });
            }
            "#,
        )?;
        // Note: `other` first, so index 0 is the non-owner.
        let mut live_extensions = vec![other, owner];

        assert_eq!(
            execute_for_owner(&mut live_extensions, Some("ghost"), "dupe", ""),
            0,
            "an unknown owner must run nothing"
        );
        assert_eq!(
            execute_for_owner(&mut live_extensions, Some("owner"), "dupe", ""),
            1,
            "the resolved owner must run exactly once"
        );
        assert!(
            live_extensions[0]
                .live
                .render_now()?
                .is_some_and(|vdom| vdom.contains("idle")),
            "the non-owner must not have run"
        );
        assert!(
            live_extensions[1]
                .live
                .render_now()?
                .is_some_and(|vdom| vdom.contains("owner-ran")),
            "the owner must have run"
        );

        live_extensions[1].suspended = true;
        assert_eq!(
            execute_for_owner(&mut live_extensions, Some("owner"), "dupe", ""),
            0,
            "a suspended owner must not run, and nobody else may run instead"
        );
        assert_eq!(
            execute_for_owner(&mut live_extensions, Some("other"), "unregistered", ""),
            0,
            "a command the owner never registered must be dropped"
        );

        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
