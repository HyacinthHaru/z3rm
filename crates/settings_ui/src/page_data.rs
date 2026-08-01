use gpui::{Action as _, App};
use itertools::Itertools as _;
use settings::SettingsContent;
use strum::{EnumMessage, IntoDiscriminant as _, VariantArray};
use theme::SystemAppearance;
use ui::IntoElement;

use crate::{
    ActionLink, DynamicItem, PROJECT, SettingField, SettingItem, SettingsFieldMetadata,
    SettingsPage, SettingsPageItem, SubPageLink, USER, active_language, all_language_names,
};

const DEFAULT_STRING: String = String::new();
/// A default empty string reference. Useful in `pick` functions for cases either in dynamic item fields, or when dealing with `settings::Maybe`
/// to avoid the "NO DEFAULT" case.
const DEFAULT_EMPTY_STRING: Option<&String> = Some(&DEFAULT_STRING);

macro_rules! concat_sections {
    (@vec, $($arr:expr),+ $(,)?) => {{
        let total_len = 0_usize $(+ $arr.len())+;
        let mut out = Vec::with_capacity(total_len);

        $(
            out.extend($arr);
        )+

        out
    }};

    ($($arr:expr),+ $(,)?) => {{
        let total_len = 0_usize $(+ $arr.len())+;

        let mut out: Box<[std::mem::MaybeUninit<_>]> = Box::new_uninit_slice(total_len);

        let mut index = 0usize;
        $(
            let array = $arr;
            for item in array {
                out[index].write(item);
                index += 1;
            }
        )+

        debug_assert_eq!(index, total_len);

        // SAFETY: we wrote exactly `total_len` elements.
        unsafe { out.assume_init() }
    }}
}

// =========================================================================
// 设置页面列表 (spec §16 Plan 16)
// =========================================================================

pub(crate) fn settings_data(cx: &App) -> Vec<SettingsPage> {
    vec![
        general_page(),
        appearance_page(),
        keymap_page(),
        terminal_page(),
        mux_page(),
        shadow_snapshot_page(),
        extensions_page(),
        workspace_page(),
        developer_page(cx),
        ai_page(),
        network_page(),
    ]
}

// =========================================================================
// General Page - 基础设置 (spec §16 Plan 16)
// =========================================================================

