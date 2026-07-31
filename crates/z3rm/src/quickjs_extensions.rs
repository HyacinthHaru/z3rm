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

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use extension_host::vdom_bridge::{self, VDomNode};
use futures::StreamExt as _;
use gpui::{AppContext as _, Global};
use parking_lot::Mutex;
use quickjs_runtime::{
    DiscoveredExtension, ExtensionRunResult, ExtensionRunner, ExtensionSide, HostBridge,
    LiveExtension,
};

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
            let result = runner.load_extension(&extension.manifest.id, &extension.source, "activate");
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
    let value: serde_json::Value =
        serde_json::from_str(json).context("extension VDOM JSON invalid")?;
    vdom_bridge::parse_vdom(&value).context("extension VDOM parse failed")
}

/// §5.5 Publish collected VDOM into the app-global chrome state so freshly
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
}

impl MuxHostBridge {
    fn new(domain: Arc<mux::MuxDomain>, state: Arc<Mutex<MuxBridgeState>>) -> Self {
        Self { domain, state }
    }

    fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        smol::block_on(smol::future::or(future, async {
            smol::Timer::after(MUX_CALL_TIMEOUT).await;
            Err(anyhow!("mux call timed out after {MUX_CALL_TIMEOUT:?}"))
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
                self.run(self.domain.attach_with_window(&id))?;
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
                bail!("mux.applyLayout is not supported: the mux protocol has no apply-layout request")
            }
            "settings.get" => {
                let key = required_string(args, 0, method)?;
                read_setting(&key)
            }
            "settings.set" => bail!(
                "settings.set is not supported yet: writing settings requires the GPUI settings store"
            ),
            other => bail!("unknown host method: {other}"),
        }
    }
}

/// §5.6 `settings` capability: dotted-path lookup into the user settings file.
fn read_setting(key: &str) -> Result<serde_json::Value> {
    let path = paths::settings_file();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Value::Null);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    let document: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut cursor = &document;
    for segment in key.split('.') {
        match cursor.get(segment) {
            Some(next) => cursor = next,
            None => return Ok(serde_json::Value::Null),
        }
    }
    Ok(cursor.clone())
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
            let layout = changed
                .layout
                .as_ref()
                .and_then(|tree| tree.root.as_ref())
                .map(layout_node_json)
                .unwrap_or(serde_json::Value::Null);
            vec![("session:layout".into(), layout)]
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

// ---------------------------------------------------------------------------
// §5.2 Dedicated-thread extension host actor.
//
// Owns LiveExtensions on a single std::thread, satisfying the §5.2
// "dedicated OS thread" constraint. The host thread receives HostCommand
// messages via mpsc and pushes rendered chrome back over an async channel.
// All QuickJS ctx.with calls happen only on this thread.
// ---------------------------------------------------------------------------

enum HostCommand {
    /// §5.4 Install the mux bridge once the daemon connection exists.
    InstallBridge(Arc<dyn HostBridge>),
    Emit {
        event: String,
        payload: String,
    },
    ExecuteCommand {
        command: String,
        arguments: String,
    },
    /// Force a full re-render regardless of invalidation state.
    Render,
    Shutdown,
}

pub struct ExtensionHostController {
    command_sender: Option<std::sync::mpsc::Sender<HostCommand>>,
    host_thread: Option<std::thread::JoinHandle<()>>,
    /// Applies chrome the host thread pushes; replaces the old 1Hz poll.
    chrome_task: Option<gpui::Task<()>>,
    /// Forwards mux notifications into the extensions as events.
    mux_task: Option<gpui::Task<()>>,
    status_bars:
        parking_lot::Mutex<Vec<gpui::WeakEntity<crate::extension_status_bar::ExtensionStatusBar>>>,
}

pub struct GlobalHostController(pub gpui::Entity<ExtensionHostController>);
impl gpui::Global for GlobalHostController {}

/// A live extension plus the host-side state that spec §5.6 requires: an
/// extension that blows its CPU budget is suspended rather than left to keep
/// burning the host thread on every subsequent event.
struct HostedExtension {
    live: LiveExtension,
    suspended: bool,
}

