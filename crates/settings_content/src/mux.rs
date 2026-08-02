use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};
use std::time::Duration;

/// spec §16.1 默认 mux socket 连接超时 (毫秒)。
///
/// §16.1 treats a connect timeout as "no daemon is answering here" — the
/// trigger for spawning one — so this bound has to stay short enough that GUI
/// startup does not visibly stall.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 500;

/// 多路复用器设置 (spec §16 Plan 16)
#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(default)]
pub struct MuxSettingsContent {
    /// Unix socket path for the mux server.
    pub socket_path: Option<String>,

    /// Connection timeout in milliseconds. Default: 500
    pub connect_timeout_ms: Option<u64>,

    /// Whether to keep the mux server alive when no clients are connected. Default: true
    pub keep_alive: bool,

    /// Keep-alive interval in seconds.
    pub keep_alive_seconds: Option<u64>,

    /// Keymap profile to use for terminal keybindings.
    /// Available profiles: "default", "tmux", "zellij", "screen". Default: "default"
    pub keymap_profile: Option<String>,

    /// Tabbar position in the terminal UI. Default: "top"
    pub tabbar_style: TabBarStyle,

    /// Scroll mode: per_client or global. Default: "per_client"
    pub scroll_mode: ScrollMode,
}

impl MuxSettingsContent {
    /// spec §16.1 已解析的 socket 连接超时。
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(
            self.connect_timeout_ms
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        )
    }
}

/// Tabbar position in the terminal UI.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum TabBarStyle {
    /// Tabbar displayed at the top of the terminal window.
    #[default]
    Top,
    /// Tabbar displayed at the bottom of the terminal window.
    Bottom,
    /// Tabbar is hidden.
    Hidden,
}

/// Scroll mode for the multiplexer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum ScrollMode {
    /// Each client maintains its own scroll position independently.
    #[default]
    PerClient,
    /// All clients share a single global scroll position.
    Global,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// spec §16.1 未配置时必须落到 500ms 默认值。
    #[test]
    fn connect_timeout_defaults_to_500ms() {
        assert_eq!(
            MuxSettingsContent::default().connect_timeout(),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn connect_timeout_honors_configured_value() {
        let content = MuxSettingsContent {
            connect_timeout_ms: Some(1500),
            ..Default::default()
        };
        assert_eq!(content.connect_timeout(), Duration::from_millis(1500));
    }

    /// The connection path parses just the `mux` object out of settings.json,
    /// so it has to deserialize on its own with every other field absent.
    #[test]
    fn connect_timeout_parses_from_partial_json() -> anyhow::Result<()> {
        let content: MuxSettingsContent = serde_json::from_str(r#"{"connect_timeout_ms": 250}"#)?;
        assert_eq!(content.connect_timeout(), Duration::from_millis(250));
        Ok(())
    }
}
