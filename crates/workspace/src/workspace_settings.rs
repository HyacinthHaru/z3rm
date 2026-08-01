use std::{num::NonZeroUsize, time::Duration};

use crate::DockPosition;
pub use crate::settings_stubs::{
    ActivateOnClose, AutosaveSetting, BottomDockLayout, CenteredLayoutSettings,
    CliDefaultOpenBehavior, CloseWindowWhenNoItems, DefaultOpenBehavior, InactiveOpacity,
    PaneSplitDirectionHorizontal, PaneSplitDirectionVertical, RestoreOnStartupBehavior,
};
use collections::HashMap;
use gpui::{App, Subscription};
use serde::Deserialize;
pub use settings::{CommandAliasTarget, EncodingDisplayOptions, SettingsStore};
pub use settings::{RegisterSetting, Settings};

/// 工作区设置 (spec §16 Plan 16)
/// 原 settings 字段已大幅精简，保留向后兼容桩值
#[derive(Clone, RegisterSetting)]
pub struct WorkspaceSettings {
    pub active_pane_modifiers: ActivePanelModifiers,
    pub bottom_dock_layout: BottomDockLayout,
    pub pane_split_direction_horizontal: PaneSplitDirectionHorizontal,
    pub pane_split_direction_vertical: PaneSplitDirectionVertical,
    pub centered_layout: CenteredLayoutSettings,
    pub confirm_quit: bool,
    pub show_call_status_icon: bool,
    pub autosave: AutosaveSetting,
    pub restore_on_startup: RestoreOnStartupBehavior,
    pub cli_default_open_behavior: CliDefaultOpenBehavior,
    pub default_open_behavior: DefaultOpenBehavior,
    pub restore_on_file_reopen: bool,
    pub drop_target_size: f32,
    pub use_system_path_prompts: bool,
    pub use_system_prompts: bool,
    pub accessible_mode: bool,
    pub command_aliases: HashMap<String, CommandAliasTarget>,
    pub max_tabs: Option<NonZeroUsize>,
    pub when_closing_with_no_tabs: CloseWindowWhenNoItems,
    pub on_last_window_closed: settings::OnLastWindowClosed,
    pub text_rendering_mode: settings::TextRenderingMode,
    pub resize_all_panels_in_dock: Vec<DockPosition>,
    pub close_on_file_delete: bool,
    pub close_panel_on_toggle: bool,
    pub use_system_window_tabs: bool,
    pub zoomed_padding: bool,
    pub window_decorations: settings::WindowDecorations,
    pub focus_follows_mouse: FocusFollowsMouse,
}

/// 鼠标跟随焦点设置 (spec §16 Plan 16)
#[derive(Copy, Clone, Deserialize)]
pub struct FocusFollowsMouse {
    pub enabled: bool,
    pub debounce: Duration,
}

/// 活动面板修饰样式 (spec §16 Plan 16)
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub struct ActivePanelModifiers {
    /// 活动面板边框大小, 0 表示无边框
    pub border_size: Option<f32>,
    /// 非活动面板不透明度, 范围 [0.0, 1.0], 默认 1.0
    pub inactive_opacity: Option<InactiveOpacity>,
}

/// Tab 栏设置 (spec §16 Plan 16)
#[derive(Deserialize, RegisterSetting)]
pub struct TabBarSettings {
    /// 是否显示 Tab 栏
    pub show: bool,
    /// 中间点击关闭标签
    pub middle_click_to_close: bool,
    /// 鼠标滚轮切换标签
    pub mouse_scroll_to_switch: bool,
    /// 仅显示活动项
    pub show_active_item: bool,
    /// 关闭按钮显示方式
    pub show_close_button: settings::ShowCloseButton,
    /// 导航历史按钮 (向后兼容桩)
    pub show_nav_history_buttons: bool,
    /// Tab 栏按钮 (向后兼容桩)
    pub show_tab_bar_buttons: bool,
    /// 固定标签单独行 (向后兼容桩)
    pub show_pinned_tabs_in_separate_row: bool,
}

/// 读取 TabBarSettings 从设置内容
impl Settings for TabBarSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let tab_bar = content.tab_bar.clone().unwrap_or_default();
        TabBarSettings {
            show: tab_bar.show,
            middle_click_to_close: tab_bar.middle_click_to_close,
            mouse_scroll_to_switch: tab_bar.mouse_scroll_to_switch,
            show_active_item: tab_bar.show_active_item,
            show_close_button: tab_bar.show_close_button,
            // 向后兼容桩字段 (spec §16 Plan 16)
            show_nav_history_buttons: true,
            show_tab_bar_buttons: true,
            show_pinned_tabs_in_separate_row: false,
        }
    }
}

