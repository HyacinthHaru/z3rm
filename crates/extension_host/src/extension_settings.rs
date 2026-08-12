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
    use crate::capability_granter::CapabilityGranter;
    use extension::ExtensionManifest;
    use settings::SettingsContent;
    use url::Url;

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

    /// Guards the JSON shape shipped in `assets/settings/default.json`: the
    /// `extension` section has to parse, and the shipped `granted_capabilities`
    /// must stay an empty allowlist matching the Rust fallback, so no privileged
    /// operation is allowed without an explicit user grant.
    #[test]
    fn test_default_json_ships_no_capability_grants() {
        let content: SettingsContent = settings::parse_json_with_comments(
            settings::default_settings().as_ref(),
        )
        .expect("assets/settings/default.json should parse");

        let settings = ExtensionSettings::from_settings(&content);

        assert!(
            settings.granted_capabilities.is_empty(),
            "assets/settings/default.json must ship an empty capability allowlist; \
             privileged operations require an explicit user grant"
        );
    }

    /// Every privileged operation is denied with the default (an empty
    /// allowlist) and becomes allowed only once an explicit grant is persisted:
    /// a specific user grant deserializes unchanged and serializes back to the
    /// same JSON.
    #[test]
    fn test_privileged_operations_denied_until_grant_persisted() {
        // The extension's manifest requests all three privileged operations, so
        // any denial below comes from the host's grant list, not the manifest.
        let manifest: Arc<ExtensionManifest> = Arc::new(
            serde_json::from_str(
                r#"{
                    "id": "test-extension",
                    "name": "Test Extension",
                    "version": "1.0.0",
                    "schema_version": 0,
                    "capabilities": [
                        { "kind": "process:exec", "command": "ls", "args": ["-la"] },
                        { "kind": "download_file", "host": "github.com", "path": ["**"] },
                        { "kind": "npm:install", "package": "typescript" }
                    ]
                }"#,
            )
            .expect("test manifest should deserialize"),
        );

        // The default grants nothing, so every privileged operation is denied.
        let defaults = ExtensionSettings::from_settings(&SettingsContent::default());
        let granter = CapabilityGranter::new(defaults.granted_capabilities, manifest.clone());
        assert!(granter.grant_exec("ls", &["-la"]).is_err());
        assert!(granter
            .grant_download_file(
                &Url::parse("https://github.com/zed-industries/zed/archive/refs/heads/main.zip")
                    .expect("test URL should parse"),
            )
            .is_err());
        assert!(granter.grant_npm_install_package("typescript").is_err());

        // A specific user grant deserializes unchanged ...
        let user_content: SettingsContent = settings::parse_json_with_comments(
            r#"{
                "extension": {
                    "granted_capabilities": [
                        { "kind": "process:exec", "command": "ls", "args": ["-la"] },
                        { "kind": "download_file", "host": "github.com", "path": ["**"] },
                        { "kind": "npm:install", "package": "typescript" }
                    ]
                }
            }"#,
        )
        .expect("user extension settings should parse");
        let user_settings = ExtensionSettings::from_settings(&user_content);
        assert_eq!(
            user_settings.granted_capabilities,
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

        // ... serializes back to the same JSON ...
        assert_eq!(
            serde_json::to_value(&user_settings.granted_capabilities)
                .expect("capabilities should serialize"),
            serde_json::json!([
                { "kind": "process:exec", "command": "ls", "args": ["-la"] },
                { "kind": "download_file", "host": "github.com", "path": ["**"] },
                { "kind": "npm:install", "package": "typescript" }
            ])
        );

        // ... and the operations the default denied are now allowed.
        let granter = CapabilityGranter::new(user_settings.granted_capabilities, manifest);
        assert!(granter.grant_exec("ls", &["-la"]).is_ok());
        assert!(granter
            .grant_download_file(
                &Url::parse("https://github.com/zed-industries/zed/archive/refs/heads/main.zip")
                    .expect("test URL should parse"),
            )
            .is_ok());
        assert!(granter.grant_npm_install_package("typescript").is_ok());
    }
}
