// §16.8 Server-side QuickJS extension host.
//
// mux_server is the authority for sessions and panes; this module loads
// extensions that declare `runtime.side = "server"` (or `"both"`) and runs
// them inside the daemon under the same quickjs_runtime resource limits the
// GUI client applies (CPU fuel, memory cap, IO token bucket).
//
// Design (§5.2): a manager thread (`z3rm-ext-host`) plus one dedicated OS
// thread per extension (`z3rm-ext-<id>`). Every `LiveExtension` is created
// and retained on its own worker thread, and all QuickJS `ctx.with`
// re-entry — activation, rendering, events, commands — happens there only.
// Connection handlers talk to the manager through a command channel and
// await a oneshot reply; the manager routes emit/render/execute/list work to
// workers with bounded waits, so a hung extension blocks nobody: not a
// healthy peer, not the manager, not shutdown. Workers that exceed a
// resource limit (CPU fuel, memory cap, IO token bucket) or fail to answer
// in time are suspended: their chrome is tombstoned (empty payload) and a
// daemon-authored status-bar VDOM notice naming the extension and the
// reason is published. Chrome views rendered by server extensions are
// fanned out to attached clients as `ExtensionChromeUpdate` notifications
// (§16), using each session's existing lifecycle subscriber set — the
// daemon never invents its own client list.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration};
use web_time::Instant;


use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use http_client::HttpClient as _;
use reqwest_client::ReqwestClient;
use mux_protocol::{ExtensionChromeUpdate, Notification};
use quickjs_runtime::{
    ExtensionManifest, ExtensionRunner, FilesystemAccess, HostBridge, LiveExtension,
    discover_server_extensions, extension_roots, parse_manifest_str,
};

type Sessions = Arc<parking_lot::RwLock<Vec<crate::session::Session>>>;

/// Compressed install payload cap: extensions are a few KB of JS; a 16 MiB
/// tar.gz already dwarfs anything legitimate and bounds decode work.
const MAX_INSTALL_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
/// Uncompressed cap, guarding against decompression bombs on the host thread.
const MAX_EXTRACTED_BYTES: u64 = 64 * 1024 * 1024;
/// Max extension id / install-name length; it becomes a directory component.
const MAX_EXTENSION_ID_LEN: usize = 128;
/// §16.9 Cap on a server chrome action request: command + arguments are a
/// few hundred bytes of JSON in practice; the cap bounds host-thread parse
/// work from a misbehaving client.
const MAX_CHROME_ACTION_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// §5.6 Server-extension approval ledger
// ---------------------------------------------------------------------------
//
// Server extensions (side `server` or `both`) only activate after an
// explicit approval. The ledger is a JSON file next to the user extensions
// directory, mirroring the GUI client's consent store format exactly:
// `[{"id", "policy_fingerprint", "state": "approved"|"denied"}]`, keyed by
// id + the exact policy fingerprint (`ExtensionManifest::policy_fingerprint`)
// the decision was made against — an update that changes capabilities,
// limits, side or version re-requires approval. Loading fails closed: an
// absent or corrupt file means nothing is approved, and records without the
// explicit state/fingerprint the current format requires are skipped.

/// §5.6 The daemon's decision for one exact policy fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerConsentState {
    Approved,
    Denied,
}

/// §5.6 One persisted daemon-side approval decision. Serializes as
/// `{"id", "policy_fingerprint", "state"}` with an explicit
/// `"approved"/"denied"` state — no sentinel values.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConsentRecord {
    pub id: String,
    pub policy_fingerprint: String,
    pub state: ServerConsentState,
}

impl ServerConsentRecord {
    fn to_json(&self) -> serde_json::Value {
        let state = match self.state {
            ServerConsentState::Approved => "approved",
            ServerConsentState::Denied => "denied",
        };
        serde_json::json!({
            "id": self.id,
            "policy_fingerprint": self.policy_fingerprint,
            "state": state,
        })
    }

    /// Strict record parse: legacy or malformed records (missing fields,
    /// unknown state, wrong types) fail closed into `None` and are skipped —
    /// they never grant approval.
    fn from_json(value: &serde_json::Value) -> Option<ServerConsentRecord> {
        let id = value.get("id")?.as_str()?.to_string();
        let policy_fingerprint = value.get("policy_fingerprint")?.as_str()?.to_string();
        let state = match value.get("state")?.as_str()? {
            "approved" => ServerConsentState::Approved,
            "denied" => ServerConsentState::Denied,
            _ => return None,
        };
        Some(ServerConsentRecord {
            id,
            policy_fingerprint,
            state,
        })
    }
}

/// §5.6 Ledger path: a JSON file next to the user extensions directory.
fn server_consent_path(user_extensions_dir: &Path) -> PathBuf {
    user_extensions_dir.join("extension-consent.json")
}

/// Load approval records as `id → record`. An absent file is an empty
/// ledger; an unreadable or corrupt file is treated as empty (fail closed:
/// nothing unapproved runs, nothing crashes) with a log.
fn load_server_consent(path: &Path) -> BTreeMap<String, ServerConsentRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "server extension consent file unreadable; treating as empty"
            );
            return BTreeMap::new();
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "server extension consent file corrupt; treating as empty"
            );
            return BTreeMap::new();
        }
    };
    let Some(records) = value.as_array() else {
        tracing::warn!(
            path = %path.display(),
            "server extension consent file is not an array; treating as empty"
        );
        return BTreeMap::new();
    };
    let mut consented = BTreeMap::new();
    for record in records {
        let Some(record) = ServerConsentRecord::from_json(record) else {
            tracing::warn!(
                path = %path.display(),
                %record,
                "skipping malformed server extension consent record"
            );
            continue;
        };
        consented.insert(record.id.clone(), record);
    }
    consented
}

/// Load the raw ledger record array (each element untouched), used by
/// writers that must preserve unknown fields (proof records with nonce/TTL
/// extras). An absent or corrupt file yields an empty vec.
fn load_server_consent_raw(path: &Path) -> Vec<serde_json::Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "server extension consent file unreadable; treating as empty"
            );
            return Vec::new();
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Array(records)) => records,
        Ok(_) => {
            tracing::warn!(
                path = %path.display(),
                "server extension consent file is not an array; treating as empty"
            );
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "server extension consent file corrupt; treating as empty"
            );
            Vec::new()
        }
    }
}

