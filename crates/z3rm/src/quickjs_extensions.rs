//! §5.2 QuickJS extension loader — scans extensions/ directory and loads
//! JS extensions via QuickJS on a dedicated OS thread.
//!
//! Per spec §5.2: "QuickJS runtime on a dedicated OS thread. The extension
//! host must not run on the GPUI render thread. Extensions communicate with
//! the UI via async channels; a hung extension freezes only itself."

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use quickjs_runtime::{ExtensionRunResult, ExtensionRunner};

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
    dir: &Path,
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

/// §5.2 Initialize the QuickJS extension system at startup.
/// Called from main.rs after GPUI app creation.
pub fn init_extensions(cx: &mut gpui::App) {
    let extensions_dir = paths::extensions_dir().clone();

    // §5.2: Load extensions on a background thread to avoid blocking the render loop.
    cx.background_executor()
        .spawn(async move {
            let loaded = load_client_extensions(&extensions_dir);
            tracing::info!(
                count = loaded.len(),
                "QuickJS extensions loaded"
            );
            for ext in &loaded {
                tracing::debug!(
                    id = %ext.id,
                    name = %ext.name,
                    side = ?ext.side,
                    ok = ext.result.result.is_ok(),
                    duration_ms = ext.result.duration.as_millis() as u64,
                    "extension status"
                );
            }
        })
        .detach();
}
