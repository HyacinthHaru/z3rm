use crate::*;
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use fs::Fs;
use gpui::Rgba;
use paths::{cursor_settings_file_paths, vscode_settings_file_paths};
use serde_json::{Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VsCodeSettingsSource {
    VsCode,
    Cursor,
}

impl std::fmt::Display for VsCodeSettingsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VsCodeSettingsSource::VsCode => write!(f, "VS Code"),
            VsCodeSettingsSource::Cursor => write!(f, "Cursor"),
        }
    }
}

pub struct VsCodeSettings {
    pub source: VsCodeSettingsSource,
    pub path: Arc<Path>,
    content: Map<String, Value>,
}

impl VsCodeSettings {
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_str(content: &str, source: VsCodeSettingsSource) -> Result<Self> {
        Ok(Self {
            source,
            path: Path::new("/example-path/Code/User/settings.json").into(),
            content: serde_json_lenient::from_str(content)?,
        })
    }

    pub async fn load_user_settings(source: VsCodeSettingsSource, fs: Arc<dyn Fs>) -> Result<Self> {
        let candidate_paths = match source {
            VsCodeSettingsSource::VsCode => vscode_settings_file_paths(),
            VsCodeSettingsSource::Cursor => cursor_settings_file_paths(),
        };
        let mut path = None;
        for candidate_path in candidate_paths.iter() {
            if fs.is_file(candidate_path).await {
                path = Some(candidate_path.clone());
            }
        }
        let Some(path) = path else {
            return Err(anyhow!(
                "No settings file found, expected to find it in one of the following paths:\n{}",
                candidate_paths
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        };
        let content = fs.load(&path).await.with_context(|| {
            format!(
                "Error loading {} settings file from {}",
                source,
                path.display()
            )
        })?;
        let content = serde_json_lenient::from_str(&content).with_context(|| {
            format!(
                "Error parsing {} settings file from {}",
                source,
                path.display()
            )
        })?;
        Ok(Self {
            source,
            path: path.into(),
            content,
        })
    }

    fn read_value(&self, setting: &str) -> Option<&Value> {
        self.content.get(setting)
    }

    fn read_string(&self, setting: &str) -> Option<String> {
        self.read_value(setting)
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
    }

    fn read_bool(&self, setting: &str) -> Option<bool> {
        self.read_value(setting).and_then(|v| v.as_bool())
    }

    fn read_f32(&self, setting: &str) -> Option<f32> {
        self.read_value(setting)
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
    }

    fn read_usize(&self, setting: &str) -> Option<usize> {
        self.read_value(setting)
            .and_then(|v| v.as_u64())
            .and_then(|v| v.try_into().ok())
    }

    fn read_enum<T>(&self, key: &str, f: impl FnOnce(&str) -> Option<T>) -> Option<T> {
        self.content.get(key).and_then(Value::as_str).and_then(f)
    }

    fn read_fonts(&self, key: &str) -> (Option<FontFamilyName>, Option<Vec<FontFamilyName>>) {
        let Some(css_name) = self.content.get(key).and_then(Value::as_str) else {
            return (None, None);
        };
        let mut name_buffer = String::new();
        let mut quote_char: Option<char> = None;
        let mut fonts = Vec::new();
        let mut add_font = |buffer: &mut String| {
            let trimmed = buffer.trim();
            if !trimmed.is_empty() {
                fonts.push(trimmed.to_string().into());
            }
            buffer.clear();
        };
        for ch in css_name.chars() {
            match (ch, quote_char) {
                ('"' | '\'', None) => {
                    quote_char = Some(ch);
                }
                (_, Some(q)) if ch == q => {
                    quote_char = None;
                }
                (',', None) => {
                    add_font(&mut name_buffer);
                }
                _ => {
                    name_buffer.push(ch);
                }
            }
        }
        add_font(&mut name_buffer);
        if fonts.is_empty() {
            return (None, None);
        }
        (Some(fonts.remove(0)), skip_default(fonts))
    }

    pub fn settings_content(&self) -> SettingsContent {
        SettingsContent {
            project: self.project_settings_content(),
            extension: ExtensionSettingsContent::default(),
            remote: RemoteSettingsContent::default(),
            workspace: self.workspace_settings_content(),
            theme: Box::new(self.theme_settings_content()),
            editor: None,
            terminal: Some(self.terminal_settings_content()),
            mux: Some(MuxSettingsContent::default()),
            shadow_snapshot: Some(ShadowSnapshotSettingsContent::default()),
            title_bar: Some(TitleBarSettingsContent::default()),
            tab_bar: Some(self.tab_bar_settings_content()),
            tabs: None,
            preview_tabs: None,
            which_key: None,
            status_bar: Some(self.status_bar_settings_content()),
            base_keymap: Some(BaseKeymapContent::VSCode),
            hide_mouse: Some(HideMouseMode::default()),
            auto_update: Some(false),
            telemetry: Some(self.telemetry_settings_content()),
            log: Some(HashMap::default()),
            feature_flags: Some(FeatureFlagsMap(HashMap::default())),
            vim_mode: Some(false),
            line_indicator_format: Some(LineIndicatorFormat::Short),
            diagnostics: Some(DiagnosticsSettingsContent::default()),
            file_finder: Some(FileFinderSettingsContent::default()),
            project_panel: Some(self.project_panel_settings_content()),
            session: Some(SessionSettingsContent::default()),
            global_lsp_settings: Some(GlobalLspSettingsContent::default()),
            load_direnv: Some(LoadDirenv::default()),
        }
    }

    fn project_panel_settings_content(&self) -> ProjectPanelSettingsContent {
        ProjectPanelSettingsContent {
            auto_reveal_entries: self.read_bool("explorer.autoReveal"),
            auto_fold_dirs: self.read_bool("explorer.compactFolders"),
            drag_and_drop: self.read_bool("explorer.enableDragAndDrop"),
            sort_mode: self.read_enum("explorer.sortOrder", |s| match s {
                "default" | "type" => Some(ProjectPanelSortMode::DirectoriesFirst),
                "mixed" => Some(ProjectPanelSortMode::Mixed),
                "filesFirst" => Some(ProjectPanelSortMode::FilesFirst),
                _ => None,
            }),
            ..Default::default()
        }
    }

    fn project_settings_content(&self) -> ProjectSettingsContent {
        let excluded_paths = self
            .read_value("files.watcherExclude")
            .and_then(|v| v.as_array())
            .map(|v| {
                v.iter()
                    .filter_map(|n| n.as_str().map(str::to_owned).map(PathBuf::from))
                    .collect()
            });
        ProjectSettingsContent {
            linked_projects: None,
            excluded_paths,
            scan_symlinks: ScanSymlinksSetting::default(),
            private_files: None,
            hidden_files: None,
            read_only_files: None,
            file_scan_inclusions: None,
            all_languages: LanguageToSettingsMap::default(),
            disable_ai: SaturatingBool::default(),
            git_hosting_providers: Some(Vec::default()),
            terminal: None,
        }
    }

    fn workspace_settings_content(&self) -> WorkspaceSettingsContent {
        WorkspaceSettingsContent {
            window_decorations: WindowDecorations::default(),
            text_rendering_mode: TextRenderingMode::default(),
            focus_follows_mouse: FocusFollowsMouse::default(),
            confirm_quit: self
                .read_enum("window.confirmBeforeClose", |s| match s {
                    "always" | "keyboardOnly" => Some(true),
                    "never" => Some(false),
                    _ => None,
                })
                .unwrap_or(false),
            on_last_window_closed: OnLastWindowClosed::default(),
        }
    }

    fn theme_settings_content(&self) -> ThemeSettingsContent {
        let (buffer_font_family, buffer_font_fallbacks) = self.read_fonts("editor.fontFamily");
        ThemeSettingsContent {
            ui_font_size: None,
            ui_font_family: None,
            ui_font_fallbacks: None,
            ui_font_features: None,
            ui_font_weight: None,
            buffer_font_family,
            buffer_font_fallbacks,
            buffer_font_size: self.read_f32("editor.fontSize").map(FontSize::from),
            buffer_font_weight: self.read_f32("editor.fontWeight").map(FontWeightContent),
            buffer_line_height: None,
            buffer_font_features: None,
            agent_ui_font_size: None,
            agent_buffer_font_size: None,
            git_commit_buffer_font_size: None,
            markdown_preview_font_family: None,
            markdown_preview_code_font_family: None,
            markdown_preview_font_size: None,
            markdown_preview_theme: None,
            theme: None,
            icon_theme: None,
            ui_density: None,
            unnecessary_code_fade: None,
            experimental_theme_overrides: None,
            theme_overrides: HashMap::default(),
        }
    }

    fn tab_bar_settings_content(&self) -> TabBarSettingsContent {
        TabBarSettingsContent {
            show: true,
            show_nav_history_buttons: Some(true),
            show_tab_bar_buttons: Some(true),
            show_pinned_tabs_in_separate_row: Some(false),
            middle_click_to_close: true,
            mouse_scroll_to_switch: true,
            show_active_item: false,
            show_close_button: self
                .read_bool("workbench.editor.tabActionCloseVisibility")
                .map(|b| {
                    if b {
                        ShowCloseButton::Always
                    } else {
                        ShowCloseButton::Never
                    }
                })
                .unwrap_or_default(),
        }
    }

    fn status_bar_settings_content(&self) -> StatusBarSettingsContent {
        StatusBarSettingsContent {
            stack_size: false,
            working_directory: true,
            session_status: false,
            active_language_button: true,
            active_encoding_button: EncodingDisplayOptions::NonUtf8,
            cursor_position_button: true,
            line_endings_button: false,
        }
    }

    fn terminal_settings_content(&self) -> TerminalSettingsContent {
        let (font_family, font_fallbacks) = self.read_fonts("terminal.integrated.fontFamily");
        TerminalSettingsContent {
            blinking: self
                .read_bool("terminal.integrated.cursorBlinking")
                .map(|b| {
                    if b {
                        TerminalBlink::On
                    } else {
                        TerminalBlink::Off
                    }
                }),
            cursor_shape: self.read_enum("terminal.integrated.cursorStyle", |s| match s {
                "block" => Some(CursorShapeContent::Block),
                "line" => Some(CursorShapeContent::Bar),
                "underline" => Some(CursorShapeContent::Underline),
                _ => None,
            }),
            font_fallbacks,
            font_family,
            font_size: self
                .read_f32("terminal.integrated.fontSize")
                .map(FontSize::from),
            font_features: None,
            font_weight: None,
            line_height: self
                .read_f32("terminal.integrated.lineHeight")
                .map(|lh| TerminalLineHeight::Custom(lh)),
            max_scroll_history_lines: self.read_usize("terminal.integrated.scrollback"),
            bell: None,
            project: self.project_terminal_settings_content(),
            ..Default::default()
        }
    }

    fn project_terminal_settings_content(&self) -> ProjectTerminalSettingsContent {
        #[cfg(target_os = "windows")]
        let platform = "windows";
        #[cfg(target_os = "linux")]
        let platform = "linux";
        #[cfg(target_os = "macos")]
        let platform = "osx";
        #[cfg(target_os = "freebsd")]
        let platform = "freebsd";
        // VS Code only names windows/linux/osx here. The browser build runs a
        // Linux guest, so its terminal settings are the Linux ones.
        #[cfg(target_family = "wasm")]
        let platform = "linux";
        let env = self
            .read_value(&format!("terminal.integrated.env.{platform}"))
            .and_then(|v| v.as_object())
            .map(|v| {
                v.iter()
                    .map(|(k, v)| (k.clone(), v.to_string()))
                    .filter(|(_, v)| !v.contains('$'))
                    .collect::<HashMap<_, _>>()
            });
        ProjectTerminalSettingsContent {
            shell: self
                .read_string(&format!("terminal.integrated.{platform}Exec"))
                .map(|s| Shell::Program(s)),
            working_directory: None,
            env,
            ..Default::default()
        }
    }

    fn telemetry_settings_content(&self) -> TelemetrySettingsContent {
        let (metrics, diagnostics) = self
            .read_enum("telemetry.telemetryLevel", |level| {
                Some(match level {
                    "all" => (true, true),
                    "error" | "crash" => (false, true),
                    "off" => (false, false),
                    _ => (true, true),
                })
            })
            .unwrap_or((true, true));
        TelemetrySettingsContent {
            diagnostics,
            events: true,
            metrics,
        }
    }
}

fn skip_default<T: Default + PartialEq>(value: T) -> Option<T> {
    if value == T::default() {
        None
    } else {
        Some(value)
    }
}
