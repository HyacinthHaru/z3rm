use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

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
