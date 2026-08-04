//! §16.11 Server-side settings for mux_server.
//!
//! Client settings use SettingsStore file watching; the daemon has its own
//! lightweight owner so remote/local daemons can reload scrollback capacity
//! and keep-alive without pulling the full GPUI settings stack.
//!
//! Sources (highest wins at load time):
//! 1. Environment: `Z3RM_KEEP_ALIVE_SECONDS`, `Z3RM_SCROLLBACK_LINES`
//! 2. Optional JSON file: `$Z3RM_SERVER_SETTINGS` or
//!    `$XDG_CONFIG_HOME/z3rm/server.json` / `~/.config/z3rm/server.json`
//!
//! Hot reload watches the JSON path (if present) every few seconds and applies
//! safe live updates (scrollback capacity). keep_alive_seconds is read at
//! startup for the accept loop; a change is logged and applied on next idle
//! cycle via the shared AtomicU64.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;

const DEFAULT_SCROLLBACK_LINES: u64 = 10_000;
const DEFAULT_KEEP_ALIVE_SECONDS: u64 = 0; // 0 = forever

/// Live server settings shared with the accept loop and pane spawn paths.
#[derive(Debug)]
pub struct ServerSettings {
    pub keep_alive_seconds: AtomicU64,
    pub scrollback_lines: AtomicU64,
    settings_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct ServerSettingsFile {
    #[serde(default)]
    keep_alive_seconds: Option<u64>,
    #[serde(default)]
    scrollback_lines: Option<u64>,
    /// Alias matching client terminal.max_scroll_history_lines
    #[serde(default)]
    max_scroll_history_lines: Option<u64>,
}

impl ServerSettings {
    pub fn load() -> Arc<Self> {
        let path = resolve_settings_path();
        let mut keep_alive = std::env::var("Z3RM_KEEP_ALIVE_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_KEEP_ALIVE_SECONDS);
        let mut scrollback = std::env::var("Z3RM_SCROLLBACK_LINES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SCROLLBACK_LINES);

        if let Some(ref p) = path {
            if let Some(file) = read_file(p) {
                if let Some(v) = file.keep_alive_seconds {
                    keep_alive = v;
                }
                if let Some(v) = file.scrollback_lines.or(file.max_scroll_history_lines) {
                    scrollback = v.min(100_000);
                }
            }
        }

        Arc::new(Self {
            keep_alive_seconds: AtomicU64::new(keep_alive),
            scrollback_lines: AtomicU64::new(scrollback),
            settings_path: path,
        })
    }

    pub fn scrollback_lines(&self) -> usize {
        self.scrollback_lines.load(Ordering::Relaxed) as usize
    }

    pub fn keep_alive_seconds(&self) -> u64 {
        self.keep_alive_seconds.load(Ordering::Relaxed)
    }

    /// Apply a file snapshot; returns true if scrollback changed.
    pub(crate) fn apply_file(&self, file: &ServerSettingsFile) -> bool {
        let mut scrollback_changed = false;
        if let Some(v) = file.keep_alive_seconds {
            self.keep_alive_seconds.store(v, Ordering::Relaxed);
        }
        if let Some(v) = file.scrollback_lines.or(file.max_scroll_history_lines) {
            let v = v.min(100_000);
            let prev = self.scrollback_lines.swap(v, Ordering::Relaxed);
            scrollback_changed = prev != v;
        }
        scrollback_changed
    }

    pub fn settings_path(&self) -> Option<&Path> {
        self.settings_path.as_deref()
    }
}

pub fn default_scrollback_lines() -> usize {
    std::env::var("Z3RM_SCROLLBACK_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SCROLLBACK_LINES) as usize
}

fn resolve_settings_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("Z3RM_SERVER_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    let config = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")));
    let path = config?.join("z3rm").join("server.json");
    if path.exists() {
        Some(path)
    } else {
        // Still return the path so a watcher can pick it up if created later.
        Some(path)
    }
}

fn read_file(path: &Path) -> Option<ServerSettingsFile> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Spawn a background task that reloads server.json every `interval` and
/// applies scrollback capacity to all live panes.
pub fn spawn_hot_reload(
    settings: Arc<ServerSettings>,
    sessions: Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
) {
    let interval = Duration::from_secs(2);
    tokio::spawn(async move {
        let mut last_mtime = None::<std::time::SystemTime>;
        loop {
            tokio::time::sleep(interval).await;
            let Some(path) = settings.settings_path() else {
                continue;
            };
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if last_mtime == Some(mtime) {
                continue;
            }
            last_mtime = Some(mtime);
            let Some(file) = read_file(path) else {
                zlog::warn!("server settings reload failed to parse {}", path.display());
                continue;
            };
            let scrollback_changed = settings.apply_file(&file);
            zlog::info!(
                "server settings reloaded from {}: keep_alive={}s scrollback={}",
                path.display(),
                settings.keep_alive_seconds(),
                settings.scrollback_lines()
            );
            if scrollback_changed {
                let cap = settings.scrollback_lines();
                let sessions_r = sessions.read();
                for session in sessions_r.iter() {
                    let panes = session.panes.read();
                    for pane in panes.values() {
                        pane.set_scrollback_capacity(cap);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// §3.5 daemon 默认永不自动退出，直到被显式 kill —— 这是 tmux 用户的预期。
    /// `0` 编码的就是"永远"，把它改成一个有限秒数会让 session 在闲置后无声消失。
    #[test]
    fn keep_alive_defaults_to_forever() {
        assert_eq!(
            DEFAULT_KEEP_ALIVE_SECONDS, 0,
            "0 means never expire; a finite default would silently drop idle sessions"
        );
    }

    #[test]
    fn apply_file_updates_atomics() {
        let settings = ServerSettings {
            keep_alive_seconds: AtomicU64::new(0),
            scrollback_lines: AtomicU64::new(10_000),
            settings_path: None,
        };
        let changed = settings.apply_file(&ServerSettingsFile {
            keep_alive_seconds: Some(30),
            scrollback_lines: Some(2000),
            max_scroll_history_lines: None,
        });
        assert!(changed);
        assert_eq!(settings.keep_alive_seconds(), 30);
        assert_eq!(settings.scrollback_lines(), 2000);
    }

    #[test]
    fn scrollback_caps_at_100k() {
        let settings = ServerSettings {
            keep_alive_seconds: AtomicU64::new(0),
            scrollback_lines: AtomicU64::new(10_000),
            settings_path: None,
        };
        settings.apply_file(&ServerSettingsFile {
            keep_alive_seconds: None,
            scrollback_lines: Some(500_000),
            max_scroll_history_lines: None,
        });
        assert_eq!(settings.scrollback_lines(), 100_000);
    }

    #[test]
    fn read_file_parses_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{"keep_alive_seconds": 12, "scrollback_lines": 1234}}"#
        )
        .unwrap();
        let parsed = read_file(&path).expect("parse");
        assert_eq!(parsed.keep_alive_seconds, Some(12));
        assert_eq!(parsed.scrollback_lines, Some(1234));
    }

    /// §16.11 The shipped reference sample
    /// (`crates/mux_server/server.example.json`) must parse through the
    /// production deserializer and carry the documented default values. This
    /// guards the "default server.json sample" against schema drift.
    #[test]
    fn example_file_parses_with_defaults() {
        let sample = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("server.example.json");
        let parsed =
            read_file(&sample).unwrap_or_else(|| panic!("parse {}: failed", sample.display()));
        assert_eq!(parsed.keep_alive_seconds, Some(0));
        assert_eq!(parsed.scrollback_lines, Some(10_000));
        assert_eq!(parsed.max_scroll_history_lines, Some(10_000));

        // Applying the sample to a fresh ServerSettings must yield the same
        // live scrollback the default boot path produces (10_000), proving a
        // daemon pointed at the sample behaves identically to the env default.
        let settings = ServerSettings {
            keep_alive_seconds: AtomicU64::new(999),
            scrollback_lines: AtomicU64::new(999),
            settings_path: None,
        };
        settings.apply_file(&parsed);
        assert_eq!(settings.keep_alive_seconds(), 0);
        assert_eq!(settings.scrollback_lines(), 10_000);
    }
}