impl Settings for WorkspaceSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let workspace = &content.workspace;
        Self {
            // 活动面板修饰 (原 settings 字段已移除, 使用默认值)
            active_pane_modifiers: ActivePanelModifiers::default(),
            // 底部停靠布局 (原 settings 字段已移除, 使用默认值)
            bottom_dock_layout: BottomDockLayout::default(),
            // 水平分割方向 (原 settings 字段已移除, 使用默认值)
            pane_split_direction_horizontal: PaneSplitDirectionHorizontal::default(),
            // 垂直分割方向 (原 settings 字段已移除, 使用默认值)
            pane_split_direction_vertical: PaneSplitDirectionVertical::default(),
            // 居中布局 (原 settings 字段已移除, 使用默认值)
            centered_layout: CenteredLayoutSettings::default(),
            // 确认退出
            confirm_quit: workspace.confirm_quit,
            // 调用状态图标 (原 settings 字段已移除, 使用默认值)
            show_call_status_icon: false,
            // 自动保存 (原 settings 字段已移除, 使用默认值)
            autosave: AutosaveSetting::default(),
            // 启动恢复 (原 settings 字段已移除, 使用默认值)
            restore_on_startup: RestoreOnStartupBehavior::default(),
            // CLI 默认打开行为 (原 settings 字段已移除, 使用默认值)
            cli_default_open_behavior: CliDefaultOpenBehavior::default(),
            // 默认打开行为 (原 settings 字段已移除, 使用默认值)
            default_open_behavior: DefaultOpenBehavior::default(),
            // 文件重开恢复 (原 settings 字段已移除, 使用默认值)
            restore_on_file_reopen: true,
            // 拖放目标大小 (原 settings 字段已移除, 使用默认值)
            drop_target_size: 20.0,
            // 系统路径提示 (原 settings 字段已移除, 使用默认值)
            use_system_path_prompts: false,
            // 系统提示 (原 settings 字段已移除, 使用默认值)
            use_system_prompts: false,
            // 无障碍模式 (原 settings 字段已移除, 使用默认值)
            accessible_mode: false,
            // 命令别名 (原 settings 字段已移除, 使用空默认值)
            command_aliases: HashMap::default(),
            // 最大标签数 (原 settings 字段已移除, 使用默认值)
            max_tabs: None,
            // 关闭无标签窗口 (原 settings 字段已移除, 使用默认值)
            when_closing_with_no_tabs: CloseWindowWhenNoItems::default(),
            // 关闭窗口行为
            on_last_window_closed: workspace.on_last_window_closed,
            // 文本渲染模式
            text_rendering_mode: workspace.text_rendering_mode,
            // 停靠面板缩放 (原 settings 字段已移除, 使用默认值)
            resize_all_panels_in_dock: Vec::new(),
            // 文件删除关闭 (原 settings 字段已移除, 使用默认值)
            close_on_file_delete: true,
            // 切换面板关闭 (原 settings 字段已移除, 使用默认值)
            close_panel_on_toggle: false,
            // 系统窗口标签 (原 settings 字段已移除, 使用默认值)
            use_system_window_tabs: false,
            // 缩放填充 (原 settings 字段已移除, 使用默认值)
            zoomed_padding: true,
            // 窗口装饰
            window_decorations: workspace.window_decorations.clone(),
            // 鼠标跟随焦点
            focus_follows_mouse: FocusFollowsMouse {
                enabled: workspace.focus_follows_mouse.enabled,
                // debounce_ms 字段已从 settings::FocusFollowsMouse 移除 (spec §16 Plan 16)
                debounce: Duration::from_millis(250),
            },
        }
    }
}

/// 无障碍模式访问 trait
pub trait AccessibleMode {
    fn accessible_mode(&self) -> bool;
}

impl AccessibleMode for App {
    fn accessible_mode(&self) -> bool {
        WorkspaceSettings::get_global(self).accessible_mode
    }
}

/// 观察无障碍模式变化
pub fn observe_accessible_mode(
    cx: &mut App,
    mut callback: impl FnMut(bool, &mut App) + 'static,
) -> Subscription {
    let mut last_accessible_mode = WorkspaceSettings::get_global(cx).accessible_mode;
    cx.observe_global::<SettingsStore>(move |cx| {
        let accessible_mode = WorkspaceSettings::get_global(cx).accessible_mode;
        if accessible_mode != last_accessible_mode {
            last_accessible_mode = accessible_mode;
            callback(accessible_mode, cx);
        }
    })
}

/// 状态栏设置 (spec §16 Plan 16)
#[derive(Deserialize, RegisterSetting)]
pub struct StatusBarSettings {
    /// 是否显示状态栏
    pub show: bool,
    /// 堆栈大小显示
    pub stack_size: bool,
    /// 工作目录显示
    pub working_directory: bool,
    /// 会话状态显示
    pub session_status: bool,
    /// 当前语言按钮
    pub active_language_button: bool,
    /// 当前编码按钮 (原 EncodingDisplayOptions)
    pub active_encoding_button: EncodingDisplayOptions,
    /// 光标位置按钮
    pub cursor_position_button: bool,
    /// 行尾按钮
    pub line_endings_button: bool,
}

impl Settings for StatusBarSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let status_bar = content.status_bar.clone().unwrap_or_default();
        StatusBarSettings {
            show: true, // 状态栏默认显示
            stack_size: status_bar.stack_size,
            working_directory: status_bar.working_directory,
            session_status: status_bar.session_status,
            active_language_button: status_bar.active_language_button,
            active_encoding_button: status_bar.active_encoding_button,
            cursor_position_button: status_bar.cursor_position_button,
            line_endings_button: status_bar.line_endings_button,
        }
    }
}