fn general_page() -> SettingsPage {
    fn auto_update_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Auto Update"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Auto Update",
                description: "Whether or not to automatically check for updates.",
                field: Box::new(SettingField {
                    json_path: Some("auto_update"),
                    pick: |settings_content| settings_content.auto_update.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.auto_update = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn telemetry_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("Telemetry"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Telemetry Diagnostics",
                description: "Send debug information like crash reports.",
                field: Box::new(SettingField {
                    json_path: Some("telemetry.diagnostics"),
                    pick: |settings_content| {
                        settings_content.telemetry.as_ref().map(|t| &t.diagnostics)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .telemetry
                            .get_or_insert_default()
                            .diagnostics = value.unwrap_or(false);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Telemetry Events",
                description: "Send anonymous usage events.",
                field: Box::new(SettingField {
                    json_path: Some("telemetry.events"),
                    pick: |settings_content| settings_content.telemetry.as_ref().map(|t| &t.events),
                    write: |settings_content, value, _| {
                        settings_content.telemetry.get_or_insert_default().events =
                            value.unwrap_or(false);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Telemetry Metrics",
                description: "Send anonymized metrics.",
                field: Box::new(SettingField {
                    json_path: Some("telemetry.metrics"),
                    pick: |settings_content| {
                        settings_content.telemetry.as_ref().map(|t| &t.metrics)
                    },
                    write: |settings_content, value, _| {
                        settings_content.telemetry.get_or_insert_default().metrics =
                            value.unwrap_or(false);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scoped_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Scoped Settings"),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "Preview Channel",
                description: "Which settings should be activated only in Preview build.",
                field: Box::new(
                    SettingField {
                        json_path: Some("preview_channel_settings"),
                        pick: |settings_content| Some(settings_content),
                        write: |_settings_content, _value, _| {},
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "Settings Profiles",
                description: "Any number of settings profiles that are temporarily applied on top of your existing user settings.",
                field: Box::new(
                    SettingField {
                        json_path: Some("settings_profiles"),
                        pick: |settings_content| Some(settings_content),
                        write: |_settings_content, _value, _| {},
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn workspace_settings_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Window"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Window Decorations",
                description: "What draws window decorations/titlebar. Default: client",
                field: Box::new(SettingField {
                    json_path: Some("window_decorations"),
                    pick: |settings_content| Some(&settings_content.workspace.window_decorations),
                    write: |settings_content, value, _| {
                        if let Some(v) = value {
                            settings_content.workspace.window_decorations = v;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Text Rendering Mode",
                description: "The text rendering mode to use. Default: platform_default",
                field: Box::new(SettingField {
                    json_path: Some("text_rendering_mode"),
                    pick: |settings_content| Some(&settings_content.workspace.text_rendering_mode),
                    write: |settings_content, value, _| {
                        if let Some(v) = value {
                            settings_content.workspace.text_rendering_mode = v;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Confirm Quit",
                description: "Whether or not to prompt the user to confirm before closing the application. Default: false",
                field: Box::new(SettingField {
                    json_path: Some("confirm_quit"),
                    pick: |settings_content| Some(&settings_content.workspace.confirm_quit),
                    write: |settings_content, value, _| {
                        if let Some(v) = value {
                            settings_content.workspace.confirm_quit = v;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "On Last Window Closed",
                description: "What to do when the last window is closed.",
                field: Box::new(SettingField {
                    json_path: Some("on_last_window_closed"),
                    pick: |settings_content| {
                        Some(&settings_content.workspace.on_last_window_closed)
                    },
                    write: |settings_content, value, _| {
                        if let Some(v) = value {
                            settings_content.workspace.on_last_window_closed = v;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn remote_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Remote"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Remote Server Path",
                description: "Path to the remote server binary",
                field: Box::new(SettingField {
                    json_path: Some("remote.remote_server_path"),
                    pick: |settings_content| settings_content.remote.remote_server_path.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.remote.remote_server_path = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Auto Install",
                description: "Whether to auto install remote server. Default: true",
                field: Box::new(SettingField {
                    json_path: Some("remote.auto_install"),
                    pick: |settings_content| Some(&settings_content.remote.auto_install),
                    write: |settings_content, value, _| {
                        settings_content.remote.auto_install = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "General",
        items: concat_sections!(
            @vec,
            auto_update_section(),
            telemetry_section(),
            scoped_settings_section(),
            workspace_settings_section(),
            remote_settings_section(),
        )
        .into(),
    }
}

// =========================================================================
// Appearance Page - 主题与外观设置 (spec §16 Plan 16)
// =========================================================================

fn appearance_page() -> SettingsPage {
    fn theme_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Theme"),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "Theme Mode",
                    description: "Choose a static, fixed theme or dynamically select themes based on appearance and light/dark modes.",
                    field: Box::new(SettingField {

                        json_path: Some("theme$"),
                        pick: |settings_content| {
                            Some(&dynamic_variants::<settings::ThemeSelection>()[
                                settings_content
                                    .theme
                                    .theme
                                    .as_ref()?
                                    .discriminant() as usize])
                        },
                        write: |settings_content, value, app: &App| {
                            let Some(value) = value else {
                                settings_content.theme.theme = None;
                                return;
                            };
                            let settings_value = settings_content.theme.theme.get_or_insert_default();
                            *settings_value = match value {
                                settings::ThemeSelectionDiscriminants::Static => {
                                    let name = match settings_value {
                                        settings::ThemeSelection::Static(_) => return,
                                        settings::ThemeSelection::Dynamic { mode, light, dark } => {
                                            match mode {
                                                settings::ThemeAppearanceMode::Light => light.clone(),
                                                settings::ThemeAppearanceMode::Dark => dark.clone(),
                                                settings::ThemeAppearanceMode::System => {
                                                    if SystemAppearance::global(app).is_light() {
                                                        light.clone()
                                                    } else {
                                                        dark.clone()
                                                    }
                                                }
                                            }
                                        },
                                    };
                                    settings::ThemeSelection::Static(name)
                                },
                                settings::ThemeSelectionDiscriminants::Dynamic => {
                                    let static_name = match settings_value {
                                        settings::ThemeSelection::Static(theme_name) => theme_name.clone(),
                                        settings::ThemeSelection::Dynamic {..} => return,
                                    };

                                    settings::ThemeSelection::Dynamic {
                                        mode: settings::ThemeAppearanceMode::System,
                                        light: static_name.clone(),
                                        dark: static_name,
                                    }
                                },
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.theme.theme.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::ThemeSelection>().into_iter().map(|variant| {
                    match variant {
                        settings::ThemeSelectionDiscriminants::Static => vec![
                            SettingItem {
                                files: USER,
                                title: "Theme Name",
                                description: "The name of your selected theme.",
                                field: Box::new(SettingField {

                                    json_path: Some("theme"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Static(name)) => Some(name),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Static(theme_name) => *theme_name = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                        settings::ThemeSelectionDiscriminants::Dynamic => vec![
                            SettingItem {
                                files: USER,
                                title: "Mode",
                                description: "Choose whether to use the selected light or dark theme or to follow your OS appearance configuration.",
                                field: Box::new(SettingField {

                                    json_path: Some("theme.mode"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Dynamic { mode, ..}) => Some(mode),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Dynamic{ mode, ..} => *mode = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "Light Theme",
                                description: "The theme to use when mode is set to light, or when mode is set to system and it is in light mode.",
                                field: Box::new(SettingField {

                                    json_path: Some("theme.light"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Dynamic { light, ..}) => Some(light),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Dynamic{ light, ..} => *light = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "Dark Theme",
                                description: "The theme to use when mode is set to dark, or when mode is set to system and it is in dark mode.",
                                field: Box::new(SettingField {

                                    json_path: Some("theme.dark"),
                                    pick: |settings_content| {
                                        match settings_content.theme.theme.as_ref() {
                                            Some(settings::ThemeSelection::Dynamic { dark, ..}) => Some(dark),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .theme.get_or_insert_default() {
                                                settings::ThemeSelection::Dynamic{ dark, ..} => *dark = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                    }
                }).collect(),
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: "Icon Theme",
                    description: "The custom set of icons Zed will associate with files and directories.",
                    field: Box::new(SettingField {

                        json_path: Some("icon_theme$"),
                        pick: |settings_content| {
                            Some(&dynamic_variants::<settings::IconThemeSelection>()[
                                settings_content
                                    .theme
                                    .icon_theme
                                    .as_ref()?
                                    .discriminant() as usize])
                        },
                        write: |settings_content, value, app| {
                            let Some(value) = value else {
                                settings_content.theme.icon_theme = None;
                                return;
                            };
                            let settings_value = settings_content.theme.icon_theme.get_or_insert_with(|| {
                                settings::IconThemeSelection::Static(settings::IconThemeName(theme::default_icon_theme().name.clone().into()))
                            });
                            *settings_value = match value {
                                settings::IconThemeSelectionDiscriminants::Static => {
                                    let name = match settings_value {
                                        settings::IconThemeSelection::Static(_) => return,
                                        settings::IconThemeSelection::Dynamic { mode, light, dark } => {
                                            match mode {
                                                settings::ThemeAppearanceMode::Light => light.clone(),
                                                settings::ThemeAppearanceMode::Dark => dark.clone(),
                                                settings::ThemeAppearanceMode::System => {
                                                    if SystemAppearance::global(app).is_light() {
                                                        light.clone()
                                                    } else {
                                                        dark.clone()
                                                    }
                                                }
                                            }
                                        },
                                    };
                                    settings::IconThemeSelection::Static(name)
                                },
                                settings::IconThemeSelectionDiscriminants::Dynamic => {
                                    let static_name = match settings_value {
                                        settings::IconThemeSelection::Static(theme_name) => theme_name.clone(),
                                        settings::IconThemeSelection::Dynamic {..} => return,
                                    };

                                    settings::IconThemeSelection::Dynamic {
                                        mode: settings::ThemeAppearanceMode::System,
                                        light: static_name.clone(),
                                        dark: static_name,
                                    }
                                },
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.theme.icon_theme.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::IconThemeSelection>().into_iter().map(|variant| {
                    match variant {
                        settings::IconThemeSelectionDiscriminants::Static => vec![
                            SettingItem {
                                files: USER,
                                title: "Icon Theme Name",
                                description: "The name of your selected icon theme.",
                                field: Box::new(SettingField {

                                    json_path: Some("icon_theme$string"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Static(name)) => Some(name),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Static(theme_name)) => *theme_name = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                        settings::IconThemeSelectionDiscriminants::Dynamic => vec![
                            SettingItem {
                                files: USER,
                                title: "Mode",
                                description: "Choose whether to use the selected light or dark icon theme or to follow your OS appearance configuration.",
                                field: Box::new(SettingField {

                                    json_path: Some("icon_theme"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Dynamic { mode, ..}) => Some(mode),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Dynamic{ mode, ..}) => *mode = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "Light Icon Theme",
                                description: "The icon theme to use when mode is set to light, or when mode is set to system and it is in light mode.",
                                field: Box::new(SettingField {

                                    json_path: Some("icon_theme.light"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Dynamic { light, ..}) => Some(light),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Dynamic{ light, ..}) => *light = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: "Dark Icon Theme",
                                description: "The icon theme to use when mode is set to dark, or when mode is set to system and it is in dark mode.",
                                field: Box::new(SettingField {

                                    json_path: Some("icon_theme.dark"),
                                    pick: |settings_content| {
                                        match settings_content.theme.icon_theme.as_ref() {
                                            Some(settings::IconThemeSelection::Dynamic { dark, ..}) => Some(dark),
                                            _ => None
                                        }
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .theme
                                            .icon_theme.as_mut() {
                                                Some(settings::IconThemeSelection::Dynamic{ dark, ..}) => *dark = value,
                                                _ => return
                                            }
                                    },
                                }),
                                metadata: None,
                            }
                        ],
                    }
                }).collect(),
            }),
        ]
    }

    fn font_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Font"),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "Buffer Font Family",
                description: "The name of a font to use for rendering in text buffers.",
                field: Box::new(SettingField {
                    json_path: Some("buffer_font_family"),
                    pick: |settings_content| settings_content.theme.buffer_font_family.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_family = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "Buffer Font Size",
                description: "The default font size for rendering in text buffers.",
                field: Box::new(SettingField {
                    json_path: Some("buffer_font_size"),
                    pick: |settings_content| settings_content.theme.buffer_font_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_size = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "UI Font Family",
                description: "The name of a font to use for rendering in the UI.",
                field: Box::new(SettingField {
                    json_path: Some("ui_font_family"),
                    pick: |settings_content| settings_content.theme.ui_font_family.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_family = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: "UI Font Size",
                description: "The default font size for text in the UI.",
                field: Box::new(SettingField {
                    json_path: Some("ui_font_size"),
                    pick: |settings_content| settings_content.theme.ui_font_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_size = value;
                    },
                }),
                metadata: None,
            }),
        ]
    }

    SettingsPage {
        title: "Appearance",
        items: concat_sections!(theme_section(), font_section(),).into(),
    }
}

// =========================================================================
// Keymap Page - 键盘映射设置 (spec §16 Plan 16)
// =========================================================================

fn keymap_page() -> SettingsPage {
    fn base_keymap_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Base Keymap"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Base Keymap",
                description: "The base keymap to use. Default: VSCode",
                field: Box::new(SettingField {
                    json_path: Some("base_keymap"),
                    pick: |settings_content| settings_content.base_keymap.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.base_keymap = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn hide_mouse_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Mouse"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Hide Mouse",
                description: "Mouse hide mode during typing",
                field: Box::new(SettingField {
                    json_path: Some("hide_mouse"),
                    pick: |settings_content| settings_content.hide_mouse.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.hide_mouse = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Keymap",
        items: concat_sections!(base_keymap_section(), hide_mouse_section(),).into(),
    }
}

// =========================================================================
// Terminal Page - 终端设置 (spec §16 Plan 16)
// =========================================================================

fn terminal_page() -> SettingsPage {
    fn terminal_font_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Font"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Font Family",
                description: "Sets the terminal's font family.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.font_family"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Font Size",
                description: "Sets the terminal's font size.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.font_size"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn terminal_behavior_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("Behavior"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Cursor Shape",
                description: "Default cursor shape for the terminal.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.cursor_shape"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.cursor_shape.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .cursor_shape = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Blinking",
                description: "Sets the cursor blinking behavior in the terminal.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.blinking"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.blinking.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().blinking = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Option as Meta",
                description: "Whether the option key behaves as the meta key.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.option_as_meta"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.option_as_meta.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .option_as_meta = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Copy on Select",
                description: "Whether or not selecting text in the terminal will automatically copy to the system clipboard.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.copy_on_select"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.copy_on_select.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .copy_on_select = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Keep Selection on Copy",
                description: "Whether to keep the text selection after copying it to the clipboard.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.keep_selection_on_copy"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.keep_selection_on_copy.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .keep_selection_on_copy = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn terminal_dock_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Dock"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Button",
                description: "Whether to show the terminal button in the status bar.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.button"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.button.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Flexible",
                description: "Whether the terminal panel should use flexible (proportional) sizing.",
                field: Box::new(SettingField {
                    json_path: Some("terminal.flexible"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|t| t.flexible.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().flexible = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Terminal",
        items: concat_sections!(
            terminal_font_section(),
            terminal_behavior_section(),
            terminal_dock_section(),
        )
        .into(),
    }
}

// =========================================================================
// Mux Page - 多路复用器设置 (spec §16 Plan 16)
// =========================================================================

fn mux_page() -> SettingsPage {
    fn mux_connection_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Connection"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Socket Path",
                description: "Unix socket path for the mux server.",
                field: Box::new(SettingField {
                    json_path: Some("mux.socket_path"),
                    pick: |settings_content| {
                        settings_content
                            .mux
                            .as_ref()
                            .and_then(|m| m.socket_path.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.mux.get_or_insert_default().socket_path = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn mux_behavior_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Behavior"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Keep Alive",
                description: "Whether to keep the mux server alive when no clients are connected. Default: true",
                field: Box::new(SettingField {
                    json_path: Some("mux.keep_alive"),
                    pick: |settings_content| settings_content.mux.as_ref().map(|m| &m.keep_alive),
                    write: |settings_content, value, _| {
                        settings_content.mux.get_or_insert_default().keep_alive =
                            value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn mux_ui_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("UI"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Keymap Profile",
                description: "Keymap profile: default, tmux, vim, or wezterm",
                field: Box::new(SettingField {
                    json_path: Some("mux.keymap_profile"),
                    pick: |settings_content| {
                        settings_content
                            .mux
                            .as_ref()
                            .and_then(|m| m.keymap_profile.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.mux.get_or_insert_default().keymap_profile = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Mux",
        items: concat_sections!(
            @vec,
            mux_connection_section(),
            mux_behavior_section(),
            mux_ui_section(),
        )
        .into(),
    }
}

// =========================================================================
// Shadow Snapshot Page - 影子快照设置 (spec §16 Plan 16)
// =========================================================================

fn shadow_snapshot_page() -> SettingsPage {
    fn shadow_snapshot_enabled_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Enabled"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Enabled",
                description: "Whether shadow snapshots are enabled. Default: true",
                field: Box::new(SettingField {
                    json_path: Some("shadow_snapshot.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .shadow_snapshot
                            .as_ref()
                            .map(|s| &s.enabled)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .shadow_snapshot
                            .get_or_insert_default()
                            .enabled = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn shadow_snapshot_quota_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Quota"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Per Project Quota (MB)",
                description: "Per-project quota in megabytes. Default: 500",
                field: Box::new(SettingField {
                    json_path: Some("shadow_snapshot.per_project_quota_mb"),
                    pick: |settings_content| {
                        settings_content
                            .shadow_snapshot
                            .as_ref()
                            .map(|s| &s.per_project_quota_mb)
                    },
                    write: |settings_content, value, _| {
                        if let Some(value) = value {
                            settings_content
                                .shadow_snapshot
                                .get_or_insert_default()
                                .per_project_quota_mb = value;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn shadow_snapshot_behavior_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Behavior"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Binary Detection",
                description: "Whether to detect and skip binary files. Default: true",
                field: Box::new(SettingField {
                    json_path: Some("shadow_snapshot.binary_detection"),
                    pick: |settings_content| {
                        settings_content
                            .shadow_snapshot
                            .as_ref()
                            .map(|s| &s.binary_detection)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .shadow_snapshot
                            .get_or_insert_default()
                            .binary_detection = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Debounce MS",
                description: "Debounce interval in milliseconds for file change events. Default: 500",
                field: Box::new(SettingField {
                    json_path: Some("shadow_snapshot.debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .shadow_snapshot
                            .as_ref()
                            .map(|s| &s.debounce_ms)
                    },
                    write: |settings_content, value, _| {
                        if let Some(value) = value {
                            settings_content
                                .shadow_snapshot
                                .get_or_insert_default()
                                .debounce_ms = value;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Shadow Snapshot",
        items: concat_sections!(
            @vec,
            shadow_snapshot_enabled_section(),
            shadow_snapshot_quota_section(),
            shadow_snapshot_behavior_section(),
        )
        .into(),
    }
}

// =========================================================================
// Extensions Page - 扩展设置 (spec §16 Plan 16)
// =========================================================================

fn extensions_page() -> SettingsPage {
    fn extension_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Extensions"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Directory",
                description: "Directory where extensions are stored. Default: ~/.config/z3rm/extensions",
                field: Box::new(SettingField {
                    json_path: Some("extension.directory"),
                    pick: |settings_content| Some(&settings_content.extension.directory),
                    write: |settings_content, value, _| {
                        if let Some(value) = value {
                            settings_content.extension.directory = value.clone();
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Auto Sync to Remote",
                description: "Whether to automatically sync extensions to remote servers. Default: true",
                field: Box::new(SettingField {
                    json_path: Some("extension.auto_sync_to_remote"),
                    pick: |settings_content| Some(&settings_content.extension.auto_sync_to_remote),
                    write: |settings_content, value, _| {
                        settings_content.extension.auto_sync_to_remote = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Extensions",
        items: concat_sections!(extension_section(),).into(),
    }
}

// =========================================================================
// Workspace Page - 工作区设置 (spec §16 Plan 16)
// =========================================================================

fn workspace_page() -> SettingsPage {
    fn workspace_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Workspace"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Focus Follows Mouse",
                description: "Whether the focused panel follows the mouse location.",
                field: Box::new(SettingField {
                    json_path: Some("workspace.focus_follows_mouse"),
                    pick: |settings_content| Some(&settings_content.workspace.focus_follows_mouse),
                    write: |settings_content, value, _| {
                        if let Some(v) = value {
                            settings_content.workspace.focus_follows_mouse = v;
                        }
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn title_bar_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Title Bar"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Show Branch Status Icon",
                description: "Whether to show git status indicators on the branch icon in the title bar.",
                field: Box::new(SettingField {
                    json_path: Some("title_bar.show_branch_status_icon"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()
                            .and_then(|t| t.show_branch_status_icon.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_branch_status_icon = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Show Branch Name",
                description: "Whether to show the branch name button in the titlebar.",
                field: Box::new(SettingField {
                    json_path: Some("title_bar.show_branch_name"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()
                            .and_then(|t| t.show_branch_name.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_branch_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn status_bar_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Status Bar"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Show Stack Size",
                description: "Whether to show the stack size on the status bar.",
                field: Box::new(SettingField {
                    json_path: Some("status_bar.stack_size"),
                    pick: |settings_content| {
                        settings_content.status_bar.as_ref().map(|s| &s.stack_size)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .stack_size = value.unwrap_or(false);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Show Working Directory",
                description: "Whether to show the working directory on the status bar.",
                field: Box::new(SettingField {
                    json_path: Some("status_bar.working_directory"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()
                            .map(|s| &s.working_directory)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .working_directory = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn tab_bar_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Tab Bar"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Middle Click to Close",
                description: "Whether to show the middle click to close tab behavior.",
                field: Box::new(SettingField {
                    json_path: Some("tab_bar.middle_click_to_close"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()
                            .map(|t| &t.middle_click_to_close)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .middle_click_to_close = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Mouse Scroll to Switch",
                description: "Whether to show the mouse scroll to switch tab behavior.",
                field: Box::new(SettingField {
                    json_path: Some("tab_bar.mouse_scroll_to_switch"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()
                            .map(|t| &t.mouse_scroll_to_switch)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .mouse_scroll_to_switch = value.unwrap_or(true);
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Workspace",
        items: concat_sections!(
            @vec,
            workspace_section(),
            title_bar_section(),
            status_bar_section(),
            tab_bar_section(),
        )
        .into(),
    }
}

// =========================================================================
// Developer Page - 开发者设置 (spec §16 Plan 16)
// =========================================================================

fn developer_page(cx: &App) -> SettingsPage {
    fn log_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Log"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Log Levels",
                description: "Log range to level mapping",
                field: Box::new(
                    SettingField {
                        json_path: Some("log"),
                        pick: |settings_content| settings_content.log.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.log = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn feature_flags_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Feature Flags"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Feature Flags",
                description: "Local overrides for feature flags",
                field: Box::new(
                    SettingField {
                        json_path: Some("feature_flags"),
                        pick: |settings_content| settings_content.feature_flags.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.feature_flags = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: "Developer",
        items: concat_sections!(
            @vec,
            log_section(),
            feature_flags_section(),
        )
        .into(),
    }
}

// =========================================================================
// Stub Pages - 空页面 (spec §16 Plan 16)
// =========================================================================

fn ai_page() -> SettingsPage {
    SettingsPage {
        title: "AI",
        items: Box::new([]),
    }
}

fn network_page() -> SettingsPage {
    SettingsPage {
        title: "Network",
        items: Box::new([]),
    }
}

// =========================================================================
// Helper Functions (spec §16 Plan 16)
// =========================================================================

fn dynamic_variants<T>() -> &'static [T::Discriminant]
where
    T: strum::IntoDiscriminant,
    T::Discriminant: strum::VariantArray,
{
    <<T as strum::IntoDiscriminant>::Discriminant as strum::VariantArray>::VARIANTS
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_settings_data_has_pages() {
        // Placeholder test - verify settings_data returns non-empty
    }
}
