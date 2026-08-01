use anyhow::{Context as _, Result};
use async_trait::async_trait;
use gpui::AsyncApp;
use http_client::{
    github::{AssetKind, GitHubLspBinaryVersion, build_asset_url},
    github_download::download_server_binary,
};
use language::{LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use project::Fs;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::{fs, stream::StreamExt};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::merge_json_value_into;
use util::{fs::remove_matching, rel_path::RelPath};

fn eslint_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![
        "--max-old-space-size=8192".into(),
        server_path.into(),
        "--stdio".into(),
    ]
}

/// ESLint 语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct EsLintLspAdapter {
    fs: Arc<dyn Fs>,
}

impl EsLintLspAdapter {
    const CURRENT_VERSION: &'static str = "3.0.24";
    const CURRENT_VERSION_TAG_NAME: &'static str = "release/3.0.24";

    #[cfg(not(windows))]
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::TarGz;
    #[cfg(windows)]
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Zip;

    const SERVER_PATH: &'static str = "vscode-eslint/server/out/eslintServer.js";
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("eslint");

    const FLAT_CONFIG_FILE_NAMES_V8_21: &'static [&'static str] = &["eslint.config.js"];
    const FLAT_CONFIG_FILE_NAMES_V8_57: &'static [&'static str] =
        &["eslint.config.js", "eslint.config.mjs", "eslint.config.cjs"];
    const FLAT_CONFIG_FILE_NAMES_V10: &'static [&'static str] = &[
        "eslint.config.js",
        "eslint.config.mjs",
        "eslint.config.cjs",
        "eslint.config.ts",
        "eslint.config.cts",
        "eslint.config.mts",
    ];
    const LEGACY_CONFIG_FILE_NAMES: &'static [&'static str] = &[
        ".eslintrc",
        ".eslintrc.js",
        ".eslintrc.cjs",
        ".eslintrc.yaml",
        ".eslintrc.yml",
        ".eslintrc.json",
    ];

    pub fn new(fs: Arc<dyn Fs>) -> Self {
        EsLintLspAdapter { fs }
    }

    fn build_destination_path(container_dir: &Path) -> PathBuf {
        container_dir.join(format!("vscode-eslint-{}", Self::CURRENT_VERSION))
    }
}

impl LspInstaller for EsLintLspAdapter {
    type BinaryVersion = GitHubLspBinaryVersion;

