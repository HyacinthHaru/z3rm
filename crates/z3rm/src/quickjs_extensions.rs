//! §5.2 QuickJS extension loader — scans extensions/ directory and loads
//! JS extensions via QuickJS on a dedicated OS thread.
//!
//! Per spec §5.2: "QuickJS runtime on a dedicated OS thread. The extension
//! host must not run on the GPUI render thread. Extensions communicate with
//! the UI via async channels; a hung extension freezes only itself."
//!
//! §5.4/§5.5 wiring: after loading, the host parses each extension's VDOM
//! JSON (returned from `context.render(vdom)` or `registerChromeView`) and
//! publishes the merged set into an app-global [`AcceptedVdom`] slot. The
//! workspace observer applies that pending VDOM to every
//! [`ExtensionStatusBar`] it creates, so the chrome actually displays
//! extension output end-to-end.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use extension_host::vdom_bridge::{self, VDomNode};
use gpui::{AppContext as _, Global};
use parking_lot::Mutex;
use quickjs_runtime::{ExtensionRunResult, ExtensionRunner};

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

/// §16.8 Extension runtime side declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionSide {
    Client,
    Server,
    Both,
}

impl ExtensionSide {
    fn from_str(s: &str) -> Self {
        match s {
            "server" => Self::Server,
            "both" => Self::Both,
            _ => Self::Client,
        }
    }
}

/// Extension metadata parsed from extension.toml.
struct ExtensionMeta {
    id: String,
    name: String,
    side: ExtensionSide,
    memory_limit_mb: usize,
    cpu_budget_ms: u64,
}

/// §5.2 Scan the extensions directory and load all client-side JS extensions.
///
/// Returns loaded extensions with their run results. Extensions that fail to
/// load are logged and skipped (a hung/broken extension must not crash the app).
pub fn load_client_extensions(extensions_dir: &Path) -> Vec<LoadedExtension> {
    let mut loaded = Vec::new();

    let entries = match std::fs::read_dir(extensions_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(error = %e, path = %extensions_dir.display(), "extensions directory not readable");
            return loaded;
        }
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        let toml_path = dir.join("extension.toml");
        let main_js_path = dir.join("main.js");

        if !toml_path.exists() || !main_js_path.exists() {
            continue;
        }

        match load_single_extension(&dir, &toml_path, &main_js_path) {
            Ok(ext) => {
                // §16.8: only load client-side or both-side extensions in the GUI
                if ext.side != ExtensionSide::Server {
                    if ext.result.result.is_ok() {
                        tracing::info!(id = %ext.id, "extension loaded successfully");
                    } else {
                        tracing::warn!(
                            id = %ext.id,
                            error = ?ext.result.result,
                            "extension loaded with errors"
                        );
                    }
                    loaded.push(ext);
                }
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "failed to load extension");
            }
        }
    }

    loaded
}

fn load_single_extension(
    _dir: &Path,
    toml_path: &Path,
    main_js_path: &Path,
) -> Result<LoadedExtension> {
    let meta = parse_extension_toml(toml_path)
        .with_context(|| format!("parsing {}", toml_path.display()))?;

    let source = std::fs::read_to_string(main_js_path)
        .with_context(|| format!("reading {}", main_js_path.display()))?;

    let runner = ExtensionRunner::new(meta.memory_limit_mb, meta.cpu_budget_ms);
    let result = runner.load_extension(&meta.id, &source, "activate");

    Ok(LoadedExtension {
        id: meta.id,
        name: meta.name,
        side: meta.side,
        result,
    })
}

/// Parse extension.toml for metadata. Minimal TOML parsing (no serde dependency
/// needed for the simple key-value format).
fn parse_extension_toml(path: &Path) -> Result<ExtensionMeta> {
    let content = std::fs::read_to_string(path)?;

    let mut name = String::new();
    let mut side = ExtensionSide::Client;
    let mut memory_limit_mb: usize = 64;
    let mut cpu_budget_ms: u64 = 50;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "name" => name = value.to_string(),
                "side" => side = ExtensionSide::from_str(value),
                "memory_limit_mb" => {
                    memory_limit_mb = value.parse().unwrap_or(64);
                }
                "cpu_budget_ms" => {
                    cpu_budget_ms = value.parse().unwrap_or(50);
                }
                _ => {}
            }
        }
    }

    let id = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());

    Ok(ExtensionMeta {
        id,
        name,
        side,
        memory_limit_mb,
        cpu_budget_ms,
    })
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
        match serde_json::from_str::<serde_json::Value>(json) {
            Ok(value) => match vdom_bridge::parse_vdom(&value) {
                Ok(node) => nodes.push(node),
                Err(e) => tracing::warn!(id = %ext.id, error = %e, "extension VDOM parse failed"),
            },
            Err(e) => tracing::warn!(id = %ext.id, error = %e, "extension VDOM JSON invalid"),
        }
    }
    nodes
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
/// Called from main.rs after GPUI app creation.
///
/// Loads extensions on a background thread (the host must never run on the
/// render thread — spec §5.2), then parses and publishes any VDOM back onto
/// the foreground thread into the app-global chrome state. Status bars created
/// afterward drain that state via [`take_pending_vdom`].
pub fn init_extensions(cx: &mut gpui::App) {
    init(cx);
    let extensions_dir = paths::extensions_dir().clone();

    cx.spawn(async move |cx| {
        let loaded = cx
            .background_spawn(async move { load_client_extensions(&extensions_dir) })
            .await;
        tracing::info!(count = loaded.len(), "QuickJS extensions loaded");

        let nodes = collect_status_bar_vdom(&loaded);
        let count = nodes.len();
        cx.update(|cx| publish_vdom(cx, nodes));
        if count > 0 {
            tracing::info!(vdom_nodes = count, "extension VDOM published to chrome");
        }
    })
    .detach();
}

