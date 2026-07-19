//! Terminal-oriented extension API traits (spec §5.3).
//!
//! These traits define the surface area available to QuickJS chrome extensions.
//! The JS→Rust FFI bindings live in `quickjs_runtime`; these traits describe
//! the contracts that extensions interact with.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gpui::Task;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Unique identifier for a mux_server session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(pub Arc<str>);

/// Unique identifier for a pane within a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PaneId(pub Arc<str>);

/// Command identifier registered by an extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CommandId(pub Arc<str>);

/// Key sequence for keymap bindings (e.g., "ctrl+shift+s").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct KeySequence(pub String);

/// Split direction for pane splitting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Event type emitted by a terminal pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneEvent {
    /// Pane title changed.
    TitleChanged { title: String },
    /// Pane exited / closed.
    Closed,
    /// New line output captured.
    Output { lines: Vec<String> },
}

// ---------------------------------------------------------------------------
// MuxApi — mux_server session/pane management
// ---------------------------------------------------------------------------

/// API for managing mux_server sessions and panes.
///
/// Provides split operations, input injection, output capture, and session listing.
/// Maps to mux_server's session/pane model (spec §3).
 
pub trait MuxApi: Send + Sync {
    /// Split the active pane in the given direction, returning the new pane ID.
    fn split_pane(&self, direction: SplitDirection) -> Result<PaneId>;

    /// Send raw input bytes to a pane (e.g., keystrokes for a shell).
    fn send_input(&self, pane_id: &PaneId, input: &[u8]) -> Result<()>;

    /// Capture the current visible content of a pane as text lines.
    fn capture_pane(&self, pane_id: &PaneId) -> Task<Result<Vec<String>>>;

    /// List all active sessions with their pane counts.
    fn list_sessions(&self) -> Result<BTreeMap<SessionId, usize>>;

    /// Close a pane by ID.
    fn close_pane(&self, pane_id: &PaneId) -> Result<()>;

    /// Switch focus to a specific pane.
    fn focus_pane(&self, pane_id: &PaneId) -> Result<()>;
}

// ---------------------------------------------------------------------------
// CommandApi — extension command registration and execution
// ---------------------------------------------------------------------------

/// API for registering and executing extension commands.
///
/// Commands are user-invokable actions that appear in the command palette.
 
pub trait CommandApi: Send + Sync {
    /// Register a command with a unique ID and display label.
    fn register_command(&self, id: CommandId, label: Arc<str>) -> Result<()>;

    /// Unregister a previously registered command.
    fn unregister_command(&self, id: &CommandId) -> Result<()>;

    /// Execute a registered command by ID.
    fn execute_command(&self, id: &CommandId) -> Task<Result<()>>;

    /// List all registered commands with their labels.
    fn list_commands(&self) -> BTreeMap<CommandId, Arc<str>>;
}

// ---------------------------------------------------------------------------
// KeymapApi — keyboard shortcut bindings
// ---------------------------------------------------------------------------

/// API for binding key sequences to commands.
 
pub trait KeymapApi: Send + Sync {
    /// Bind a key sequence to a command.
    fn bind_key(&self, key: KeySequence, command: CommandId) -> Result<()>;

    /// Remove a key binding.
    fn unbind_key(&self, key: &KeySequence) -> Result<()>;

    /// List all active key bindings.
    fn list_bindings(&self) -> BTreeMap<KeySequence, CommandId>;
}

// ---------------------------------------------------------------------------
// SettingsApi — extension settings read/write
// ---------------------------------------------------------------------------

/// API for reading and writing extension settings.
///
/// Settings are persisted per-extension in the z3rm config directory.
 
pub trait SettingsApi: Send + Sync {
    /// Read a setting value by key for this extension.
    fn read_setting(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// Write a setting value by key for this extension.
    fn write_setting(&self, key: &str, value: serde_json::Value) -> Result<()>;

    /// List all setting keys for this extension.
    fn list_settings(&self) -> Result<Vec<String>>;

    /// Remove a setting key.
    fn remove_setting(&self, key: &str) -> Result<()>;

    /// Path to the extension's settings file on disk.
    fn settings_path(&self) -> PathBuf;
}

// ---------------------------------------------------------------------------
// TerminalApi — terminal pane event subscription
// ---------------------------------------------------------------------------

/// API for subscribing to terminal pane events.
///
/// Extensions use this to react to pane lifecycle events (title changes,
/// output, close) for chrome overlays and status indicators.
 
pub trait TerminalApi: Send + Sync {
    /// Subscribe to events from a specific pane.
    /// Returns a channel receiver for the events.
    fn subscribe_pane(&self, pane_id: &PaneId) -> Result<futures::channel::mpsc::UnboundedReceiver<PaneEvent>>;

    /// Unsubscribe from a pane's events.
    fn unsubscribe_pane(&self, pane_id: &PaneId) -> Result<()>;

    /// Get the current title of a pane.
    fn get_pane_title(&self, pane_id: &PaneId) -> Result<String>;

    /// List all active pane IDs.
    fn list_panes(&self) -> Result<Vec<PaneId>>;
}

// ---------------------------------------------------------------------------
// ExtensionApi — aggregate facade for all extension APIs
// ---------------------------------------------------------------------------

/// Aggregate extension API combining all terminal-oriented traits.
///
/// This is the single object passed to QuickJS extensions at activation time.
pub struct ExtensionApi {
    pub mux: Box<dyn MuxApi>,
    pub command: Box<dyn CommandApi>,
    pub keymap: Box<dyn KeymapApi>,
    pub settings: Box<dyn SettingsApi>,
    pub terminal: Box<dyn TerminalApi>,
}

impl ExtensionApi {
    pub fn new(
        mux: Box<dyn MuxApi>,
        command: Box<dyn CommandApi>,
        keymap: Box<dyn KeymapApi>,
        settings: Box<dyn SettingsApi>,
        terminal: Box<dyn TerminalApi>,
    ) -> Self {
        Self {
            mux,
            command,
            keymap,
            settings,
            terminal,
        }
    }
}
