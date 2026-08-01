use anyhow::Result;
use async_trait::async_trait;
use gpui::AsyncApp;
use language::{
    LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain, language_settings::AllLanguageSettings,
};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use semver::Version;
use serde_json::Value;
use settings::{Settings, SettingsLocation};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{ResultExt, merge_json_value_into, rel_path::RelPath};

const SERVER_PATH: &str = "node_modules/yaml-language-server/bin/yaml-language-server";

fn server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

/// YAML 语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct YamlLspAdapter;

impl YamlLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("yaml-language-server");
    const PACKAGE_NAME: &str = "yaml-language-server";
}

impl LspInstaller for YamlLspAdapter {
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
        let path = delegate.which(Self::SERVER_NAME.as_ref()).await?;
        let env = delegate.shell_env().await;

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["--stdio".into()],
        })
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        _container_dir: PathBuf,
        _delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async { anyhow::bail!("language server installation unavailable (node_runtime removed)") }
    }

    fn check_if_version_installed(
        &self,
        _version: &Self::BinaryVersion,
        _container_dir: &PathBuf,
        _delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        async { None }
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
impl LspAdapter for YamlLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let location = SettingsLocation {
            worktree_id: delegate.worktree_id(),
            path: RelPath::empty(),
        };

        let tab_size = cx.update(|cx| {
            AllLanguageSettings::get(Some(location), cx)
                .language(Some(location), Some(&"YAML".into()), cx)
                .tab_size
        });

        let mut options = serde_json::json!({
            "[yaml]": {"editor.tabSize": tab_size},
            "yaml": {"format": {"enable": true}}
        });

        // lsp_store 已删除，跳过 project options
        // worktree_root 逻辑不再适用
        Ok(options)
    }
}