// §5.2 Dedicated-thread extension host actor.
//
// Owns LiveExtensions on a single std::thread, satisfying the §5.2
// "dedicated OS thread" constraint. The host thread receives HostCommand
// messages via mpsc and responds via oneshot. All QuickJS ctx.with calls
// happen only on this thread.

enum HostCommand {
    Render {
        reply: std::sync::mpsc::Sender<Result<Vec<VDomNode>>>,
    },
    Emit {
        event: String,
        payload: String,
    },
    Shutdown,
}

pub struct ExtensionHostController {
    command_sender: Option<std::sync::mpsc::Sender<HostCommand>>,
    host_thread: Option<std::thread::JoinHandle<()>>,
    render_tick: Option<gpui::Task<()>>,
    status_bars: parking_lot::Mutex<Vec<gpui::WeakEntity<crate::extension_status_bar::ExtensionStatusBar>>>,
}

pub struct GlobalHostController(pub std::sync::Arc<ExtensionHostController>);
impl gpui::Global for GlobalHostController {}

impl ExtensionHostController {
    pub fn new() -> Self {
        Self {
            command_sender: None,
            host_thread: None,
            render_tick: None,
            status_bars: parking_lot::Mutex::new(Vec::new()),
        }
    }
    pub fn start(&mut self, extensions_dir: &Path, cx: &mut gpui::Context<Self>) {
        let (command_tx, command_rx) = std::sync::mpsc::channel::<HostCommand>();
        let dir = extensions_dir.to_path_buf();

        let handle = std::thread::Builder::new()
            .name("quickjs-ext-host".into())
            .spawn(move || {

                let runner = ExtensionRunner::with_defaults();
                let mut live_extensions: Vec<quickjs_runtime::LiveExtension> = Vec::new();

                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(_) => return,
                };
                for entry in entries.flatten() {
                    let ext_dir = entry.path();
                    if !ext_dir.is_dir() {
                        continue;
                    }
                    let main_js = ext_dir.join("main.js");
                    let toml = ext_dir.join("extension.toml");
                    if !main_js.exists() || !toml.exists() {
                        continue;
                    }
                    let source = match std::fs::read_to_string(&main_js) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let id = ext_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match runner.load_live(&id, &source, "activate") {
                        Ok(live) => {
                            tracing::info!(id = %id, "live extension loaded");
                            live_extensions.push(live);
                        }
                        Err(error) => {
                            tracing::warn!(id = %id, error = %error, "live extension load failed");
                        }
                    }
                }

                loop {
                    match command_rx.recv() {
                        Ok(HostCommand::Render { reply }) => {
                            let mut nodes = Vec::new();
                            for live in &live_extensions {
                                match live.render_now() {
                                    Ok(Some(json_str)) => {
                                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) {
                                            if let Ok(node) = vdom_bridge::parse_vdom(&value) {
                                                nodes.push(node);
                                            }
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        tracing::warn!(error = %error, "extension render failed");
                                    }
                                }
                            }
                            let _ = reply.send(Ok(nodes));
                        }
                        Ok(HostCommand::Emit { event, payload }) => {
                            for live in &live_extensions {
                                if let Err(error) = live.emit_event(&event, &payload) {
                                    tracing::warn!(event = %event, error = %error, "extension emit failed");
                                }
                            }
                        }
                        Ok(HostCommand::Shutdown) | Err(_) => break,
                    }
                }
            });

        if let Ok(handle) = handle {
            self.command_sender = Some(command_tx);
            self.host_thread = Some(handle);
            self.start_render_tick(cx);
        }
    }

    fn start_render_tick(&mut self, cx: &mut gpui::Context<Self>) {
        let sender = self.command_sender.clone();
        let timer_interval = std::time::Duration::from_secs(1);
        self.render_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(timer_interval).await;

                if let Some(sender) = &sender {
                    let (reply, response) = std::sync::mpsc::channel();
                    if sender.send(HostCommand::Render { reply }).is_err() {
                        break;
                    }
                    match response.recv() {
                        Ok(Ok(nodes)) => {
                            let _ = this.update(cx, |this, cx| {
                                for bar in this.status_bars.lock().iter() {
                                    if let Some(bar) = bar.upgrade() {
                                        bar.update(cx, |bar, cx| bar.set_vdom_nodes(nodes.clone(), cx));
                                    }
                                }
                            });
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(error = %error, "extension host render error");
                        }
                        Err(_) => break,
                    }
                }
            }
        }));
    }

    pub fn emit_event(&self, event: &str, payload: &str) {
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(HostCommand::Emit {
                event: event.to_string(),
                payload: payload.to_string(),
            });
        }
    }

    pub fn add_status_bar(&self, bar: gpui::WeakEntity<crate::extension_status_bar::ExtensionStatusBar>) {
        self.status_bars.lock().push(bar);
    }
}

impl Drop for ExtensionHostController {
    fn drop(&mut self) {
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(HostCommand::Shutdown);
        }
        if let Some(handle) = self.host_thread.take() {
            let _ = handle.join();
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
        assert_eq!(node.props.get("id").and_then(|v| v.as_str()), Some("status-bar"));
        assert_eq!(node.style.get("flexDirection").map(String::as_str), Some("row"));
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
        assert!(text.contains("hello"), "bridge vdom_to_text must surface the span text: {text}");
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
            (bar.vdom_node_count(), bar.vdom_nodes().first().map(|n| n.element_type.clone()))
        });
        assert_eq!(count, 1);
        assert_eq!(element_type.as_deref(), Some("div"));
    }
}
