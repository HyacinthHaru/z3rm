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
use futures::StreamExt as _;
use gpui::{AppContext as _, Global};
use parking_lot::Mutex;
use quickjs_runtime::{
    DiscoveredExtension, ExtensionCapabilities, ExtensionLimits, ExtensionRunResult,
    ExtensionRunner, ExtensionSide, FilesystemAccess, HostBridge, LiveExtension,
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
    merged.extend(local_nodes.iter().cloned());
    merged.extend(server_nodes.values().cloned());
    merged
}

fn apply_server_chrome_node(
    server_nodes: &mut BTreeMap<(String, String), VDomNode>,
    update: mux_protocol::ExtensionChromeUpdate,
) -> Result<()> {
    let key = (update.extension_id, update.view_id);
    if update.vdom_payload.is_empty() {
        server_nodes.remove(&key);
        return Ok(());
    }
    let json = std::str::from_utf8(&update.vdom_payload)
        .context("server extension VDOM payload is not UTF-8")?;
    let node = parse_vdom_json(json).context("server extension VDOM rejected")?;
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

/// §5.6 Canonical policy fingerprint: the exact serialized policy tuple the
/// consent decision covers — id, version, runtime side, capabilities and
/// resource limits — as canonical JSON (objects built from `BTreeMap`, so key
/// order is deterministic). Fingerprints are never hashed, so they cannot
/// collide: two manifests share a fingerprint iff their entire policy tuple
/// is byte-identical, and any change re-prompts.
fn consent_fingerprint(extension: &DiscoveredExtension) -> String {
    let manifest = &extension.manifest;
    let payload = serde_json::Value::Object(BTreeMap::from([
        ("id".into(), serde_json::Value::String(manifest.id.clone())),
        (
            "version".into(),
            serde_json::Value::String(manifest.version.clone()),
        ),
        ("side".into(), serde_json::Value::String(side_name(manifest.side).into())),
        ("capabilities".into(), capabilities_json(&manifest.capabilities)),
        ("limits".into(), limits_json(&manifest.limits)),
    ]).into_iter().collect());
    payload.to_string()
}

/// Canonical JSON for the resource-limit tuple of a manifest.
fn limits_json(limits: &ExtensionLimits) -> serde_json::Value {
    serde_json::Value::Object(BTreeMap::from([
        ("memory_limit_mb".into(), serde_json::json!(limits.memory_limit_mb)),
        ("cpu_budget_ms".into(), serde_json::json!(limits.cpu_budget_ms)),
        ("io_rate_limit".into(), serde_json::json!(limits.io_rate_limit)),
    ]).into_iter().collect())
}

fn side_name(side: ExtensionSide) -> &'static str {
    match side {
        ExtensionSide::Client => "client",
        ExtensionSide::Server => "server",
        ExtensionSide::Both => "both",
    }
}

fn capabilities_json(capabilities: &ExtensionCapabilities) -> serde_json::Value {
    serde_json::Value::Object(BTreeMap::from([
        ("terminal".into(), serde_json::json!(capabilities.terminal)),
        ("mux".into(), serde_json::json!(capabilities.mux)),
        ("workspace".into(), serde_json::json!(capabilities.workspace)),
        ("settings".into(), serde_json::json!(capabilities.settings)),
        ("network".into(), serde_json::json!(capabilities.network)),
        (
            "process_spawn".into(),
            serde_json::json!(capabilities.process_spawn),
        ),
        (
            "filesystem".into(),
            serde_json::json!(filesystem_name(capabilities.filesystem)),
        ),
    ]).into_iter().collect())
}

