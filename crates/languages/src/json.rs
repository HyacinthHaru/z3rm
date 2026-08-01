use anyhow::{Context as _, Result, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use async_trait::async_trait;
use collections::HashMap;
use futures::StreamExt;
use gpui::AsyncApp;
use http_client::github::{GitHubLspBinaryVersion, latest_github_release};
use language::{
    LanguageName, LanguageRegistry, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain,
};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use semver::Version;
use serde_json::{Value, json};
use settings::SettingsLocation;
use smol::{
    fs::{self},
    io::BufReader,
};
use std::{
    borrow::Cow,
    env::consts,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use util::{
    ResultExt, archive::extract_zip, fs::remove_matching, maybe, merge_json_value_into,
    paths::PathStyle, rel_path::RelPath,
};

const SERVER_PATH: &str =
    "node_modules/vscode-langservers-extracted/bin/vscode-json-language-server";

fn server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

/// JSON 语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct JsonLspAdapter {
    languages: Arc<LanguageRegistry>,
}

impl JsonLspAdapter {
    const PACKAGE_NAME: &str = "vscode-langservers-extracted";

    pub fn new(languages: Arc<LanguageRegistry>) -> Self {
        Self { languages }
    }
}

impl LspInstaller for JsonLspAdapter {
    type BinaryVersion = Version;

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        anyhow::bail!("npm package version lookup unavailable (node_runtime removed)")
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate
            .which("vscode-json-language-server".as_ref())
            .await?;
        let env = delegate.shell_env().await;

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["--stdio".into()],
        })
    }

    fn check_if_version_installed(
        &self,
        _version: &Self::BinaryVersion,
        _container_dir: &PathBuf,
        _delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        async { None }
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
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
        // node_runtime 已删除，无法检查缓存的 npm 包
        None
    }
}

#[async_trait(?Send)]
impl LspAdapter for JsonLspAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName("json-language-server".into())
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Some(json!({
            "provideFormatter": true
        })))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        requested_uri: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let requested_path = requested_uri.as_ref().and_then(|uri| {
            (uri.scheme() == "file")
                .then(|| uri.to_file_path().ok())
                .flatten()
        });
        let path_in_worktree = requested_path
            .as_ref()
            .and_then(|abs_path| {
                let rel_path = abs_path.strip_prefix(delegate.worktree_root_path()).ok()?;
                RelPath::new(rel_path, PathStyle::local()).ok()
            })
            .unwrap_or_else(|| Cow::Borrowed(RelPath::empty()));
        let mut config = cx.update(|cx| {
            let schemas = json_schema_store::all_schema_file_associations(
                &self.languages,
                Some(SettingsLocation {
                    worktree_id: delegate.worktree_id(),
                    path: path_in_worktree.as_ref(),
                }),
                cx,
            );

            serde_json::json!({
                "json": {
                    "format": {
                        "enable": true,
                    },
                    "validate": {
                        "enable": true,
                    },
                    "schemas": schemas
                }
            })
        });

        if let Some(proxy_settings) = cx.update(|cx| {
            json_schema_proxy_settings(cx.http_client().proxy().map(ToString::to_string))
        }) {
            merge_json_value_into(proxy_settings, &mut config);
        }

        // lsp_store 已删除，跳过 project options
        Ok(config)
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        [
            (LanguageName::new_static("JSON"), "json".into()),
            (LanguageName::new_static("JSONC"), "jsonc".into()),
        ]
        .into_iter()
        .collect()
    }

    fn is_primary_zed_json_schema_adapter(&self) -> bool {
        true
    }
}

fn worktree_root(delegate: &Arc<dyn LspAdapterDelegate>, settings: Option<Value>) -> Option<Value> {
    let Some(Value::Object(mut settings_map)) = settings else {
        return settings;
    };

    let Some(Value::Object(json_config)) = settings_map.get_mut("json") else {
        return Some(Value::Object(settings_map));
    };

    let Some(Value::Array(schemas)) = json_config.get_mut("schemas") else {
        return Some(Value::Object(settings_map));
    };

    for schema in schemas.iter_mut() {
        let Value::Object(schema_map) = schema else {
            continue;
        };
        let Some(Value::String(url)) = schema_map.get_mut("url") else {
            continue;
        };

        if !url.starts_with(".") && !url.starts_with("~") {
            continue;
        }

        *url = delegate
            .resolve_relative_path(url.clone().into())
            .to_string_lossy()
            .into_owned();
    }

    Some(Value::Object(settings_map))
}

