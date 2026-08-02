use collections::HashMap;
use extension::{
    DownloadFileCapability, ExtensionCapability, NpmInstallPackageCapability, ProcessExecCapability,
};
use settings::{ExtensionCapabilityContent, RegisterSetting, Settings};
use std::sync::Arc;

/// 扩展设置 (spec §16 Plan 16)
#[derive(Debug, Default, Clone, RegisterSetting)]
pub struct ExtensionSettings {
    /// 自动安装的扩展
    pub auto_install_extensions: HashMap<Arc<str>, bool>,
    /// 自动更新的扩展
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    /// 已授予的能力
    pub granted_capabilities: Vec<ExtensionCapability>,
}

impl ExtensionSettings {
    /// 判断是否应该自动安装指定扩展
    pub fn should_auto_install(&self, extension_id: &str) -> bool {
        self.auto_install_extensions
            .get(extension_id)
            .copied()
            .unwrap_or(true)
    }

    /// 判断是否应该自动更新指定扩展
    pub fn should_auto_update(&self, extension_id: &str) -> bool {
        self.auto_update_extensions
            .get(extension_id)
            .copied()
            .unwrap_or(true)
    }
}

impl Settings for ExtensionSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let extension = &content.extension;
        Self {
            auto_install_extensions: extension.auto_install_extensions.clone(),
            auto_update_extensions: extension.auto_update_extensions.clone(),
            granted_capabilities: extension
                .granted_capabilities
                .iter()
                .map(capability_from_content)
                .collect(),
        }
    }
}

fn capability_from_content(content: &ExtensionCapabilityContent) -> ExtensionCapability {
    match content {
        ExtensionCapabilityContent::ProcessExec { command, args } => {
            ExtensionCapability::ProcessExec(ProcessExecCapability {
                command: command.clone(),
                args: args.clone(),
            })
        }
        ExtensionCapabilityContent::DownloadFile { host, path } => {
            ExtensionCapability::DownloadFile(DownloadFileCapability {
                host: host.clone(),
                path: path.clone(),
            })
        }
        ExtensionCapabilityContent::NpmInstallPackage { package } => {
            ExtensionCapability::NpmInstallPackage(NpmInstallPackageCapability {
                package: package.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::SettingsContent;

    fn content_with_extension_settings() -> SettingsContent {
        let mut content = SettingsContent::default();
        content.extension.auto_install_extensions =
            HashMap::from_iter([(Arc::from("html"), true), (Arc::from("toml"), false)]);
        content.extension.auto_update_extensions = HashMap::from_iter([(Arc::from("html"), false)]);
        content.extension.granted_capabilities = vec![
            ExtensionCapabilityContent::ProcessExec {
                command: "ls".to_string(),
                args: vec!["-la".to_string()],
            },
            ExtensionCapabilityContent::DownloadFile {
                host: "github.com".to_string(),
                path: vec!["**".to_string()],
            },
            ExtensionCapabilityContent::NpmInstallPackage {
                package: "typescript".to_string(),
            },
        ];
        content
    }

    #[test]
    fn test_reads_auto_install_and_auto_update_from_content() {
        let settings = ExtensionSettings::from_settings(&content_with_extension_settings());

        assert!(settings.should_auto_install("html"));
        assert!(!settings.should_auto_install("toml"));
        assert!(!settings.should_auto_update("html"));
        // Extensions the user never mentioned keep the opt-out default.
        assert!(settings.should_auto_update("toml"));
    }

    #[test]
    fn test_reads_granted_capabilities_from_content() {
        let settings = ExtensionSettings::from_settings(&content_with_extension_settings());

        assert_eq!(
            settings.granted_capabilities,
            vec![
                ExtensionCapability::ProcessExec(ProcessExecCapability {
                    command: "ls".to_string(),
                    args: vec!["-la".to_string()],
                }),
                ExtensionCapability::DownloadFile(DownloadFileCapability {
                    host: "github.com".to_string(),
                    path: vec!["**".to_string()],
                }),
                ExtensionCapability::NpmInstallPackage(NpmInstallPackageCapability {
                    package: "typescript".to_string(),
                }),
            ]
        );
    }

    #[test]
    fn test_defaults_to_no_capabilities_and_no_managed_extensions() {
        let settings = ExtensionSettings::from_settings(&SettingsContent::default());

        assert!(settings.auto_install_extensions.is_empty());
        assert!(settings.auto_update_extensions.is_empty());
        assert!(settings.granted_capabilities.is_empty());
    }

    /// Guards the JSON shape written in `assets/settings/default.json`: the
    /// capability entries have to deserialize into the same representation that
    /// `extension::ExtensionCapability` serializes to.
    #[test]
    fn test_parses_extension_settings_from_json() {
        let content: SettingsContent = settings::parse_json_with_comments(
            r#"{
                "extension": {
                    "auto_install_extensions": { "html": false },
                    "auto_update_extensions": { "html": false },
                    "granted_capabilities": [
                        { "kind": "process:exec", "command": "*", "args": ["**"] },
                        { "kind": "download_file", "host": "*", "path": ["**"] },
                        { "kind": "npm:install", "package": "*" }
                    ]
                }
            }"#,
        )
        .expect("extension settings should parse");

        let settings = ExtensionSettings::from_settings(&content);

        assert!(!settings.should_auto_install("html"));
        assert!(!settings.should_auto_update("html"));
        assert_eq!(
            settings.granted_capabilities,
            vec![
                ExtensionCapability::ProcessExec(ProcessExecCapability {
                    command: "*".to_string(),
                    args: vec!["**".to_string()],
                }),
                ExtensionCapability::DownloadFile(DownloadFileCapability {
                    host: "*".to_string(),
                    path: vec!["**".to_string()],
                }),
                ExtensionCapability::NpmInstallPackage(NpmInstallPackageCapability {
                    package: "*".to_string(),
                }),
            ]
        );
    }
}
