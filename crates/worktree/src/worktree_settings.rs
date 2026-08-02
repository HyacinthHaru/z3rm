use std::path::Path;

use anyhow::Context as _;
use settings::{RegisterSetting, ScanSymlinksSetting, Settings};
use util::{
    ResultExt,
    paths::{PathMatcher, PathStyle},
    rel_path::RelPath,
};

#[derive(Clone, PartialEq, Eq, RegisterSetting)]
pub struct WorktreeSettings {
    /// Whether to prevent this project from being shared in public channels.
    pub prevent_sharing_in_public_channels: bool,
    pub file_scan_exclusions: PathMatcher,
    pub file_scan_inclusions: PathMatcher,
    /// This field contains all ancestors of the `file_scan_inclusions`. It's used to
    /// determine whether to terminate worktree scanning for a given dir.
    pub parent_dir_scan_inclusions: PathMatcher,
    pub scan_symlinks: ScanSymlinksSetting,
    pub private_files: PathMatcher,
    pub hidden_files: PathMatcher,
    pub read_only_files: PathMatcher,
}

impl WorktreeSettings {
    pub fn is_path_private(&self, path: &RelPath) -> bool {
        path.ancestors()
            .any(|ancestor| self.private_files.is_match(ancestor))
    }

    pub fn is_path_excluded(&self, path: &RelPath) -> bool {
        path.ancestors()
            .any(|ancestor| self.file_scan_exclusions.is_match(ancestor))
    }

    pub fn is_path_always_included(&self, path: &RelPath, is_dir: bool) -> bool {
        if is_dir {
            self.parent_dir_scan_inclusions.is_match(path)
        } else {
            self.file_scan_inclusions.is_match(path)
        }
    }

    pub fn is_path_hidden(&self, path: &RelPath) -> bool {
        path.ancestors()
            .any(|ancestor| self.hidden_files.is_match(ancestor))
    }

    pub fn is_path_read_only(&self, path: &RelPath) -> bool {
        self.read_only_files.is_match(path)
    }

    pub fn is_std_path_read_only(&self, path: &Path) -> bool {
        self.read_only_files.is_match_std_path(path)
    }
}

impl Settings for WorktreeSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        // 项目设置结构已重构, worktree 子模块已移除
        // 使用 project 级别的字段和默认值填充 WorktreeSettings (spec §16 Plan 16)
        let scan_symlinks = content.project.scan_symlinks.clone();
        let excluded_paths = content.project.excluded_paths.clone().unwrap_or_default();
        let file_scan_exclusions: Vec<String> = excluded_paths
            .iter()
            .map(|p| p.to_string_lossy().into())
            .collect();

        let file_scan_inclusions = content
            .project
            .file_scan_inclusions
            .clone()
            .unwrap_or_default();
        // Scanning stops at a directory that cannot contain an included path, so
        // every ancestor of an inclusion has to match too.
        let parent_dir_inclusions: Vec<String> = file_scan_inclusions
            .iter()
            .flat_map(|glob| {
                Path::new(glob)
                    .ancestors()
                    .skip(1)
                    .filter(|ancestor| !ancestor.as_os_str().is_empty())
                    .map(|ancestor| ancestor.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .collect();

        Self {
            prevent_sharing_in_public_channels: false,
            file_scan_exclusions: path_matchers(file_scan_exclusions, "file_scan_exclusions")
                .log_err()
                .unwrap_or_default(),
            parent_dir_scan_inclusions: path_matchers(
                parent_dir_inclusions,
                "file_scan_inclusions ancestors",
            )
            .log_err()
            .unwrap_or_default(),
            file_scan_inclusions: path_matchers(file_scan_inclusions, "file_scan_inclusions")
                .log_err()
                .unwrap_or_default(),
            private_files: path_matchers(
                content.project.private_files.clone().unwrap_or_default(),
                "private_files",
            )
            .log_err()
            .unwrap_or_default(),
            hidden_files: path_matchers(
                content.project.hidden_files.clone().unwrap_or_default(),
                "hidden_files",
            )
            .log_err()
            .unwrap_or_default(),
            read_only_files: path_matchers(
                content.project.read_only_files.clone().unwrap_or_default(),
                "read_only_files",
            )
            .log_err()
            .unwrap_or_default(),
            scan_symlinks,
        }
    }
}

fn path_matchers(mut values: Vec<String>, context: &'static str) -> anyhow::Result<PathMatcher> {
    values.sort();
    PathMatcher::new(values, PathStyle::local())
        .with_context(|| format!("Failed to parse globs from {}", context))
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::SettingsContent;
    use util::rel_path::rel_path;

    fn settings_with(
        configure: impl FnOnce(&mut settings::ProjectSettingsContent),
    ) -> WorktreeSettings {
        let mut content = SettingsContent::default();
        configure(&mut content.project);
        WorktreeSettings::from_settings(&content)
    }

    /// Every one of these globs used to be dropped on the floor, so a `.env`
    /// was neither private nor read-only and the guards built on top of these
    /// predicates silently passed everything.
    #[test]
    fn configured_globs_reach_the_predicates() {
        let settings = settings_with(|project| {
            project.private_files = Some(vec!["**/.env*".into(), "**/*.pem".into()]);
            project.hidden_files = Some(vec!["**/*.log".into()]);
            project.read_only_files = Some(vec!["**/vendor/**".into()]);
        });

        assert!(settings.is_path_private(rel_path("app/.env.local")));
        assert!(settings.is_path_private(rel_path("keys/server.pem")));
        assert!(!settings.is_path_private(rel_path("src/main.rs")));

        assert!(settings.is_path_hidden(rel_path("logs/build.log")));
        assert!(!settings.is_path_hidden(rel_path("src/main.rs")));

        assert!(settings.is_path_read_only(rel_path("vendor/lib/thing.rs")));
        assert!(!settings.is_path_read_only(rel_path("src/main.rs")));
    }

    #[test]
    fn unset_globs_match_nothing() {
        let settings = settings_with(|_| {});
        assert!(!settings.is_path_private(rel_path("app/.env")));
        assert!(!settings.is_path_hidden(rel_path("app/.env")));
        assert!(!settings.is_path_read_only(rel_path("app/.env")));
    }

    /// Scanning stops at a directory that cannot contain an inclusion, so the
    /// ancestors of every included glob have to match as well — otherwise the
    /// walk never reaches the file the user asked to include.
    #[test]
    fn inclusion_ancestors_are_matched_so_scanning_descends() {
        let settings = settings_with(|project| {
            project.file_scan_inclusions = Some(vec!["node_modules/some-package/dist".into()]);
        });

        assert!(
            settings.is_path_always_included(rel_path("node_modules/some-package/dist"), false)
        );
        assert!(settings.is_path_always_included(rel_path("node_modules/some-package"), true));
        assert!(settings.is_path_always_included(rel_path("node_modules"), true));
        assert!(!settings.is_path_always_included(rel_path("target"), true));
    }
}