impl HostedExtension {
    /// §5.6 "Resource limits are enforced at runtime — exceeding them results
    /// in extension suspension." Suspension lasts for the process lifetime;
    /// the chrome falls back to the native GPUI baseline (§5.1).
    fn note_resource_violations(&mut self) {
        if self.live.take_cpu_interrupted() {
            self.suspended = true;
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its CPU budget and was suspended"
            );
        }
    }
}

/// Render every live extension and hand the VDOM JSON back, logging (never
/// discarding) whatever each extension reported as an internal error.
fn render_live_extensions(live_extensions: &mut [HostedExtension]) -> Vec<VDomNode> {
    let mut nodes = Vec::new();
    for hosted in live_extensions.iter_mut() {
        if hosted.suspended {
            continue;
        }
        let live_extension = &hosted.live;
        match live_extension.render_all_views() {
            Ok(views) => {
                for json in views {
                    match parse_vdom_json(&json) {
                        Ok(node) => nodes.push(node),
                        Err(error) => {
                            tracing::warn!(id = %live_extension.id(), %error, "extension VDOM rejected")
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(id = %live_extension.id(), %error, "extension render failed")
            }
        }
        match live_extension.take_errors() {
            Ok(errors) => {
                for error in errors {
                    tracing::warn!(id = %live_extension.id(), %error, "extension reported an error");
                }
            }
            Err(error) => {
                tracing::warn!(id = %live_extension.id(), %error, "draining extension errors failed")
            }
        }
        hosted.note_resource_violations();
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
            !hosted.suspended
                && hosted.live.needs_render().unwrap_or_else(|error| {
                    tracing::warn!(id = %hosted.live.id(), %error, "extension invalidation check failed");
                    false
                })
        });
    if !dirty {
        return true;
    }
    let nodes = render_live_extensions(live_extensions);
    match sender.unbounded_send(nodes) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, "extension chrome receiver dropped");
            false
        }
    }
}

impl ExtensionHostController {
    pub fn new() -> Self {
        Self {
            command_sender: None,
            host_thread: None,
            chrome_task: None,
            mux_task: None,
            status_bars: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn start(&mut self, extensions_dir: &Path, cx: &mut gpui::Context<Self>) {
        let (command_sender, command_receiver) = std::sync::mpsc::channel::<HostCommand>();
        let (chrome_sender, chrome_receiver) =
            futures::channel::mpsc::unbounded::<Vec<VDomNode>>();
        let roots = quickjs_runtime::extension_roots(extensions_dir);

        let host_thread = std::thread::Builder::new()
            .name("quickjs-ext-host".into())
            .spawn(move || {
                let discovered = quickjs_runtime::discover_client_extensions(&roots);
                if discovered.is_empty() {
                    tracing::warn!(?roots, "no client extensions found");
                }
                let mut live_extensions = activate_extensions(discovered);

                // First paint: extensions register their chrome during
                // activate, so publish it before waiting for any event.
                if !push_chrome_if_dirty(&mut live_extensions, &chrome_sender, true) {
                    return;
                }

                loop {
                    let command = match command_receiver.recv() {
                        Ok(command) => command,
                        Err(_) => break,
                    };
                    match command {
                        HostCommand::InstallBridge(bridge) => {
                            for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended) {
                                if let Err(error) = hosted.live.install_bridge(bridge.clone()) {
                                    tracing::warn!(id = %hosted.live.id(), %error, "installing mux bridge failed");
                                }
                            }
                        }
                        HostCommand::Emit { event, payload } => {
                            for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended) {
                                if let Err(error) = hosted.live.emit_event(&event, &payload) {
                                    tracing::warn!(id = %hosted.live.id(), %event, %error, "extension emit failed");
                                }
                            }
                        }
                        HostCommand::ExecuteCommand { command, arguments } => {
                            for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended) {
                                if let Err(error) = hosted.live.execute_command(&command, &arguments)
                                {
                                    tracing::warn!(id = %hosted.live.id(), %command, %error, "extension command failed");
                                }
                            }
                        }
                        HostCommand::Render => {
                            if !push_chrome_if_dirty(&mut live_extensions, &chrome_sender, true) {
                                break;
                            }
                            continue;
                        }
                        HostCommand::Shutdown => break,
                    }
                    // §5.6 A runaway handler is suspended before the next
                    // command, even if nothing requested a re-render.
                    for hosted in live_extensions.iter_mut() {
                        hosted.note_resource_violations();
                    }
                    if !push_chrome_if_dirty(&mut live_extensions, &chrome_sender, false) {
                        break;
                    }
                }
            });