fn filesystem_name(access: FilesystemAccess) -> &'static str {
    match access {
        FilesystemAccess::None => "none",
        FilesystemAccess::Cwd => "cwd",
        FilesystemAccess::Home => "home",
    }
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
    /// Invalidates display-list clock views without running on the render thread.
    clock_task: Option<gpui::Task<()>>,
    /// Forwards mux notifications into the extensions as events.
    mux_task: Option<gpui::Task<()>>,
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
    /// §5.6 Controller-global prompt claim: at most one window may present
    /// the first-install prompt for the pending batch at a time.
    prompt_claimed: bool,
    /// Consent store location. Defaults to the config dir; tests redirect it
    /// so they never touch (or share) the real user's consent file.
    consent_file: PathBuf,
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
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its CPU budget and was suspended"
            );
            return;
        }
        if self.live.take_memory_violated() {
            self.suspended = true;
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its memory budget and was suspended"
            );
            return;
        }
        if self.live.take_io_violated() {
            self.suspended = true;
            tracing::error!(
                id = %self.live.id(),
                "extension exceeded its IO rate limit and was suspended"
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
fn suspension_notices(live_extensions: &[HostedExtension]) -> Vec<VDomNode> {
    live_extensions
        .iter()
        .filter(|hosted| hosted.suspended)
        .map(|hosted| VDomNode {
            element_type: "div".to_string(),
            props: BTreeMap::new(),
            style: BTreeMap::new(),
            children: vec![VDomChild::Text(format!(
                "{} suspended (resource limit)",
                hosted.live.id()
            ))],
        })
        .collect()
}

impl ExtensionHostController {
    pub fn new() -> Self {
        Self {
            command_sender: None,
            host_thread: None,
            chrome_task: None,
            clock_task: None,
            mux_task: None,
            status_bars: parking_lot::Mutex::new(Vec::new()),
            local_chrome: Vec::new(),
            server_chrome: BTreeMap::new(),
            pending_approvals: Vec::new(),
            pending_task: None,
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
        let (pending_sender, pending_receiver) =
            futures::channel::mpsc::unbounded::<Vec<PendingApproval>>();
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
                let mut installed_bridge: Option<Arc<dyn HostBridge>> = None;

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
                    let mut force_render = false;
                    match command {
                        HostCommand::InstallBridge(bridge) => {
                            installed_bridge = Some(bridge.clone());
                            for hosted in live_extensions.iter().filter(|hosted| !hosted.suspended) {
                                if let Err(error) = hosted.live.install_bridge(bridge.clone()) {
                                    tracing::warn!(id = %hosted.live.id(), %error, "installing mux bridge failed");
                                }
                            }
                        }
                        HostCommand::Emit { event, payload } => {
                            for hosted in live_extensions.iter().filter(|hosted| {
                                !hosted.suspended
                                    && hosted.live.capabilities().allows_host_event(&event)
                            }) {
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
                                // reach the newly activated extensions too.
                                if let Some(bridge) = &installed_bridge {
                                    for hosted in &live_extensions[existing..] {
                                        if let Err(error) = hosted.live.install_bridge(bridge.clone()) {
                                            tracing::warn!(id = %hosted.live.id(), %error, "installing mux bridge into approved extension failed");
                                        }
                                    }
                                }
                                force_render = true;
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
                self.start_pending_task(pending_receiver, cx);
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
        let nodes = merge_chrome_nodes(&self.local_chrome, &self.server_chrome);
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
                if this
                    .update(cx, |this, _| this.send(HostCommand::Render))
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
            if let Some(snapshot) = domain.last_attached_snapshot() {
                {
                    let mut bridge_state = state.lock();
                    for tab in &snapshot.tabs {
                        for pane in &tab.panes {
                            bridge_state
                                .pane_tabs
                                .insert(pane.id.clone(), tab.id.clone());
                            bridge_state
                                .pane_titles
                                .insert(pane.id.clone(), pane.title.clone());
                        }
                    }
                    if !snapshot.focused_pane_id.is_empty() {
                        bridge_state.focused_pane = Some(snapshot.focused_pane_id.clone());
                    }
                }

                if !snapshot.focused_pane_id.is_empty() {
                    let hydration = mux_protocol::Notification {
                        event: Some(mux_protocol::notification::Event::PaneFocused(
                            mux_protocol::PaneFocused {
                                pane_id: snapshot.focused_pane_id,
                            },
                        )),
                    };
                    for (event, payload) in notification_events(&hydration, &state) {
                        let payload = match serde_json::to_string(&payload) {
                            Ok(payload) => payload,
                            Err(error) => {
                                tracing::warn!(
                                    %event,
                                    %error,
                                    "serializing initial extension focus failed"
                                );
                                continue;
                            }
                        };
                        if let Err(error) =
                            this.read_with(cx, |this, _| this.emit_event(&event, &payload))
                        {
                            tracing::debug!(
                                %error,
                                "extension host dropped before initial focus hydration"
                            );
                            return;
                        }
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
    /// descriptors and native keybindings both route through here).
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
        cx.read(|_cx| {
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
        cx.read(|_cx| {
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
}
