// The derive(RegisterSetting) macro emits paths through the `settings` crate
// name; inside the crate itself that only resolves via a self-extern.
extern crate self as settings;

mod base_keymap_setting;
mod content_into_gpui;
mod editable_setting_control;
mod editorconfig_store;
mod keymap_file;
pub mod mux_actions;
mod settings_file;
mod settings_store;
mod vscode_import;

pub use settings_macros::RegisterSetting;

pub mod settings_content {
    pub use ::settings_content::*;
}

pub mod fallible_options {
    pub use ::settings_content::{FallibleOption, parse_json};
}

#[doc(hidden)]
pub mod private {
    pub use crate::settings_store::{RegisteredSetting, SettingValue};
    pub use inventory;
}

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use rust_embed::RustEmbed;
use std::env;
use std::{borrow::Cow, fmt, str};
use util::asset_str;

pub use ::settings_content::*;
pub use base_keymap_setting::*;
pub use content_into_gpui::IntoGpui;
pub use editable_setting_control::*;
pub use editorconfig_store::{
    Editorconfig, EditorconfigEvent, EditorconfigProperties, EditorconfigStore,
};
pub use keymap_file::{
    KeyBindingValidator, KeyBindingValidatorRegistration, KeybindSource, KeybindUpdateOperation,
    KeybindUpdateTarget, KeymapFile, KeymapFileLoadResult,
};
pub use settings_file::*;
pub use settings_json::*;
pub use settings_store::{
    DefaultSemanticTokenRules, InvalidSettingsError, LSP_SETTINGS_SCHEMA_URL_PREFIX,
    LocalSettingsKind, LocalSettingsPath, MigrationStatus, Settings, SettingsFile,
    SettingsJsonSchemaParams, SettingsKey, SettingsLocation, SettingsParseResult, SettingsStore,
};

pub use vscode_import::{VsCodeSettings, VsCodeSettingsSource};

pub use keymap_file::ActionSequence;
pub use settings_content::{
    AllLanguageSettingsContent, AutoIndentMode, CodestralSettingsContent,
    CompletionSettingsContent, CopilotSettingsContent, CursorShape, DiagnosticsSettingsContent,
    DocumentFoldingRanges, DocumentSymbols, EditPredictionDataCollectionChoice,
    EditPredictionPromptFormatContent, EditPredictionProvider, EditPredictionSettingsContent,
    EditPredictionsMode, EncodingDisplayOptions, ExtensionsSettingsContent, FileTypeMap,
    FormatOnSave, Formatter, FormatterList, GitHostingProviderConfig, GitHostingProviderKind,
    IconThemeName, IndentGuideBackgroundColoring, IndentGuideColoring, IndentGuidesSettingsContent,
    InlayHintKind, InlayHintsSettingsContent, JsxTagAutoCloseContent, LanguageSettingsContent,
    LanguageToSettingsMap, LineEndingSetting, LineIndicatorFormat, LspInsertMode, LspSettings,
    LspSettingsMap, ModifiersContent, OpenAiCompatibleApiSettingsContent, PrettierSettingsContent,
    REST_OF_LANGUAGE_SERVERS, RewrapBehavior, SemanticTokenRules, SemanticTokens,
    ShowWhitespaceSetting, SoftWrap, SshConnection, SshPortForwardOption, TaskSettingsContent,
    WhitespaceMapContent, WordsCompletionMode, WslConnection,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSettingsProfileName(pub String);

impl Global for ActiveSettingsProfileName {}

pub trait UserSettingsContentExt {
    fn for_profile(&self, cx: &App) -> Option<&SettingsProfile>;
    fn for_release_channel(&self) -> Option<&SettingsContent>;
    fn for_os(&self) -> Option<&SettingsContent>;
}

impl UserSettingsContentExt for UserSettingsContent {
    fn for_profile(&self, cx: &App) -> Option<&SettingsProfile> {
        let Some(active_profile) = cx.try_global::<ActiveSettingsProfileName>() else {
            return None;
        };
        self.profiles.get(&active_profile.0)
    }

    fn for_release_channel(&self) -> Option<&SettingsContent> {
        self.release_channel_overrides
            .get_by_key(release_channel::RELEASE_CHANNEL.dev_name())
    }

    fn for_os(&self) -> Option<&SettingsContent> {
        self.platform_overrides.get_by_key(env::consts::OS)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct WorktreeId(usize);

impl From<WorktreeId> for usize {
    fn from(value: WorktreeId) -> Self {
        value.0
    }
}

impl WorktreeId {
    pub fn from_usize(handle_id: usize) -> Self {
        Self(handle_id)
    }

    pub fn from_proto(id: u64) -> Self {
        Self(id as usize)
    }

    pub fn to_proto(self) -> u64 {
        self.0 as u64
    }

    pub fn to_usize(self) -> usize {
        self.0
    }
}

impl fmt::Display for WorktreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(RustEmbed)]
#[folder = "../../assets"]
#[include = "settings/*"]
#[include = "keymaps/*"]
#[exclude = "*.DS_Store"]
pub struct SettingsAssets;

pub fn init(cx: &mut App) {
    let settings = SettingsStore::new(cx, &default_settings());
    cx.set_global(settings);
    SettingsStore::observe_active_settings_profile_name(cx).detach();
}

pub fn default_settings() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/default.json")
}

pub fn default_semantic_token_rules() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/default_semantic_token_rules.json")
}