    async fn fetch_latest_server_version(
        &self,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<GitHubLspBinaryVersion> {
        let url = build_asset_url(
            "microsoft/vscode-eslint",
            Self::CURRENT_VERSION_TAG_NAME,
            Self::GITHUB_ASSET_KIND,
        )?;

        Ok(GitHubLspBinaryVersion {
            name: Self::CURRENT_VERSION.into(),
            digest: None,
            url,
        })
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        // 用户可能在 PATH 中安装了 eslint-languageserver
        let path = delegate.which("vscode-eslint".as_ref()).await?;
        let env = delegate.shell_env().await;
        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["--stdio".into()],
        })
    }

    fn fetch_server_binary(
        &self,
        _version: GitHubLspBinaryVersion,
        _container_dir: PathBuf,
        _delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async { anyhow::bail!("language server installation unavailable (node_runtime removed)") }
    }

    async fn cached_server_binary(
        &self,
        _container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        // node_runtime 已删除，无法检查缓存
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EslintConfigKind {
    Flat,
    Legacy,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EslintSettingsOverrides {
    use_flat_config: Option<bool>,
    experimental_use_flat_config: Option<bool>,
}

impl EslintSettingsOverrides {
    fn apply_to(self, workspace_configuration: &mut Value) {
        if let Some(use_flat_config) = self.use_flat_config
            && let Some(workspace_configuration) = workspace_configuration.as_object_mut()
        {
            workspace_configuration.insert("useFlatConfig".to_string(), json!(use_flat_config));
        }

        if let Some(experimental_use_flat_config) = self.experimental_use_flat_config
            && let Some(workspace_configuration) = workspace_configuration.as_object_mut()
        {
            workspace_configuration.insert(
                "experimental.useFlatConfig".to_string(),
                json!(experimental_use_flat_config),
            );
        }
    }
}

#[async_trait(?Send)]
impl LspAdapter for EsLintLspAdapter {
    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::new("source.fixAll.eslint"),
        ])
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        requested_uri: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let worktree_root = delegate.worktree_root_path();
        let requested_file_path = requested_uri
            .as_ref()
            .filter(|uri| uri.scheme() == "file")
            .and_then(|uri| uri.to_file_path().ok())
            .filter(|path| path.starts_with(worktree_root));

        // eslint 版本检测不再可用 (node_runtime 已删除)
        let eslint_version: Option<Version> = None;
        let config_kind = find_eslint_config_kind(
            worktree_root,
            requested_file_path.as_deref(),
            eslint_version.as_ref(),
            self.fs.as_ref(),
        )
        .await;
        let eslint_settings_overrides =
            eslint_settings_overrides_for(eslint_version.as_ref(), config_kind);

        let mut default_workspace_configuration = json!({
            "validate": "on",
            "rulesCustomizations": [],
            "run": "onType",
            "nodePath": null,
            "workingDirectory": {
                "mode": "auto"
            },
            "workspaceFolder": {
                "uri": Uri::from_file_path(worktree_root)
                    .map(|uri| uri.as_str().to_owned())
                    .unwrap_or_default(),
                "name": worktree_root.file_name()
                    .unwrap_or(worktree_root.as_os_str())
                    .to_string_lossy(),
            },
            "problems": {},
            "codeActionOnSave": {
                "enable": true,
            },
            "codeAction": {
                "disableRuleComment": {
                    "enable": true,
                    "location": "separateLine",
                },
                "showDocumentation": {
                    "enable": true
                }
            }
        });
        eslint_settings_overrides.apply_to(&mut default_workspace_configuration);

        // lsp_store 已删除，跳过 language_server_settings_for
        Ok(json!({
            "": default_workspace_configuration
        }))
    }

    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }
}

fn ancestor_directories<'a>(
    worktree_root: &'a Path,
    requested_file: Option<&'a Path>,
) -> impl Iterator<Item = &'a Path> + 'a {
    let start = requested_file
        .filter(|file| file.starts_with(worktree_root))
        .unwrap_or(worktree_root);

    std::iter::successors(Some(start), |dir| {
        dir.parent().filter(|p| !p.as_os_str().is_empty())
    })
    .chain(std::iter::once(worktree_root))
}

fn flat_config_file_names(version: Option<&Version>) -> &'static [&'static str] {
    let Some(version) = version else {
        return EsLintLspAdapter::FLAT_CONFIG_FILE_NAMES_V8_21;
    };

    if version.major >= 10 {
        EsLintLspAdapter::FLAT_CONFIG_FILE_NAMES_V10
    } else if version.major == 8 && version.minor >= 57 {
        EsLintLspAdapter::FLAT_CONFIG_FILE_NAMES_V8_57
    } else {
        EsLintLspAdapter::FLAT_CONFIG_FILE_NAMES_V8_21
    }
}

async fn find_eslint_config_kind(
    worktree_root: &Path,
    requested_file: Option<&Path>,
    version: Option<&Version>,
    fs: &dyn Fs,
) -> Option<EslintConfigKind> {
    let flat_config_file_names = flat_config_file_names(version);

    for directory in ancestor_directories(worktree_root, requested_file) {
        for file_name in flat_config_file_names {
            if fs.is_file(&directory.join(file_name)).await {
                return Some(EslintConfigKind::Flat);
            }
        }

        for file_name in EsLintLspAdapter::LEGACY_CONFIG_FILE_NAMES {
            if fs.is_file(&directory.join(file_name)).await {
                return Some(EslintConfigKind::Legacy);
            }
        }
    }

    None
}

fn eslint_settings_overrides_for(
    version: Option<&Version>,
    config_kind: Option<EslintConfigKind>,
) -> EslintSettingsOverrides {
    let Some(version) = version else {
        return EslintSettingsOverrides::default();
    };

    match config_kind {
        Some(EslintConfigKind::Flat) if version.major == 8 && (21..57).contains(&version.minor) => {
            EslintSettingsOverrides {
                use_flat_config: None,
                experimental_use_flat_config: Some(true),
            }
        }
        Some(EslintConfigKind::Legacy) if version.major == 9 => EslintSettingsOverrides {
            use_flat_config: Some(false),
            experimental_use_flat_config: None,
        },
        _ => EslintSettingsOverrides::default(),
    }
}