fn json_schema_proxy_settings(proxy: Option<String>) -> Option<Value> {
    proxy.map(|proxy| {
        json!({
            "http": {
                "proxy": proxy,
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::json_schema_proxy_settings;

    #[test]
    fn test_json_schema_proxy_settings_includes_proxy() {
        assert_eq!(
            json_schema_proxy_settings(Some("http://proxy.example:8080".to_string())),
            Some(json!({
                "http": {
                    "proxy": "http://proxy.example:8080",
                }
            }))
        );
    }

    #[test]
    fn test_json_schema_proxy_settings_ignores_missing_proxy() {
        assert_eq!(json_schema_proxy_settings(None), None);
    }
}

pub struct NodeVersionAdapter;

impl NodeVersionAdapter {
    const SERVER_NAME: LanguageServerName =
        LanguageServerName::new_static("package-version-server");
}

impl LspInstaller for NodeVersionAdapter {
    type BinaryVersion = GitHubLspBinaryVersion;

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<GitHubLspBinaryVersion> {
        let release = latest_github_release(
            "zed-industries/package-version-server",
            true,
            false,
            delegate.http_client(),
        )
        .await?;
        let os = match consts::OS {
            "macos" => "apple-darwin",
            "linux" => "unknown-linux-gnu",
            "windows" => "pc-windows-msvc",
            other => bail!("Running on unsupported os: {other}"),
        };
        let suffix = if consts::OS == "windows" {
            ".zip"
        } else {
            ".tar.gz"
        };
        let asset_name = format!("{}-{}-{os}{suffix}", Self::SERVER_NAME, consts::ARCH);
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| format!("no asset found matching `{asset_name:?}`"))?;
        Ok(GitHubLspBinaryVersion {
            name: release.tag_name,
            url: asset.browser_download_url.clone(),
            digest: asset.digest.clone(),
        })
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which(Self::SERVER_NAME.as_ref()).await?;
        Some(LanguageServerBinary {
            path,
            env: None,
            arguments: Default::default(),
        })
    }

    fn fetch_server_binary(
        &self,
        latest_version: GitHubLspBinaryVersion,
        container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();

        async move {
            let version = &latest_version;
            let destination_path = container_dir.join(format!(
                "{}-{}{}",
                Self::SERVER_NAME,
                version.name,
                std::env::consts::EXE_SUFFIX
            ));
            let destination_container_path =
                container_dir.join(format!("{}-{}-tmp", Self::SERVER_NAME, version.name));
            if fs::metadata(&destination_path).await.is_err() {
                let mut response = delegate
                    .http_client()
                    .get(&version.url, Default::default(), true)
                    .await
                    .context("downloading release")?;
                if version.url.ends_with(".zip") {
                    extract_zip(&destination_container_path, response.body_mut()).await?;
                } else if version.url.ends_with(".tar.gz") {
                    let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
                    let archive = Archive::new(decompressed_bytes);
                    archive.unpack(&destination_container_path).await?;
                }

                fs::copy(
                    destination_container_path.join(format!(
                        "{}{}",
                        Self::SERVER_NAME,
                        std::env::consts::EXE_SUFFIX
                    )),
                    &destination_path,
                )
                .await?;
                remove_matching(&container_dir, |entry| entry != destination_path).await;
            }
            Ok(LanguageServerBinary {
                path: destination_path,
                env: None,
                arguments: Default::default(),
            })
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_version_server_binary(container_dir).await
    }
}

#[async_trait(?Send)]
impl LspAdapter for NodeVersionAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }
}

async fn get_cached_version_server_binary(container_dir: PathBuf) -> Option<LanguageServerBinary> {
    maybe!(async {
        let mut last = None;
        let mut entries = fs::read_dir(&container_dir).await?;
        while let Some(entry) = entries.next().await {
            last = Some(entry?.path());
        }

        anyhow::Ok(LanguageServerBinary {
            path: last.context("no cached binary")?,
            env: None,
            arguments: Default::default(),
        })
    })
    .await
    .log_err()
}