#[cfg(target_os = "macos")]
pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-macos.json";

#[cfg(target_os = "windows")]
pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-windows.json";

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const DEFAULT_KEYMAP_PATH: &str = "keymaps/default-linux.json";

pub fn default_keymap() -> Cow<'static, str> {
    asset_str::<SettingsAssets>(DEFAULT_KEYMAP_PATH)
}

pub const VIM_KEYMAP_PATH: &str = "keymaps/vim.json";

pub fn vim_keymap() -> Cow<'static, str> {
    asset_str::<SettingsAssets>(VIM_KEYMAP_PATH)
}

// ============================================================================
// §16.7 Mux keymap profile loading (spec §16.7 Plan 17)
// ============================================================================

/// §16.7 mux keymap profile 路径映射表。
pub const MUX_KEYMAP_PROFILE_PATHS: [(&str, &str); 4] = [
    ("default", "keymaps/default.json"),
    ("tmux", "keymaps/tmux.json"),
    ("zellij", "keymaps/zellij.json"),
    ("screen", "keymaps/screen.json"),
];

/// §16.7 可用的 mux keymap profile 名称列表。
pub const MUX_KEYMAP_PROFILE_NAMES: [&str; 4] = ["default", "tmux", "zellij", "screen"];

/// §16.7 根据 profile 名称获取对应的 keymap 文件路径。
/// 未知名称回退到 "default"。
pub fn mux_keymap_profile_path(profile: &str) -> &'static str {
    for (name, path) in MUX_KEYMAP_PROFILE_PATHS {
        if name == profile {
            return path;
        }
    }
    // 回退到 default profile
    MUX_KEYMAP_PROFILE_PATHS[0].1
}

/// §16.7 加载指定的 mux keymap profile 内容。
pub fn mux_keymap_profile_content(profile: &str) -> Cow<'static, str> {
    let path = mux_keymap_profile_path(profile);
    asset_str::<SettingsAssets>(path)
}

/// §16.7 加载指定 mux keymap profile 的全部绑定。
///
/// Built-in profiles must reject partial failures rather than silently retaining
/// broken bindings: a single bad binding is treated as a hard error so the profile
/// cannot be switched to a half-applied state. This mirrors the strict behaviour of
/// [`KeymapFile::load_asset`] and never uses the partial-failure path.
pub fn load_mux_keymap_profile(
    profile: &str,
    cx: &gpui::App,
) -> anyhow::Result<Vec<gpui::KeyBinding>> {
    let path = mux_keymap_profile_path(profile);
    // Built-in assets are not user-editable, so any per-binding failure is a bug
    // in the bundled profile rather than a user typo and must surface as an error.
    KeymapFile::load_asset(path, None, cx)
}

/// The mux keymap settings: which named keymap profile (default/tmux/
/// zellij/screen) the mux pane action chords come from.
#[derive(Clone, Debug, RegisterSetting)]
pub struct MuxSettings {
    pub keymap_profile: String,
}

