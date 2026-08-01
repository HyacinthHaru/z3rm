use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncApp;
use language::{LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use semver::Version;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt;

#[cfg(target_os = "windows")]
const SERVER_PATH: &str =
    "node_modules/@tailwindcss/language-server/bin/tailwindcss-language-server";
#[cfg(not(target_os = "windows"))]
const SERVER_PATH: &str = "node_modules/.bin/tailwindcss-language-server";

fn server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

/// Tailwind 语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct TailwindLspAdapter;

impl TailwindLspAdapter {
    const SERVER_NAME: LanguageServerName =
        LanguageServerName::new_static("tailwindcss-language-server");
    const PACKAGE_NAME: &str = "@tailwindcss/language-server";
}

impl LspInstaller for TailwindLspAdapter {
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
impl LspAdapter for TailwindLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Some(json!({
            "provideFormatter": true,
        })))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        _cx: &mut AsyncApp,
    ) -> Result<Value> {
        // lsp_store 已删除，使用默认配置
        let mut tailwind_user_settings = json!({});
        tailwind_user_settings["emmetCompletions"] = Value::Bool(true);
        tailwind_user_settings["includeLanguages"] = json!({
            "html": "html",
            "css": "css",
            "javascript": "javascript",
            "typescript": "typescript",
            "typescriptreact": "typescriptreact",
        });

        Ok(json!({
            "tailwindCSS": tailwind_user_settings
        }))
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("Astro"), "astro".to_string()),
            (LanguageName::new_static("HTML"), "html".to_string()),
            (LanguageName::new_static("Gleam"), "html".to_string()),
            (LanguageName::new_static("CSS"), "css".to_string()),
            (
                LanguageName::new_static("JavaScript"),
                "javascript".to_string(),
            ),
            (
                LanguageName::new_static("TypeScript"),
                "typescript".to_string(),
            ),
            (
                LanguageName::new_static("TSX"),
                "typescriptreact".to_string(),
            ),
            (LanguageName::new_static("Svelte"), "svelte".to_string()),
            (LanguageName::new_static("Elixir"), "elixir".to_string()),
            (LanguageName::new_static("HEEx"), "heex".to_string()),
            (LanguageName::new_static("ERB"), "erb".to_string()),
            (LanguageName::new_static("HTML+ERB"), "erb".to_string()),
            (LanguageName::new_static("PHP"), "php".to_string()),
            (LanguageName::new_static("Vue.js"), "vue".to_string()),
        ])
    }
}
