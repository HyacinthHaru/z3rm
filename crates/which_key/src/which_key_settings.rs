use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Debug, Clone, Copy, RegisterSetting)]
pub struct WhichKeySettings {
    pub enabled: bool,
    pub delay_ms: u64,
}

impl Settings for WhichKeySettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let which_key = content.which_key.clone().unwrap_or_default();
        Self {
            enabled: which_key.enabled.unwrap_or(false),
            delay_ms: which_key.delay_ms.unwrap_or(1000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::WhichKeySettingsContent;

    #[test]
    fn test_reads_which_key_settings_from_content() {
        let mut content = SettingsContent::default();
        content.which_key = Some(WhichKeySettingsContent {
            enabled: Some(true),
            delay_ms: Some(250),
        });

        let settings = WhichKeySettings::from_settings(&content);

        assert!(settings.enabled);
        assert_eq!(settings.delay_ms, 250);
    }

    #[test]
    fn test_falls_back_to_defaults_when_unset() {
        let settings = WhichKeySettings::from_settings(&SettingsContent::default());

        assert!(!settings.enabled);
        assert_eq!(settings.delay_ms, 1000);
    }

    #[test]
    fn test_parses_which_key_settings_from_json() {
        let content: SettingsContent =
            settings::parse_json_with_comments(r#"{ "which_key": { "enabled": true } }"#)
                .expect("which_key settings should parse");

        let settings = WhichKeySettings::from_settings(&content);

        assert!(settings.enabled);
        assert_eq!(settings.delay_ms, 1000);
    }
}