impl Settings for MuxSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let mux = content.mux.clone().unwrap_or_default();
        Self {
            keymap_profile: mux.keymap_profile.unwrap_or_else(|| "default".to_string()),
        }
    }
}

/// Tracks the currently-active mux keymap profile plus its bindings so a
/// profile switch can emit `Unbind` entries for the prior keystrokes before
/// binding the new profile. Without this, switching profiles left the old
/// profile's bindings live alongside the new ones (§16.7).
pub struct ActiveMuxKeymapProfile {
    profile: String,
    bindings: Vec<gpui::KeyBinding>,
}

impl Global for ActiveMuxKeymapProfile {}

/// Load the platform default keymap plus the configured mux keymap profile,
/// and re-bind both whenever the settings change. Both the desktop binary and
/// the WebAssembly client call this so the two surfaces share one keymap.
pub fn bind_startup_keymaps(cx: &mut App) {
    match KeymapFile::load_asset_allow_partial_failure(DEFAULT_KEYMAP_PATH, cx) {
        Ok(key_bindings) => cx.bind_keys(key_bindings),
        Err(error) => tracing::error!(error = %error, "failed to load default keymap"),
    }
    bind_configured_mux_keymap_profile(cx);
    cx.observe_global::<SettingsStore>(|cx| {
        bind_configured_mux_keymap_profile(cx);
    })
    .detach();
}

pub fn bind_configured_mux_keymap_profile(cx: &mut App) {
    let profile = <MuxSettings as Settings>::get_global(cx)
        .keymap_profile
        .clone();
    if cx
        .try_global::<ActiveMuxKeymapProfile>()
        .is_some_and(|active| active.profile == profile)
    {
        return;
    }
    let path = mux_keymap_profile_path(&profile);
    // Built-in mux profiles reject partial failures (see load_mux_keymap_profile),
    // so a broken profile never leaves half-applied bindings.
    match load_mux_keymap_profile(&profile, cx) {
        Ok(key_bindings) => {
            // §16.7 Profile switching must not stack bindings. Before binding
            // the new profile, emit `Unbind` entries at the previous profile's
            // keystrokes (naming the previous action) so the keymap drops them.
            if let Some(prev) = cx.try_global::<ActiveMuxKeymapProfile>() {
                let unbinds: Vec<gpui::KeyBinding> = prev
                    .bindings
                    .iter()
                    .filter_map(|binding| {
                        // No-action and Unbind markers have no action name to clear.
                        if gpui::is_unbind(binding.action()) || gpui::is_no_action(binding.action())
                        {
                            return None;
                        }
                        if binding.keystrokes().is_empty() {
                            return None;
                        }
                        Some(binding.unbind())
                    })
                    .collect();
                if !unbinds.is_empty() {
                    cx.bind_keys(unbinds);
                }
            }
            // Clone before consuming — `cx.bind_keys` needs an owned iterator.
            let stored = key_bindings.clone();
            cx.bind_keys(key_bindings);
            cx.set_global(ActiveMuxKeymapProfile {
                profile,
                bindings: stored,
            });
        }
        Err(error) => {
            tracing::error!(profile, path, error = %error, "failed to load mux keymap profile")
        }
    }
}

/// Specific keybinding overrides. Loaded after the base keymap so they win over
/// conflicting base-keymap (and default `Editor`) bindings for the same chords,
/// while still allowing user keymaps (loaded last) to override them. Shared
/// across features - prefer adding a context block here over creating another
/// override keymap file.
#[cfg(target_os = "macos")]
pub const SPECIFIC_OVERRIDES_KEYMAP_PATH: &str = "keymaps/specific-overrides-macos.json";

#[cfg(not(target_os = "macos"))]
pub const SPECIFIC_OVERRIDES_KEYMAP_PATH: &str = "keymaps/specific-overrides.json";

pub fn initial_user_settings_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_user_settings.json")
}

pub fn initial_server_settings_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_server_settings.json")
}

pub fn initial_project_settings_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_local_settings.json")
}

pub fn initial_keymap_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("keymaps/initial.json")
}

pub fn initial_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_tasks.json")
}

pub fn initial_debug_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_debug_tasks.json")
}

pub fn initial_local_debug_tasks_content() -> Cow<'static, str> {
    asset_str::<SettingsAssets>("settings/initial_local_debug_tasks.json")
}