/// On Windows, converts Unix-style separators (/) to Windows-style (\).
/// On Unix, returns the path unchanged
fn normalize_path_separators(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

fn determine_working_directory(
    uri: Uri,
    working_directories: Vec<WorkingDirectory>,
    workspace_folder_path: PathBuf,
) -> Option<ResultWorkingDirectory> {
    let file_path = uri.to_file_path().ok()?;
    let file_path_str = file_path.to_string_lossy();

    for working_directory in working_directories {
        match working_directory {
            WorkingDirectory::Glob(glob) => {
                let pattern = glob.replace("**", "**");
                if let Some(matched) = match_glob_pattern(&pattern, &file_path) {
                    return Some(ResultWorkingDirectory::Path(matched.into()));
                }
            }
            WorkingDirectory::Path(path) => {
                if file_path.starts_with(&path) {
                    return Some(ResultWorkingDirectory::Path(path));
                }
            }
        }
    }

    Some(ResultWorkingDirectory::Path(workspace_folder_path))
}

fn match_glob_pattern(pattern: &str, file_path: &Path) -> Option<String> {
    let normalized_file_path = normalize_path_separators(&file_path.to_string_lossy());
    let normalized_pattern = normalize_path_separators(pattern);

    let glob = globset::GlobBuilder::new(&normalized_pattern)
        .case_insensitive(false)
        .build()
        .ok()?;
    let matcher = glob.compile_matcher();
    matcher
        .is_match(&PathBuf::from(&normalized_file_path))
        .then(|| {
            let mut path = PathBuf::from(pattern);
            // Replace ** with the relative path between the pattern root and the file
            if pattern.contains("**") {
                if let Some(base_dir) = pattern.split("**").next() {
                    let base_path = PathBuf::from(normalize_path_separators(base_dir));
                    if let Ok(rel_path) = file_path.strip_prefix(&base_path) {
                        path = base_path.join(rel_path);
                    }
                }
            }
            path.to_string_lossy().into_owned()
        })
}

#[cfg(target_os = "windows")]
async fn handle_symlink(src_dir: PathBuf, dest_dir: PathBuf) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let mut options = smol::fs::OpenOptions::new();
    options.create(true).write(true);
    options.custom_flags(0x00000200); // FILE_FLAG_OPEN_REPARSE_POINT

    let mut file = options.open(&src_dir).await?;
    file.set_length(0).await?;
    smol::fs::symlink(&dest_dir, &src_dir).await?;
    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
struct LegacyDirectoryItem {
    path: String,
    glob: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct DirectoryItem {
    path: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct PatternItem {
    pattern: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct ModeItem {
    mode: ModeEnum,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum ModeEnum {
    Auto,
    Location,
    Workspace,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum WorkingDirectory {
    Glob(String),
    Path(PathBuf),
}
#[derive(Serialize, Deserialize)]
struct WorkingDirectories(Option<Vec<WorkingDirectory>>);

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum ResultWorkingDirectory {
    Mode(ModeItem),
    Path(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flat_config_file_names() {
        // v8.21 defaults to flat config file names
        assert_eq!(
            flat_config_file_names(Some(&Version::new(8, 21, 0))),
            &["eslint.config.js"]
        );

        // v8.57+ includes mjs and cjs
        assert_eq!(
            flat_config_file_names(Some(&Version::new(8, 57, 0))),
            &["eslint.config.js", "eslint.config.mjs", "eslint.config.cjs"]
        );

        // v10+ includes ts variants
        assert_eq!(
            flat_config_file_names(Some(&Version::new(10, 0, 0))),
            &[
                "eslint.config.js",
                "eslint.config.mjs",
                "eslint.config.cjs",
                "eslint.config.ts",
                "eslint.config.cts",
                "eslint.config.mts"
            ]
        );

        // No version defaults to v8.21 behavior
        assert_eq!(flat_config_file_names(None), &["eslint.config.js"]);
    }
}
