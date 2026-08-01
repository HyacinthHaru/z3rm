//! 设置兼容桩类型 (spec §16 Plan 16)
//!
//! 以下类型已从 settings crate 移除，在此定义以保持 workspace crate 编译。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 侧边栏位置 (原 settings::SidebarSide)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SidebarSide {
    #[default]
    Left,
    Right,
}

/// 侧边栏停靠位置 (原 settings::SidebarDockPosition)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SidebarDockPosition {
    #[default]
    Left,
    Right,
    Bottom,
    Top,
}

/// 关闭时的行为 (原 settings::ActivateOnClose)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivateOnClose {
    #[default]
    Next,
    Neighbour,
    LeftNeighbour,
    History,
    None,
}

/// 关闭按钮位置 (原 settings::ClosePosition)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClosePosition {
    #[default]
    Right,
    Left,
}

/// 诊断信息展示方式 (原 settings::ShowDiagnostics)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShowDiagnostics {
    #[default]
    Off,
    Errors,
    All,
    Inline,
    OnHover,
}

/// 种子查询设置 (原 settings::SeedQuerySetting)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeedQuerySetting {
    #[default]
    None,
    Selection,
    Line,
    Surround,
}

/// 默认打开行为 (原 settings::DefaultOpenBehavior)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DefaultOpenBehavior {
    #[default]
    ActivePane,
    FirstPane,
    NewWindow,
    ExistingWindow,
}

/// CLI 默认打开行为 (原 settings::CliDefaultOpenBehavior)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CliDefaultOpenBehavior {
    #[default]
    ActivePane,
    NewWindow,
}
/// 毫秒数包装类型
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Milliseconds(pub u64);

/// 自动保存设置 (原 settings::AutosaveSetting)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutosaveSetting {
    Never,
    AfterDelay {
        milliseconds: Milliseconds,
    },
    #[default]
    OnFocusChange,
    OnWindowChange,
    OnChange,
}

impl AutosaveSetting {
    pub fn should_save_on_close(&self) -> bool {
        matches!(
            self,
            Self::OnChange | Self::OnFocusChange | Self::AfterDelay { .. }
        )
    }
}

/// 底部停靠布局 (原 settings::BottomDockLayout)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BottomDockLayout {
    Stacked,
    SideBySide,
    #[default]
    Full,
    Contained,
    RightAligned,
    LeftAligned,
}

/// 编码显示选项 (原 settings::EncodingDisplayOptions)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EncodingDisplayOptions {
    #[default]
    NonUtf8,
    All,
    Disabled,
    Never,
}

/// 非活动面板不透明度 (原 settings::InactiveOpacity)
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InactiveOpacity(pub f32);

/// 水平分割方向 (原 settings::PaneSplitDirectionHorizontal)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirectionHorizontal {
    #[default]
    Horizontal,
    Vertical,
    Up,
    Down,
}

/// 垂直分割方向 (原 settings::PaneSplitDirectionVertical)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirectionVertical {
    #[default]
    Vertical,
    Horizontal,
    Left,
    Right,
}

/// 启动时恢复行为 (原 settings::RestoreOnStartupBehavior)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestoreOnStartupBehavior {
    Nothing,
    #[default]
    RestoreWorkspace,
    RestoreWorkspaceAndBuffers,
}

/// 居中布局填充 (原 settings::CenteredPaddingSettings)
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CenteredPaddingSettings {
    pub top: f32,
    pub bottom: f32,
    pub left_padding: f32,
    pub right_padding: f32,
}

impl CenteredPaddingSettings {
    pub const MIN_PADDING: f32 = 0.0;
    pub const MAX_PADDING: f32 = 100.0;
}

/// 居中布局设置 (原 settings::CenteredLayoutSettings)
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CenteredLayoutSettings {
    pub padding: CenteredPaddingSettings,
    pub left_padding: f32,
    pub right_padding: f32,
}

/// 关闭窗口时行为 (原 settings::CloseWindowWhenNoItems)
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CloseWindowWhenNoItems {
    #[default]
    None,
    EmptyWindow,
    AnyWindow,
}

impl CloseWindowWhenNoItems {
    pub fn should_close(&self, _: bool) -> bool {
        matches!(self, Self::EmptyWindow | Self::AnyWindow)
    }
}

/// 扩展能力内容 (原 settings::ExtensionCapabilityContent)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
pub enum ExtensionCapabilityContent {
    #[serde(rename = "process_exec")]
    ProcessExec { command: String, args: Vec<String> },
    #[serde(rename = "download_file")]
    DownloadFile { host: String, path: String },
    #[serde(rename = "npm_install_package")]
    NpmInstallPackage { package: String },
}