        match host_thread {
            Ok(host_thread) => {
                self.command_sender = Some(command_sender.clone());
                self.host_thread = Some(host_thread);
                self.start_chrome_task(chrome_receiver, cx);
                self.start_mux_task(cx);
            }
            Err(error) => {
                tracing::error!(%error, "failed to start QuickJS extension host");
            }
        }
    }

    /// §5.5 Apply chrome pushed by the host thread. Event driven: the task
    /// parks on the channel instead of polling on a timer.
    fn start_chrome_task(
        &mut self,
        mut chrome_receiver: futures::channel::mpsc::UnboundedReceiver<Vec<VDomNode>>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.chrome_task = Some(cx.spawn(async move |this, cx| {
            while let Some(nodes) = chrome_receiver.next().await {
                let update = this.update(cx, |this, cx| {
                    this.status_bars.lock().retain(|status_bar| {
                        let Some(status_bar) = status_bar.upgrade() else {
                            return false;
                        };
                        status_bar
                            .update(cx, |status_bar, cx| status_bar.set_vdom_nodes(nodes.clone(), cx));
                        true
                    });
                    publish_vdom(cx, nodes);
                });
                if let Err(error) = update {
                    tracing::debug!(%error, "extension controller dropped while applying chrome");
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

            let state = Arc::new(Mutex::new(MuxBridgeState::default()));
            if let Some(session_id) = domain.last_attached_session_id()
                && let Ok(sessions) = domain.list_sessions().await
                && let Some(session) = sessions.iter().find(|session| session.id == session_id)
            {
                state.lock().session_name = Some(session.name.clone());
            }

            let bridge: Arc<dyn HostBridge> =
                Arc::new(MuxHostBridge::new(domain.clone(), state.clone()));
            if let Err(error) = this.read_with(cx, |this, _| {
                this.send(HostCommand::InstallBridge(bridge));
            }) {
                tracing::debug!(%error, "extension controller dropped before the mux bridge was installed");
                return;
            }

            let notifications = domain.subscribe();
            while let Ok(notification) = notifications.recv().await {
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
    /// descriptors and native keybindings both route through here).
    // The VDOM bridge that turns `{ command, args }` props into clicks lives in
    // `extension_host::vdom_bridge`; until it calls this, the entry point has no
    // in-tree caller.
    #[allow(dead_code)]
    pub fn execute_command(&self, command: &str, arguments_json: &str) {
        self.send(HostCommand::ExecuteCommand {
            command: command.to_string(),
            arguments: arguments_json.to_string(),
        });
    }

    /// Force a chrome re-render (used after a workspace attaches a new status
    /// bar so it inherits the current chrome).
    pub fn request_render(&self) {
        self.send(HostCommand::Render);
    }

    fn send(&self, command: HostCommand) {
        if let Some(sender) = &self.command_sender
            && let Err(error) = sender.send(command)
        {
            tracing::warn!(%error, "failed to send command to QuickJS host");
        }
    }

    pub fn add_status_bar(
        &self,
        status_bar: gpui::WeakEntity<crate::extension_status_bar::ExtensionStatusBar>,
    ) {
        self.status_bars.lock().push(status_bar);
        self.request_render();
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

        assert!(discovered.is_empty(), "server-side extensions must be skipped");
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
            assert!(ids.contains(&expected), "{expected} did not activate: {ids:?}");
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

        let mut live_extensions = activate_extensions(
            quickjs_runtime::discover_client_extensions(std::slice::from_ref(&root)),
        );
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
}