/// Persist raw ledger records as a JSON array. Writes are temp-file + rename
/// so a crash mid-write cannot corrupt the ledger. Errors propagate: callers
/// must not report success when the ledger could not be written.
fn save_server_consent_raw(path: &Path, records: &[serde_json::Value]) -> Result<()> {
    let serialized = serde_json::to_string_pretty(records)
        .context("serializing server extension consent records failed")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating consent directory {}", parent.display()))?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized)
        .with_context(|| format!("writing server extension consent file {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("committing server extension consent file {}", path.display()))?;
    Ok(())
}

/// §5.6 Whether the ledger approves `manifest`: an `Approved` record whose
/// fingerprint matches the manifest exactly. Anything else — no record, a
/// Denied record, or a record for a different fingerprint (manifest changed
/// since the decision) — is not approved.
fn ledger_approves(
    records: &BTreeMap<String, ServerConsentRecord>,
    manifest: &quickjs_runtime::ExtensionManifest,
) -> bool {
    match records.get(&manifest.id) {
        Some(record) => {
            record.state == ServerConsentState::Approved
                && record.policy_fingerprint == manifest.policy_fingerprint()
        }
        None => false,
    }
}

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
    /// §5.6 扩展声明的文件系统范围。每个扩展装载时按自己的 manifest 声明构造
    /// 专属桥 (见 [`worker_thread_main`] 的 `WorkerSetup::Discovered` 分支 /
    /// [`install_on_host_thread`]), 因此这里
    /// 是一个确定的范围, 而不是多个扩展范围的并集。
    filesystem: FilesystemAccess,
    /// §5.6 Home 约束根 (默认 `dirs::home_dir()`)。服务器可注入用户主目录
    /// (例如按连接用户解析), 测试注入临时目录。
    home: Option<PathBuf>,
    /// §5.6 Cwd 约束根 (默认宿主进程当前工作目录, 即 `workspace.getPath` 报告的
    /// 权威根)。测试注入临时目录以在不依赖进程环境的情况下验证 cwd 范围。
    cwd: Option<PathBuf>,
}

impl ServerHostBridge {
    /// 按扩展声明的文件系统范围构造桥; 约束根取宿主默认值 (home = 进程主目录,
    /// cwd = 进程当前工作目录)。
    pub fn new(sessions: Sessions, filesystem: FilesystemAccess) -> Self {
        Self {
            sessions,
            filesystem,
            home: None,
            cwd: None,
        }
    }

    /// §5.6 覆盖 Home 约束根: 注入后 `filesystem.*` 的 Home 范围操作被限制在该
    /// 目录内 (不再读取进程环境的主目录)。仅供测试与需要显式用户主目录的宿主
    /// 使用。
    pub fn with_home(sessions: Sessions, home: PathBuf, filesystem: FilesystemAccess) -> Self {
        Self {
            sessions,
            filesystem,
            home: Some(home),
            cwd: None,
        }
    }

    /// §5.6 同时覆盖 Home 与 Cwd 约束根 (仅供测试): 两个范围都能在临时目录内
    /// 验证, 不依赖进程环境。
    pub fn with_roots(
        sessions: Sessions,
        home: PathBuf,
        cwd: PathBuf,
        filesystem: FilesystemAccess,
    ) -> Self {
        Self {
            sessions,
            filesystem,
            home: Some(home),
            cwd: Some(cwd),
        }
    }

    /// 解析 Home 约束根。
    fn home_dir(&self) -> Result<PathBuf> {
        match &self.home {
            Some(home) => Ok(home.clone()),
            None => dirs::home_dir().context("host home directory unavailable"),
        }
    }

    /// 解析 Cwd 约束根: 注入的根, 否则宿主进程的权威当前工作目录 (与
    /// `workspace.getPath` 报告的是同一个根)。
    fn cwd_dir(&self) -> Result<PathBuf> {
        match &self.cwd {
            Some(cwd) => Ok(cwd.clone()),
            None => std::env::current_dir().context("host working directory unavailable"),
        }
    }

    /// §5.6 把扩展请求的路径约束到声明范围内 (见 [`quickjs_runtime::confine_to_root`])。
    ///
    /// 范围语义: `Cwd` 只允许权威工作区/当前工作根内的路径, `Home` 只允许主
    /// 目录内的路径——`cwd` 声明不能因此获得主目录访问权, home 也不能逃出主
    /// 目录。`None` (未声明) fail closed。
    fn confine(&self, path: &str) -> Result<PathBuf> {
        let root = match self.filesystem {
            FilesystemAccess::None => {
                bail!("filesystem access is not granted to this extension");
            }
            FilesystemAccess::Cwd => self.cwd_dir()?,
            FilesystemAccess::Home => self.home_dir()?,
        };
        quickjs_runtime::confine_to_root(&root, path)
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

fn run_with_timeout<T>(
    future: impl Future<Output = Result<T>>,
    timeout: Duration,
    operation: &'static str,
) -> Result<T> {
    smol::block_on(smol::future::or(future, async move {
        smol::Timer::after(timeout).await;
        bail!("{operation} timed out after {timeout:?}");
    }))
}

fn read_server_setting(key: &str) -> Result<serde_json::Value> {
    quickjs_runtime::validate_settings_key(key)?;
    let path = paths::settings_file();
    let text = match std::fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() as u64 > quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES {
                bail!(
                    "settings document exceeds {} bytes",
                    quickjs_runtime::MAX_EXTENSION_SETTINGS_DOCUMENT_BYTES
                );
            }
            String::from_utf8(bytes).context("settings document is not UTF-8")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::Value::Null),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let document: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    let mut cursor = &document;
    for segment in key.split('.') {
        cursor = match cursor.get(segment) {
            Some(next) => next,
            None => return Ok(serde_json::Value::Null),
        };
    }
    Ok(cursor.clone())
}

fn write_server_setting(key: &str, value: serde_json::Value) -> Result<serde_json::Value> {
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
            "settings.get" => {
                let key = required_string(args, 0, method)?;
                read_server_setting(&key)
            }
            "settings.set" => {
                let key = required_string(args, 0, method)?;
                let value = args.get(1).cloned().unwrap_or(serde_json::Value::Null);
                write_server_setting(&key, value)
            }
            "workspace.getPath" => {
                // 只读、无参: 返回宿主进程的工作目录。失败 (理论上的无 cwd)
                // 时报错而不是假装成功。
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
                let response = run_with_timeout(
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
                let response_body = run_with_timeout(
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

// ---------------------------------------------------------------------------
// Host manager + one worker thread per extension
// ---------------------------------------------------------------------------

/// Commands the `ServerExtensionHost` front door sends to the manager
/// thread. Worker threads also report through this channel (they hold a
/// [`mpsc::Sender`] clone), so the manager processes reports like any other
/// command — no extra thread or polling.
enum HostCommand {
    /// Extract + load (or replace) an extension on a fresh worker, answering
    /// on `reply`.
    Install {
        manifest: ExtensionManifest,
        archive: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    /// §3.4 Deliver a server event to every non-suspended extension.
    Emit {
        event: String,
        payload: String,
    },
    /// Force a full chrome re-render and push.
    Render,
    /// §16.9 Execute a command on ONE named server extension, answering on
    /// `reply`. Fail-closed validation happens on the manager thread: unknown
    /// or suspended extensions and unpublished view ids are rejected before
    /// any JS runs.
    ExecuteExtension {
        extension_id: String,
        view_id: String,
        command: String,
        arguments: String,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    ListIds(tokio::sync::oneshot::Sender<Vec<String>>),
    /// One worker → manager report.
    WorkerEvent(WorkerEvent),
    Shutdown,
}

/// §5.6 Why an extension was suspended. The reason string is surfaced
/// verbatim in the status-bar notice, so keep it user-meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuspensionReason {
    CpuBudget,
    MemoryBudget,
    IoRateLimit,
    /// The worker did not answer a bounded host wait (render/execute).
    Unresponsive,
    /// The worker thread exited without answering.
    Crashed,
}

impl SuspensionReason {
    fn as_str(self) -> &'static str {
        match self {
            SuspensionReason::CpuBudget => "cpu budget exceeded",
            SuspensionReason::MemoryBudget => "memory budget exceeded",
            SuspensionReason::IoRateLimit => "io rate limit exceeded",
            SuspensionReason::Unresponsive => "did not respond to the host in time",
            SuspensionReason::Crashed => "worker exited unexpectedly",
        }
    }
}

/// One worker → manager report.
enum WorkerEvent {
    /// The worker detected a resource-limit violation and self-suspended.
    /// `instance` distinguishes workers of the same extension id across
    /// reinstalls, so a late report from a replaced worker is ignored.
    Suspended {
        extension_id: String,
        instance: u64,
        reason: SuspensionReason,
    },
    /// An event dispatch invalidated chrome; the manager should run a render
    /// round.
    RenderRequested {
        extension_id: String,
        instance: u64,
    },
}

/// Commands the manager sends to ONE extension worker. A worker owns its
/// `LiveExtension` exclusively, so every variant runs JS on that worker's
/// thread and nowhere else (§5.2).
enum WorkerCommand {
    /// Deliver a host event to the extension's subscribers.
    Emit {
        event: String,
        payload: String,
    },
    /// Re-render (force) or render only when invalidated; answers with the
    /// fresh VDOM JSON list (possibly empty = nothing changed).
    Render {
        force: bool,
        reply: mpsc::Sender<Vec<String>>,
    },
    /// Run a command through the extension's own command registry.
    Execute {
        command: String,
        arguments: String,
        reply: mpsc::Sender<Result<()>>,
    },
    Shutdown,
}

/// How a worker is brought up: what to load before it enters its serve loop.
enum WorkerSetup {
    /// §16.6 Fresh install: extract, validate and activate on the worker
    /// thread; the extracted directory is swapped into place atomically.
    Install {
        manifest: ExtensionManifest,
        archive: Vec<u8>,
        user_extensions_dir: PathBuf,
        sessions: Sessions,
        consent: BTreeMap<String, ServerConsentRecord>,
    },
    /// §5.5 Startup discovery: activate an already-installed extension.
    Discovered {
        manifest: quickjs_runtime::ExtensionManifest,
        source: String,
        sessions: Sessions,
    },
}

/// Outcome of a worker's load step, answered on `ready_tx` before the worker
/// enters its serve loop.
enum WorkerReady {
    Running,
    /// Loaded, but already crossed a resource limit during activation; the
    /// manager must never route work to it.
    Suspended(SuspensionReason),
    /// Load failed (activation error, bad archive, ...).
    Failed(String),
}

/// A live extension plus §5.6 suspension state, owned by its worker thread.
/// An extension that blows its CPU, memory or IO budget is suspended for the
/// daemon's lifetime instead of keep burning the worker thread.
struct HostedExtension {
    live: LiveExtension,
    suspended: bool,
}

impl HostedExtension {
    /// Drain error logs and the §5.6 resource-violation flags. `Some(reason)`
    /// means the extension crossed a limit and must be suspended.
    fn detect_violation(&mut self) -> Option<SuspensionReason> {
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
            return Some(SuspensionReason::CpuBudget);
        }
        if self.live.take_memory_violated() {
            return Some(SuspensionReason::MemoryBudget);
        }
        // §5.6 IO quota rejection is flagged in Rust at the token bucket, so
        // it survives an extension's JS try/catch; the flag is the only
        // reliable signal that the extension exceeded its `io_rate_limit`.
        if self.live.take_io_violated() {
            return Some(SuspensionReason::IoRateLimit);
        }
        None
    }

    /// Re-render every registered view (or only dirty ones when `force` is
    /// false) and return the fresh VDOM JSON list. A render failure yields
    /// no views.
    fn render(&mut self, force: bool) -> Vec<String> {
        if !force && !self.live.needs_render().unwrap_or_else(|error| {
            tracing::warn!(id = %self.live.id(), %error, "invalidation check failed");
            false
        }) {
            return Vec::new();
        }
        match self.live.render_all_views() {
            Ok(rendered) => rendered,
            Err(error) => {
                tracing::warn!(id = %self.live.id(), %error, "server extension render failed");
                Vec::new()
            }
        }
    }

    /// §5.6 Check for violations after a JS-touching operation. When the
    /// extension crossed a limit: mark it suspended, report the reason to
    /// the manager (which tombstones its chrome and publishes the notice)
    /// and return `true` so the worker winds down. Otherwise, when the
    /// operation invalidated chrome, ask the manager for a render round.
    fn finish_operation(
        &mut self,
        events_tx: &mpsc::Sender<HostCommand>,
        instance: u64,
    ) -> bool {
        if let Some(reason) = self.detect_violation() {
            self.suspended = true;
            tracing::error!(
                id = %self.live.id(),
                reason = reason.as_str(),
                "server extension suspended"
            );
            let _ = events_tx.send(HostCommand::WorkerEvent(WorkerEvent::Suspended {
                extension_id: self.live.id().to_string(),
                instance,
                reason,
            }));
            return true;
        }
        let dirty = self.live.needs_render().unwrap_or_else(|error| {
            tracing::warn!(id = %self.live.id(), %error, "invalidation check failed");
            false
        });
        if dirty {
            let _ = events_tx.send(HostCommand::WorkerEvent(WorkerEvent::RenderRequested {
                extension_id: self.live.id().to_string(),
                instance,
            }));
        }
        false
    }
}

/// The manager thread's per-extension bookkeeping. `join` is handed back to
/// the host owner at shutdown so it can reap workers without ever joining a
/// hung one indefinitely.
struct WorkerHandle {
    command_tx: mpsc::SyncSender<WorkerCommand>,
    join: std::thread::JoinHandle<()>,
    /// Unique per spawn; guards against late reports from a replaced worker.
    instance: u64,
    suspended: bool,
}

pub struct ServerExtensionHost {
    command_tx: mpsc::Sender<HostCommand>,
    thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The manager hands its worker join handles back here at shutdown so
    /// `Drop` can reap them without waiting on a hung worker itself. The
    /// receiver is !Sync, so it lives behind a mutex to keep the host
    /// sharable across Tokio tasks (weak references from lifecycle hooks).
    shutdown_rx: parking_lot::Mutex<mpsc::Receiver<Vec<std::thread::JoinHandle<()>>>>,
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
    /// Spawn the manager thread (which spawns one worker thread per
    /// discovered or installed extension), discover already-installed
    /// server extensions (§5.5 / §16.8), and start the chrome fan-out task.
    ///
    /// Startup failures inside the thread are logged, never fatal (§15.7):
    /// a broken extension directory must not keep the daemon from booting.
    pub fn start(sessions: Sessions, user_extensions_dir: PathBuf) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::channel::<HostCommand>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<Vec<std::thread::JoinHandle<()>>>();
        let (chrome_tx, mut chrome_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<ChromeView>>();
        let thread_dir = user_extensions_dir.clone();
        let thread_sessions = sessions.clone();
        // Worker threads report through this same channel.
        let thread_command_tx = command_tx.clone();
        let thread = match std::thread::Builder::new()
            .name("z3rm-ext-host".into())
            .spawn(move || {
                host_thread_main(
                    &thread_dir,
                    thread_sessions,
                    command_rx,
                    thread_command_tx,
                    chrome_tx,
                    shutdown_tx,
                );
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
            shutdown_rx: parking_lot::Mutex::new(shutdown_rx),
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
            Event::ShellIntegrationChanged(event) => {
                ("shell:integration", serde_json::json!({"cwd": event.cwd}))
            }
            Event::PaneDirty(event) => ("pane:dirty", serde_json::json!({"paneId": event.pane_id})),
            Event::PaneBell(event) => ("pane:bell", serde_json::json!({"paneId": event.pane_id})),
            Event::PaneOutput(event) => (
                "pane:output",
                serde_json::json!({
                    "paneId": event.pane_id,
                    "data": event.data,
                    "outputSequence": event.output_sequence,
                }),
            ),
            Event::ClipboardChanged(_) => ("clipboard", serde_json::Value::Null),
            Event::SyncScrollback(_)
            | Event::ExtensionChrome(_)
            | Event::PaneMedia(_)
            | Event::PaneAction(_) => return,
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
        if manifest.id != name {
            bail!(
                "extension manifest id `{}` does not match request name `{name}`",
                manifest.id
            );
        }
        // §5.6 Approval gate: an install is refused before any extraction
        // when the shipped manifest is not approved for its exact policy
        // fingerprint. The host thread re-checks the on-disk copy.
        if !self.is_approved(&manifest) {
            bail!(
                "extension `{}` is not approved for server activation (policy fingerprint `{}`); approve it before installing",
                manifest.id,
                manifest.policy_fingerprint()
            );
        }

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(HostCommand::Install {
                manifest,
                archive: request.source.clone(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("extension host thread is gone"))?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => bail!("extension host thread exited before answering install"),
        }
    }

    /// §5.6 Whether the current ledger approves `manifest`: an `Approved`
    /// record for the exact policy fingerprint. Anything else is not
    /// approved (fail closed). Reads the ledger from disk, so it reflects
    /// the latest decision.
    pub fn is_approved(&self, manifest: &quickjs_runtime::ExtensionManifest) -> bool {
        let records = load_server_consent(&server_consent_path(&self.user_extensions_dir));
        ledger_approves(&records, manifest)
    }

    /// §5.6 Record (or revoke) approval for `manifest`, keyed by its exact
    /// policy fingerprint. Persists before returning; on error the ledger is
    /// unchanged and the error names the manifest. A later activation only
    /// happens on daemon restart or a fresh install of the same manifest.
    ///
    /// The write is raw-record preserving: ledger records written by other
    /// paths (e.g. install-proof records carrying nonce/TTL fields) keep
    /// their extra fields; only the record for `manifest.id` is replaced.
    pub fn set_consent(
        &self,
        manifest: &quickjs_runtime::ExtensionManifest,
        approved: bool,
    ) -> Result<()> {
        let path = server_consent_path(&self.user_extensions_dir);
        let records = load_server_consent_raw(&path);
        let record = ServerConsentRecord {
            id: manifest.id.clone(),
            policy_fingerprint: manifest.policy_fingerprint(),
            state: if approved {
                ServerConsentState::Approved
            } else {
                ServerConsentState::Denied
            },
        };
        let mut records: Vec<serde_json::Value> = records
            .into_iter()
            .filter(|existing| existing.get("id").and_then(serde_json::Value::as_str) != Some(&manifest.id))
            .collect();
        records.push(record.to_json());
        save_server_consent_raw(&path, &records)
            .with_context(|| format!("recording approval for extension `{}`", manifest.id))
    }

    /// §16.9 Execute a chrome action (an `onClick`/`onChange` from chrome
    /// the daemon published) on the authoritative host thread. The request
    /// must name an extension id + view id the daemon actually published;
    /// anything else fails closed with a contextual error before any JS
    /// runs.
    pub async fn execute_chrome_action(
        &self,
        request: &mux_protocol::ExtensionChromeActionRequest,
    ) -> Result<()> {
        let combined_len = request
            .command
            .len()
            .saturating_add(request.arguments.len());
        if combined_len > MAX_CHROME_ACTION_BYTES {
            bail!(
                "chrome action for `{}` is {} bytes; limit is {MAX_CHROME_ACTION_BYTES}",
                request.extension_id,
                combined_len
            );
        }
        validate_extension_id(&request.extension_id)?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(HostCommand::ExecuteExtension {
                extension_id: request.extension_id.clone(),
                view_id: request.view_id.clone(),
                command: request.command.clone(),
                arguments: request.arguments.clone(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("extension host thread is gone"))?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => bail!("extension host thread exited before answering chrome action"),
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
        if self
            .command_tx
            .send(HostCommand::ListIds(reply_tx))
            .is_err()
        {
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
        // The manager exits within its bounded waits and hands its workers
        // back here; anything that does not arrive within the grace is
        // detached rather than waited on.
        let workers = self
            .shutdown_rx
            .lock()
            .recv_timeout(MANAGER_SHUTDOWN_GRACE)
            .unwrap_or_default();
        if let Some(handle) = self.thread.lock().take() {
            join_bounded(handle, MANAGER_SHUTDOWN_GRACE);
        }
        for worker in workers {
            join_bounded(worker, WORKER_JOIN_GRACE);
        }
    }
}

/// Join a thread if it finishes within `grace`; otherwise detach it. A hung
/// extension worker must never hold up host shutdown, so we never wait
/// indefinitely — the process reaps detached threads at exit.
fn join_bounded(handle: std::thread::JoinHandle<()>, grace: Duration) {
    let deadline = Instant::now() + grace;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if handle.is_finished() && handle.join().is_err() {
        tracing::warn!("extension thread panicked during shutdown");
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

/// Reserved view id under which the daemon publishes a suspension notice for
/// an extension. Distinct from any view an extension renders, so the notice
/// cannot collide with (or be tombstoned as) extension chrome.
const SUSPENDED_NOTICE_VIEW_ID: &str = "z3rm.suspended";

/// §16.8 Bounds: how long the manager waits (per round) for a worker to
/// answer render or execute work before marking it unresponsive.
const WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Install: extraction + activation can legitimately take a moment.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Startup discovery: a single shared deadline across all discovered
/// extensions so one stuck activation cannot hold up the daemon's boot.
const STARTUP_LOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `Drop` waits for the manager thread to hand back its workers.
const MANAGER_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long `Drop` waits per worker before detaching it.
const WORKER_JOIN_GRACE: Duration = Duration::from_millis(500);
/// Per-worker command queue depth. Emits are `try_send` into this queue, so
/// a stuck extension drops events beyond the bound instead of accumulating
/// unbounded work.
const WORKER_QUEUE_CAPACITY: usize = 8;

/// The manager thread: owns every worker handle plus the chrome bookkeeping
/// (published views for tombstones and command ownership, suspension notices).
/// It never runs JS itself — every QuickJS call happens on a worker thread —
/// and every wait it performs on a worker is bounded by a deadline.
struct ExtensionManager {
    workers: BTreeMap<String, WorkerHandle>,
    /// (extension_id, view_id) pairs currently on the wire, keyed for
    /// tombstones and exact command ownership.
    published_views: BTreeSet<(String, String)>,
    /// Suspension notices currently on the wire.
    notices: BTreeSet<(String, String)>,
    chrome_tx: tokio::sync::mpsc::UnboundedSender<Vec<ChromeView>>,
    /// Worker reports ride the manager's own command channel.
    events_tx: mpsc::Sender<HostCommand>,
    next_instance: u64,
}

impl ExtensionManager {
    fn new(
        chrome_tx: tokio::sync::mpsc::UnboundedSender<Vec<ChromeView>>,
        events_tx: mpsc::Sender<HostCommand>,
    ) -> Self {
        Self {
            workers: BTreeMap::new(),
            published_views: BTreeSet::new(),
            notices: BTreeSet::new(),
            chrome_tx,
            events_tx,
            next_instance: 0,
        }
    }

    /// Spawn a worker thread for `extension_id` and wait (bounded by
    /// `deadline`) for it to load. On success the worker is registered,
    /// atomically replacing any previous instance of the same id. `Ok`
    /// means the worker is registered (possibly already suspended from a
    /// violation during activation).
    fn spawn_worker(
        &mut self,
        extension_id: String,
        setup: WorkerSetup,
        deadline: &Instant,
    ) -> Result<(), String> {
        let instance = self.next_instance;
        self.next_instance += 1;
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(WORKER_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::channel::<WorkerReady>();
        let events_tx = self.events_tx.clone();
        let thread_name = format!("z3rm-ext-{extension_id}");
        let thread_extension_id = extension_id.clone();
        let join = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                worker_thread_main(
                    thread_extension_id,
                    setup,
                    events_tx,
                    instance,
                    command_rx,
                    ready_tx,
                );
            })
            .map_err(|error| format!("spawning worker thread failed: {error}"))?;

        match ready_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(WorkerReady::Running) => {
                self.insert_worker(extension_id, command_tx, join, instance, false, None);
                Ok(())
            }
            Ok(WorkerReady::Suspended(reason)) => {
                self.insert_worker(extension_id, command_tx, join, instance, true, Some(reason));
                Ok(())
            }
            Ok(WorkerReady::Failed(error)) => {
                // The thread already exited; reap it.
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                // Timed out: the worker may still be loading. Register it as
                // suspended so nothing routes work to it and its thread is
                // reaped at shutdown; when the load completes its ready send
                // fails and the worker exits on its own.
                self.insert_worker(
                    extension_id,
                    command_tx,
                    join,
                    instance,
                    true,
                    Some(SuspensionReason::Unresponsive),
                );
                Err("worker did not finish loading in time; it was suspended".to_string())
            }
        }
    }

    /// Register a worker, atomically replacing any previous instance of the
    /// same id: the old worker is asked to wind down and all chrome it
    /// published (including a suspension notice) is tombstoned. The old
    /// join handle is dropped here — the host reaps replaced workers with
    /// the rest at shutdown, never waiting on a hung one.
    fn insert_worker(
        &mut self,
        extension_id: String,
        command_tx: mpsc::SyncSender<WorkerCommand>,
        join: std::thread::JoinHandle<()>,
        instance: u64,
        suspended: bool,
        initial_suspension: Option<SuspensionReason>,
    ) {
        if let Some(old) = self.workers.remove(&extension_id) {
            let _ = old.command_tx.try_send(WorkerCommand::Shutdown);
            let views = self.tombstone_chrome(&extension_id);
            if !views.is_empty() {
                let _ = self.chrome_tx.send(views);
            }
        }
        if let Some(reason) = initial_suspension {
            self.publish_notice(&extension_id, reason);
        }
        self.workers.insert(
            extension_id,
            WorkerHandle {
                command_tx,
                join,
                instance,
                suspended,
            },
        );
    }

    fn ids(&self) -> Vec<String> {
        self.workers.keys().cloned().collect()
    }

    /// §3.4 Fan one host event out to every non-suspended worker. `try_send`
    /// into a bounded queue: a worker that is still busy drops this event
    /// for it rather than accumulating unbounded work, and never blocks the
    /// manager or any other extension.
    fn emit(&mut self, event: &str, payload: &str) {
        let targets: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, worker)| !worker.suspended)
            .map(|(id, _)| id.clone())
            .collect();
        for id in targets {
            let Some(worker) = self.workers.get(&id) else {
                continue;
            };
            if worker.suspended {
                continue;
            }
            if worker
                .command_tx
                .try_send(WorkerCommand::Emit {
                    event: event.to_string(),
                    payload: payload.to_string(),
                })
                .is_err()
            {
                tracing::debug!(id = %id, %event, "server extension worker busy; event dropped for it");
            }
        }
    }

    /// Render all non-suspended workers (or only invalidated ones when
    /// `force` is false) with a single shared deadline, then publish the
    /// merged chrome plus tombstones for views that disappeared. Workers
    /// that do not answer by the deadline are suspended. `Err` means the
    /// chrome fan-out stopped (daemon shutdown).
    fn render_round(&mut self, force: bool) -> Result<(), ()> {
        let deadline = Instant::now() + WORKER_REPLY_TIMEOUT;
        let mut pending: Vec<(String, u64, mpsc::Receiver<Vec<String>>)> = Vec::new();
        for (id, worker) in &self.workers {
            if worker.suspended {
                continue;
            }
            let (reply_tx, reply_rx) = mpsc::channel();
            if worker
                .command_tx
                .try_send(WorkerCommand::Render { force, reply: reply_tx })
                .is_ok()
            {
                pending.push((id.clone(), worker.instance, reply_rx));
            }
        }

        let mut current_views: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut unresponsive: Vec<(String, u64)> = Vec::new();
        let mut crashed: Vec<(String, u64)> = Vec::new();
        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            // Round-robin poll so one hung worker's deadline does not starve
            // the replies of its healthy peers.
            let slice = remaining.min(Duration::from_millis(10));
            let mut index = 0;
            while index < pending.len() {
                let (id, instance, receiver) = &mut pending[index];
                match receiver.recv_timeout(slice) {
                    Ok(views) => {
                        // A worker suspended mid-round (its own report) must
                        // not have its chrome published.
                        if !self.workers.get(id).is_some_and(|worker| worker.suspended) {
                            for json in views {
                                current_views.insert((id.clone(), view_id_of(&json)), json);
                            }
                        }
                        pending.swap_remove(index);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => index += 1,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        crashed.push((id.clone(), *instance));
                        pending.swap_remove(index);
                    }
                }
            }
        }
        unresponsive.extend(pending.drain(..).map(|(id, instance, _)| (id, instance)));
        for (id, instance) in crashed {
            self.suspend_extension(&id, instance, SuspensionReason::Crashed);
        }
        for (id, instance) in unresponsive {
            self.suspend_extension(&id, instance, SuspensionReason::Unresponsive);
        }
        self.publish_views(current_views)
    }

    /// Publish the current views plus empty-payload tombstones for every
    /// previously published key that disappeared (an extension closed an
    /// overlay, or got suspended).
    fn publish_views(
        &mut self,
        current_views: BTreeMap<(String, String), String>,
    ) -> Result<(), ()> {
        let current_keys: BTreeSet<(String, String)> = current_views.keys().cloned().collect();
        let mut views = Vec::with_capacity(
            current_views.len() + self.published_views.difference(&current_keys).count(),
        );
        for ((extension_id, view_id), vdom_json) in current_views {
            views.push(ChromeView {
                extension_id,
                view_id,
                vdom_json,
            });
        }
        for (extension_id, view_id) in self.published_views.difference(&current_keys) {
            views.push(ChromeView {
                extension_id: extension_id.clone(),
                view_id: view_id.clone(),
                vdom_json: String::new(),
            });
        }
        self.published_views = current_keys;
        if views.is_empty() {
            return Ok(());
        }
        self.chrome_tx.send(views).map_err(|_| ())
    }

    /// ChromeViews (empty payloads) that remove every published view and the
    /// suspension notice of `extension_id` from the wire, plus bookkeeping.
    fn tombstone_chrome(&mut self, extension_id: &str) -> Vec<ChromeView> {
        let mut views = Vec::new();
        let stale: Vec<(String, String)> = self
            .published_views
            .iter()
            .filter(|(id, _)| id == extension_id)
            .cloned()
            .collect();
        for (id, view_id) in stale {
            self.published_views.remove(&(id.clone(), view_id.clone()));
            views.push(ChromeView {
                extension_id: id,
                view_id,
                vdom_json: String::new(),
            });
        }
        let notice_key = (extension_id.to_string(), SUSPENDED_NOTICE_VIEW_ID.to_string());
        if self.notices.remove(&notice_key) {
            views.push(ChromeView {
                extension_id: extension_id.to_string(),
                view_id: SUSPENDED_NOTICE_VIEW_ID.to_string(),
                vdom_json: String::new(),
            });
        }
        views
    }

    /// Publish (once per instance) the daemon-authored status-bar VDOM
    /// notice naming the suspended extension and the reason.
    fn publish_notice(&mut self, extension_id: &str, reason: SuspensionReason) {
        let key = (extension_id.to_string(), SUSPENDED_NOTICE_VIEW_ID.to_string());
        if !self.notices.insert(key) {
            return;
        }
        let notice = ChromeView {
            extension_id: extension_id.to_string(),
            view_id: SUSPENDED_NOTICE_VIEW_ID.to_string(),
            vdom_json: suspension_notice_json(extension_id, reason),
        };
        let _ = self.chrome_tx.send(vec![notice]);
    }

    /// §5.6 Suspend a worker: no further work is routed to it, its chrome is
    /// tombstoned and a status-bar notice with the reason is published. A
    /// hung worker is not joined — its thread is reaped (or detached) at
    /// host shutdown.
    fn suspend_extension(&mut self, extension_id: &str, instance: u64, reason: SuspensionReason) {
        let Some(worker) = self.workers.get_mut(extension_id) else {
            return;
        };
        // Late reports from a replaced worker must not suspend its successor.
        if worker.instance != instance || worker.suspended {
            return;
        }
        worker.suspended = true;
        tracing::error!(
            id = %extension_id,
            reason = reason.as_str(),
            "server extension suspended; chrome removed"
        );
        // Best-effort wind-down; a hung worker ignores this until its JS
        // returns (or never — the host detaches it at shutdown).
        let _ = worker.command_tx.try_send(WorkerCommand::Shutdown);
        let views = self.tombstone_chrome(extension_id);
        if !views.is_empty() {
            let _ = self.chrome_tx.send(views);
        }
        self.publish_notice(extension_id, reason);
    }

    /// §16.9 Execute a chrome action on the ONE extension that owns the
    /// named view. Fail-closed before any JS runs: unknown or suspended
    /// extensions and unpublished view ids are rejected. The wait is
    /// bounded; a worker that does not answer in time is suspended rather
    /// than letting the caller hang.
    fn execute_chrome_action(
        &mut self,
        extension_id: &str,
        view_id: &str,
        command: &str,
        arguments: &str,
    ) -> Result<()> {
        if command.is_empty() {
            bail!("chrome action command must not be empty");
        }
        let Some(worker) = self.workers.get(extension_id) else {
            bail!("chrome action targets unknown server extension `{extension_id}`");
        };
        if worker.suspended {
            bail!("chrome action targets suspended server extension `{extension_id}`");
        }
        if !self
            .published_views
            .contains(&(extension_id.to_string(), view_id.to_string()))
        {
            bail!(
                "chrome action targets view `{view_id}` of `{extension_id}`, which was never published to clients"
            );
        }
        let instance = worker.instance;
        let (reply_tx, reply_rx) = mpsc::channel();
        let queued = self
            .workers
            .get(extension_id)
            .is_some_and(|worker| {
                worker
                    .command_tx
                    .try_send(WorkerCommand::Execute {
                        command: command.to_string(),
                        arguments: arguments.to_string(),
                        reply: reply_tx,
                    })
                    .is_ok()
            });
        if !queued {
            bail!("chrome action target `{extension_id}` is busy; try again later");
        }
        match reply_rx.recv_timeout(WORKER_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.suspend_extension(extension_id, instance, SuspensionReason::Unresponsive);
                bail!(
                    "chrome action on `{extension_id}` did not complete in time; extension suspended"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.suspend_extension(extension_id, instance, SuspensionReason::Crashed);
                bail!("chrome action target `{extension_id}` exited unexpectedly");
            }
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Suspended {
                extension_id,
                instance,
                reason,
            } => self.suspend_extension(&extension_id, instance, reason),
            WorkerEvent::RenderRequested {
                extension_id,
                instance,
            } => {
                let current = self
                    .workers
                    .get(&extension_id)
                    .is_some_and(|worker| worker.instance == instance && !worker.suspended);
                if current {
                    let _ = self.render_round(false);
                }
            }
        }
    }
}

fn host_thread_main(
    user_extensions_dir: &Path,
    sessions: Sessions,
    command_rx: mpsc::Receiver<HostCommand>,
    command_tx: mpsc::Sender<HostCommand>,
    chrome_tx: tokio::sync::mpsc::UnboundedSender<Vec<ChromeView>>,
    shutdown_tx: mpsc::Sender<Vec<std::thread::JoinHandle<()>>>,
) {
    // §5.5 / §16.8 discovery: user dir + built-in roots, server-side filter.
    // discover_server_extensions already skips directories without
    // extension.toml + main.js and logs per-extension failures.
    let roots = extension_roots(user_extensions_dir);
    let discovered = discover_server_extensions(&roots);
    if discovered.is_empty() {
        tracing::info!(?roots, "no server-side extensions discovered");
    }
    // Startup discovery uses the ledger snapshot below. Install commands
    // reload it at their activation boundary so a newly approved manifest is
    // visible without restarting the daemon.
    let consent = load_server_consent(&server_consent_path(user_extensions_dir));
    let mut manager = ExtensionManager::new(chrome_tx, command_tx);

    // Startup: one worker thread per approved discovered extension, all
    // loads bounded by a single deadline so one stuck activation cannot
    // hold up the daemon's boot. Failures are logged, never fatal (§15.7).
    let startup_deadline = Instant::now() + STARTUP_LOAD_TIMEOUT;
    for extension in discovered {
        let id = extension.manifest.id.clone();
        if !ledger_approves(&consent, &extension.manifest) {
            let reason = match consent.get(&id) {
                Some(record) if record.state == ServerConsentState::Denied => {
                    "denied by the server approval ledger"
                }
                Some(record) if record.policy_fingerprint != extension.manifest.policy_fingerprint() => {
                    "policy fingerprint changed since approval; re-approval required"
                }
                _ => "not approved by the server approval ledger",
            };
            tracing::warn!(
                id = %id,
                fingerprint = %extension.manifest.policy_fingerprint(),
                path = %extension.directory.display(),
                %reason,
                "server extension not activated"
            );
            continue;
        }
        if let Err(error) = manager.spawn_worker(
            id.clone(),
            WorkerSetup::Discovered {
                manifest: extension.manifest,
                source: extension.source,
                sessions: sessions.clone(),
            },
            &startup_deadline,
        ) {
            tracing::warn!(id = %id, %error, "server extension startup load failed");
        }
    }

    // First paint: extensions register chrome during activate.
    if manager.render_round(true).is_err() {
        return;
    }

    loop {
        let command = match command_rx.recv() {
            Ok(command) => command,
            Err(_) => break,
        };
        match command {
            HostCommand::Install {
                manifest,
                archive,
                reply,
            } => {
                let id = manifest.id.clone();
                let deadline = Instant::now() + INSTALL_TIMEOUT;
                let result = manager
                    .spawn_worker(
                        id.clone(),
                        WorkerSetup::Install {
                            manifest,
                            archive,
                            user_extensions_dir: user_extensions_dir.to_path_buf(),
                            sessions: sessions.clone(),
                            consent: load_server_consent(&server_consent_path(user_extensions_dir)),
                        },
                        &deadline,
                    )
                    .map_err(anyhow::Error::msg);
                let install_succeeded = result.is_ok();
                if reply.send(result).is_err() {
                    tracing::debug!(id = %id, "extension install caller dropped before reply");
                }
                if install_succeeded && manager.render_round(true).is_err() {
                    return;
                }
            }
            HostCommand::ExecuteExtension {
                extension_id,
                view_id,
                command,
                arguments,
                reply,
            } => {
                let result = manager.execute_chrome_action(
                    &extension_id,
                    &view_id,
                    &command,
                    &arguments,
                );
                if reply.send(result).is_err() {
                    tracing::debug!(
                        id = %extension_id,
                        "extension chrome action caller dropped before reply"
                    );
                }
            }
            HostCommand::Emit { event, payload } => manager.emit(&event, &payload),
            HostCommand::Render => {
                if manager.render_round(true).is_err() {
                    return;
                }
            }
            HostCommand::ListIds(reply) => {
                if reply.send(manager.ids()).is_err() {
                    tracing::debug!("extension id caller dropped before reply");
                }
            }
            HostCommand::WorkerEvent(event) => manager.handle_worker_event(event),
            HostCommand::Shutdown => break,
        }
    }

    // Hand the worker join handles back so the host owner can reap them
    // without ever waiting on a hung worker itself.
    let handles: Vec<std::thread::JoinHandle<()>> = manager
        .workers
        .into_values()
        .map(|worker| worker.join)
        .collect();
    let _ = shutdown_tx.send(handles);
}

/// One extension's dedicated OS thread. The `LiveExtension` is created and
/// retained here; every `ctx.with` re-entry (activation, events, rendering,
/// commands) happens on this thread only (§5.2). A hung extension blocks
/// nobody but this thread: the manager bounds every wait and skips workers
/// that do not answer.
fn worker_thread_main(
    extension_id: String,
    setup: WorkerSetup,
    events_tx: mpsc::Sender<HostCommand>,
    instance: u64,
    command_rx: mpsc::Receiver<WorkerCommand>,
    ready_tx: mpsc::Sender<WorkerReady>,
) {
    let live = match setup {
        WorkerSetup::Install {
            manifest,
            archive,
            user_extensions_dir,
            sessions,
            consent,
        } => match install_on_host_thread(
            &user_extensions_dir,
            &manifest,
            &archive,
            sessions,
            &consent,
        ) {
            Ok(live) => live,
            Err(error) => {
                let _ = ready_tx.send(WorkerReady::Failed(format!("{error:#}")));
                return;
            }
        },
        WorkerSetup::Discovered {
            manifest,
            source,
            sessions,
        } => {
            // §5.6 每个扩展按自己的 manifest 声明构造专属桥: 文件系统范围在桥
            // 构造时固化, `filesystem.*` 只对该扩展声明范围内的路径放行。
            let bridge = Arc::new(ServerHostBridge::new(
                sessions,
                manifest.capabilities.filesystem,
            ));
            let runner = ExtensionRunner::for_manifest(&manifest).with_bridge(bridge);
            match runner.load_live(&extension_id, &source, "activate") {
                Ok(live) => live,
                Err(error) => {
                    tracing::warn!(
                        id = %extension_id,
                        error = %format!("{error:#}"),
                        "server extension load failed"
                    );
                    let _ = ready_tx.send(WorkerReady::Failed(format!("{error:#}")));
                    return;
                }
            }
        }
    };

    let mut hosted = HostedExtension {
        live,
        suspended: false,
    };
    // Activation itself may have crossed a limit (IO quota rejection is
    // flagged in Rust and survives JS try/catch). Report that before the
    // manager publishes any chrome.
    let ready = match hosted.detect_violation() {
        Some(reason) => WorkerReady::Suspended(reason),
        None => WorkerReady::Running,
    };
    if ready_tx.send(ready).is_err() {
        // The manager timed out or went away before the load finished; the
        // worker has no home, so exit.
        return;
    }

    loop {
        match command_rx.recv() {
            Ok(WorkerCommand::Emit { event, payload }) => {
                if let Err(error) = hosted.live.emit_event(&event, &payload) {
                    tracing::warn!(id = %extension_id, %event, %error, "server extension emit failed");
                }
                if hosted.finish_operation(&events_tx, instance) {
                    return;
                }
            }
            Ok(WorkerCommand::Render { force, reply }) => {
                let views = hosted.render(force);
                // A reply receiver that is gone just means the manager ended
                // the round without us; keep serving.
                let _ = reply.send(views);
                if hosted.finish_operation(&events_tx, instance) {
                    return;
                }
            }
            Ok(WorkerCommand::Execute {
                command,
                arguments,
                reply,
            }) => {
                let result = hosted
                    .live
                    .execute_command(&command, &arguments)
                    .map(|_| ());
                if reply.send(result).is_err() {
                    // The manager suspended us (the reply was dropped after
                    // its bounded wait); wind down.
                    return;
                }
                if hosted.finish_operation(&events_tx, instance) {
                    return;
                }
            }
            Ok(WorkerCommand::Shutdown) | Err(_) => return,
        }
    }
}

/// The daemon-authored status-bar VDOM for a suspended extension: a single
/// bounded span carrying the extension id and the suspension reason.
fn suspension_notice_json(extension_id: &str, reason: SuspensionReason) -> String {
    serde_json::json!({
        "type": "span",
        "text": format!("extension {extension_id} suspended: {}", reason.as_str()),
    })
    .to_string()
}

/// Extract, validate on disk, and activate an installed extension. Extraction
/// goes to a staging directory first, so a failed install never leaves a
/// half-written directory that startup discovery would pick up, and the
/// previously installed version stays live until the new one activates.
///
/// §5.6 Approval gate: the on-disk manifest (the copy that would actually
/// load) must match an `Approved` ledger record exactly. Unapproved,
/// denied, or fingerprint-changed installs fail with a contextual error and
/// never reach activation.
fn install_on_host_thread(
    user_extensions_dir: &Path,
    expected_manifest: &ExtensionManifest,
    archive: &[u8],
    sessions: Sessions,
    consent: &BTreeMap<String, ServerConsentRecord>,
) -> Result<LiveExtension> {
    let id = &expected_manifest.id;
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
        web_time::SystemTime::now()
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
        if &manifest != expected_manifest {
            bail!("archive manifest for `{id}` does not match the request manifest");
        }
        // Defense in depth: the request pre-validated the shipped manifest,
        // but the on-disk copy is what actually loads.
        if !manifest.side.runs_on_server() {
            bail!(
                "on-disk manifest for `{id}` declares runtime side `{:?}`; refusing to run it on the server",
                manifest.side
            );
        }
        // §5.6 Defense in depth: the async side pre-validated the shipped
        // manifest against the ledger, but the on-disk copy is what would
        // actually load — an approval binds to exact content, so re-check it.
        if !ledger_approves(consent, &manifest) {
            bail!(
                "extension `{id}` is not approved for server activation (policy fingerprint `{}`); approve it first",
                manifest.policy_fingerprint()
            );
        }
        let source_path = staged.join("main.js");
        let source = std::fs::read_to_string(&source_path)
            .with_context(|| format!("reading {}", source_path.display()))?;

        // §5.6 与发现装载一致: 按本扩展 manifest 声明的文件系统范围构造专属桥。
        let runner = ExtensionRunner::for_manifest(&manifest).with_bridge(Arc::new(
            ServerHostBridge::new(sessions.clone(), manifest.capabilities.filesystem),
        ));
        let live = runner
            .load_live(&manifest.id, &source, "activate")
            .with_context(|| format!("activating extension `{id}`"))?;

        // Activation succeeded — swap the staged directory into place.
        let target = user_extensions_dir.join(id);
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("removing previous install at {}", target.display()))?;
        }
        std::fs::rename(&staged, &target)
            .with_context(|| format!("moving staged extension to {}", target.display()))?;
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

/// §16.9 Execute a chrome action on the one extension that published the
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
        let size = entry
            .header()
            .entry_size()
            .context("tar entry missing size")?;
        extracted = extracted.saturating_add(size);
        if extracted > MAX_EXTRACTED_BYTES {
            bail!("extension archive exceeds the {MAX_EXTRACTED_BYTES}-byte uncompressed limit");
        }
        let unpacked = entry
            .unpack_in(target)
            .with_context(|| format!("unpacking {}", relative.display()))?;
        if !unpacked {
            bail!(
                "tar entry {} escapes the install directory",
                relative.display()
            );
        }
    }
    Ok(())
}

/// The VDOM JSON carries the view's own `id` when extensions register named
/// chrome views; fall back to a stable placeholder for bare renders.
fn view_id_of(vdom_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(vdom_json)
        .ok()
        .and_then(|value| {
            value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "default".to_string())
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

    fn approve_request(host: &ServerExtensionHost, request: &mux_protocol::InstallExtensionRequest) {
        let manifest_text = std::str::from_utf8(&request.manifest).unwrap();
        let manifest = quickjs_runtime::parse_manifest_str(&request.name, manifest_text).unwrap();
        host.set_consent(&manifest, true).unwrap();
    }

    async fn install_approved(
        host: &ServerExtensionHost,
        request: &mux_protocol::InstallExtensionRequest,
    ) -> Result<()> {
        approve_request(host, request);
        host.install_extension(request).await
    }

    fn write_approval(dir: &Path, manifest_text: &str) -> Result<()> {
        let manifest = quickjs_runtime::parse_manifest_str("test", manifest_text)?;
        let record = ServerConsentRecord {
            id: manifest.id.clone(),
            policy_fingerprint: manifest.policy_fingerprint(),
            state: ServerConsentState::Approved,
        };
        save_server_consent_raw(&server_consent_path(dir), &[record.to_json()])
    }

    #[test]
    fn validate_extension_id_rejects_unsafe_names() {
        assert!(validate_extension_id("demo").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", "a\x00b", "a\nb"] {
            assert!(
                validate_extension_id(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        let long = "x".repeat(MAX_EXTENSION_ID_LEN + 1);
        assert!(validate_extension_id(&long).is_err());
    }

    #[test]
    fn bridge_lists_sessions_and_rejects_unknown_methods() {
        let (sessions, _rx) = sessions_with_subscriber();
        // 未声明 filesystem 能力: 范围取最保守的 `None`, 文件系统调用在碰
        // 文件系统之前就被桥拒绝 (fail closed)。
        let bridge = ServerHostBridge::new(sessions, FilesystemAccess::None);

        let listed = bridge
            .call("mux.listSessions", &serde_json::json!([]))
            .unwrap();
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

        assert!(
            bridge
                .call("mux.listSessions", &serde_json::json!([]))
                .is_ok()
        );
        // Implemented capabilities still validate arguments before touching
        // the host; malformed process requests fail with method context.
        let error = bridge
            .call("process.spawn", &serde_json::json!([]))
            .unwrap_err();
        assert!(
            error.to_string().contains("`process.spawn` requires"),
            "error={error}"
        );
        // None 范围: filesystem 调用明确报出未授予, 而不是 generic error。
        let error = bridge
            .call("filesystem.readTextFile", &serde_json::json!(["note.txt"]))
            .unwrap_err();
        assert!(
            error.to_string().contains("not granted"),
            "error={error}"
        );
        // Unknown method: fail closed with a contextual error.
        let error = bridge
            .call("foo.bar", &serde_json::json!([]))
            .unwrap_err();
        assert!(error.to_string().contains("unknown host method"));
        // Missing argument: contextual, names the method and position.
        let error = bridge
            .call("mux.sendInput", &serde_json::json!([]))
            .unwrap_err();
        assert!(error.to_string().contains("mux.sendInput"));
    }

    /// §5.6: host calls rejected by the manifest's `io_rate_limit` set the
    /// runtime's persistent violation flag even when the extension's JS
    /// catches the exceptions; the daemon-side supervisor must suspend the
    /// extension and publish a notice naming the reason.
    #[tokio::test]
    async fn io_rate_limit_rejection_suspends_server_extension() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (sessions, mut subscriber) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));
        let manifest_text = "id = \"io-limit\"\nname = \"io-limit\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"server\"\n\n[capabilities]\nmux = true\n\n[resources]\nio_rate_limit = 2\n";
        let main_js = r#"
            export function activate(context) {
                for (var i = 0; i < 8; i++) {
                    try { context.mux.listSessions(); } catch (error) {}
                }
            }
        "#;
        let request = mux_protocol::InstallExtensionRequest {
            name: "io-limit".to_string(),
            manifest: manifest_text.as_bytes().to_vec(),
            source: pack_archive(&[
                ("extension.toml", manifest_text),
                ("main.js", main_js),
            ]),
        };
        // §5.6: activation requires an explicit approval for the exact policy
        // fingerprint; the probe's IO budget test needs it installed.
        let manifest = quickjs_runtime::parse_manifest_str("io-limit", manifest_text)?;
        host.set_consent(&manifest, true)?;
        host.install_extension(&request).await?;

        // The worker detects the IO violation during activation (the flag is
        // set in Rust and survives JS try/catch) and reports Suspended; the
        // daemon publishes the suspension notice instead of the extension's
        // chrome.
        let update = recv_chrome_for(&mut subscriber, "io-limit").await;
        assert_eq!(
            update.view_id, "z3rm.suspended",
            "suspension must surface as the daemon notice view"
        );
        let payload = String::from_utf8(update.vdom_payload).unwrap();
        assert!(
            payload.contains("suspended") && payload.contains("io rate limit"),
            "notice payload: {payload}"
        );
        Ok(())
    }

    /// §5.6: 服务器桥按扩展声明的范围约束 `filesystem.*`——`Home` 只放行主目录
    /// 内的路径, `Cwd` 只放行权威工作区/当前工作根内的路径 (与
    /// `workspace.getPath` 报告的是同一个根), 越界与符号链接逃逸一律拒绝;
    /// `workspace.getPath` 返回守护进程 cwd; `network.fetch`/`process.spawn`
    /// 显式报能力级不支持错误, 而不是假装成功或落入 generic unknown-method。
    #[test]
    fn server_bridge_dispatches_declared_capabilities_safely() -> Result<()> {
        let (sessions, _rx) = sessions_with_subscriber();
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join("docs"))?;
        std::fs::write(home.join("docs").join("note.txt"), "secret")?;
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src"))?;
        std::fs::write(workspace.join("src").join("main.js"), "code")?;

        // workspace.getPath: 守护进程 cwd, 只读无参。
        let cwd = serde_json::json!(std::env::current_dir()?.to_string_lossy().to_string());

        // -- Home 声明: 全部操作限制在 (注入的) 主目录内。--
        let bridge = ServerHostBridge::with_home(
            sessions.clone(),
            home.clone(),
            FilesystemAccess::Home,
        );
        assert_eq!(
            bridge.call("workspace.getPath", &serde_json::json!([]))?,
            cwd
        );

        // filesystem.readTextFile: 主目录内放行。
        let text = bridge.call(
            "filesystem.readTextFile",
            &serde_json::json!(["docs/note.txt"]),
        )?;
        assert_eq!(text, serde_json::json!("secret"));
        // 越界路径: 在碰文件系统之前就被范围检查拒绝。
        let error = bridge
            .call("filesystem.readTextFile", &serde_json::json!(["/etc/passwd"]))
            .unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        // 主目录内的缺失文件: 报读取错误, 而不是误导性的范围错误。
        let error = bridge
            .call(
                "filesystem.readTextFile",
                &serde_json::json!(["docs/missing.txt"]),
            )
            .unwrap_err();
        assert!(!error.to_string().contains("escapes"), "error={error}");

        // filesystem.readDir: 主目录内放行, 条目带 name/kind。
        let entries = bridge.call("filesystem.readDir", &serde_json::json!(["docs"]))?;
        assert_eq!(
            entries,
            serde_json::json!([{ "name": "note.txt", "kind": "file" }])
        );
        let error = bridge
            .call("filesystem.readDir", &serde_json::json!(["/etc"]))
            .unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");

        // -- Cwd 声明: 只放行权威工作区/当前工作根内的路径。cwd 声明不能
        // 因此读取任意 HOME 文件——主目录与工作区互相隔离。--
        let cwd_bridge = ServerHostBridge::with_roots(
            sessions,
            home.clone(),
            workspace.clone(),
            FilesystemAccess::Cwd,
        );
        // 工作区根内的相对路径放行。
        let text = cwd_bridge.call(
            "filesystem.readTextFile",
            &serde_json::json!(["src/main.js"]),
        )?;
        assert_eq!(text, serde_json::json!("code"));
        // 工作区根内的绝对路径放行。
        let text = cwd_bridge.call(
            "filesystem.readTextFile",
            &serde_json::json!([workspace.join("src/main.js").to_string_lossy()]),
        )?;
        assert_eq!(text, serde_json::json!("code"));
        // HOME 内的文件: 拒绝 (cwd 声明不能读取任意 HOME 文件)。
        let error = cwd_bridge
            .call(
                "filesystem.readTextFile",
                &serde_json::json!([home.join("docs/note.txt").to_string_lossy()]),
            )
            .unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        // 经 ".." 从工作区逃进主目录: 拒绝。
        let error = cwd_bridge
            .call(
                "filesystem.readTextFile",
                &serde_json::json!(["../home/docs/note.txt"]),
            )
            .unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");
        // 工作区根外的绝对路径: 拒绝。
        let error = cwd_bridge
            .call("filesystem.readTextFile", &serde_json::json!(["/etc/passwd"]))
            .unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");

        // -- Home 声明同样不能逃出主目录: 工作区在 home 之外, 拒绝。--
        let error = bridge
            .call(
                "filesystem.readTextFile",
                &serde_json::json!([workspace.join("src/main.js").to_string_lossy()]),
            )
            .unwrap_err();
        assert!(error.to_string().contains("escapes"), "error={error}");

        // Network and process are implemented, but malformed requests must
        // fail before any unbounded or ambiguous host work occurs.
        let error = bridge
            .call("network.fetch", &serde_json::json!(["not a valid URI"]))
            .unwrap_err();
        assert!(error.to_string().contains("invalid URL"), "error={error}");
        let error = bridge
            .call("process.spawn", &serde_json::json!([]))
            .unwrap_err();
        assert!(
            error.to_string().contains("`process.spawn` requires"),
            "error={error}"
        );
        Ok(())
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

        let client_manifest = "id = \"client-only\"\nname = \"client-only\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"client\"\n";
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
    async fn install_rejects_an_archive_with_a_different_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));
        let requested_manifest = server_manifest("requested");
        let archive_manifest = server_manifest("substituted");
        let request = mux_protocol::InstallExtensionRequest {
            name: "requested".to_string(),
            manifest: requested_manifest.into_bytes(),
            source: pack_archive(&[
                ("extension.toml", &archive_manifest),
                ("main.js", "export function activate() {}"),
            ]),
        };
        approve_request(&host, &request);

        let error = host.install_extension(&request).await.unwrap_err();

        assert!(
            format!("{error:#}").contains("does not match"),
            "unexpected error: {error:#}"
        );
        assert!(!temp.path().join("extensions/requested").exists());
    }

    #[tokio::test]
    async fn install_rejects_a_manifest_with_a_different_request_name() {
        let temp = tempfile::tempdir().unwrap();
        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, temp.path().join("extensions"));
        let manifest = server_manifest("manifest-id");
        let request = mux_protocol::InstallExtensionRequest {
            name: "request-name".to_string(),
            manifest: manifest.as_bytes().to_vec(),
            source: pack_archive(&[
                ("extension.toml", &manifest),
                ("main.js", "export function activate() {}"),
            ]),
        };

        let error = host.install_extension(&request).await.unwrap_err();

        assert!(
            format!("{error:#}").contains("does not match request name"),
            "unexpected error: {error:#}"
        );
        assert!(!temp.path().join("extensions/manifest-id").exists());
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

        install_approved(&host, &install_request("late-ext", main_js))
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
        assert!(
            String::from_utf8(update.vdom_payload)
                .unwrap()
                .contains("late")
        );
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
        install_approved(&host, &install_request("server-demo", main_js))
            .await
            .unwrap();

        // Built-in server extensions from the repo root load alongside ours.
        assert!(
            host.loaded_extension_ids()
                .await
                .iter()
                .any(|id| id == "server-demo"),
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
        let bad_request = install_request(
            "bad-ext",
            "export function activate() { throw new Error('nope'); }",
        );
        approve_request(&host, &bad_request);
        let error = host.install_extension(&bad_request).await.unwrap_err();
        assert!(format!("{error:#}").contains("nope"));
        // …without leaving a broken directory for startup discovery.
        assert!(!temp.path().join("extensions/bad-ext").exists());

        // …and a subsequent good extension still installs.
        let good_request = install_request("good-ext", "export function activate(context) {}");
        install_approved(&host, &good_request).await.unwrap();
        let ids = host.loaded_extension_ids().await;
        assert!(
            ids.iter().any(|id| id == "good-ext"),
            "good-ext not loaded: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "bad-ext"),
            "bad-ext must not load: {ids:?}"
        );
    }

    #[tokio::test]
    async fn startup_discovers_installed_server_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let extensions_dir = temp.path().join("extensions");
        let extension_dir = extensions_dir.join("boot-ext");
        std::fs::create_dir_all(&extension_dir).unwrap();
        let boot_manifest = server_manifest("boot-ext");
        std::fs::write(extension_dir.join("extension.toml"), &boot_manifest).unwrap();
        std::fs::write(
            extension_dir.join("main.js"),
            "export function activate(context) {}",
        )
        .unwrap();
        write_approval(&extensions_dir, &boot_manifest).unwrap();
        // A client-only sibling must NOT load on the server.
        let client_dir = extensions_dir.join("gui-ext");
        std::fs::create_dir_all(&client_dir).unwrap();
        std::fs::write(
            client_dir.join("extension.toml"),
            "id = \"gui-ext\"\nname = \"gui-ext\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"client\"\n",
        )
        .unwrap();
        std::fs::write(
            client_dir.join("main.js"),
            "export function activate(context) {}",
        )
        .unwrap();

        let (sessions, _rx) = sessions_with_subscriber();
        let host = ServerExtensionHost::start(sessions, extensions_dir);
        let ids = host.loaded_extension_ids().await;
        assert!(
            ids.iter().any(|id| id == "boot-ext"),
            "boot-ext not loaded: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "gui-ext"),
            "client-only extension ran on server: {ids:?}"
        );
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

        install_approved(&host, &install_request("event-ext", main_js))
            .await
            .unwrap();
        let initial = recv_chrome_for(&mut subscriber, "event-ext").await;
        assert!(
            String::from_utf8(initial.vdom_payload)
                .unwrap()
                .contains("initial")
        );

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
