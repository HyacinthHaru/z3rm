use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncApp;
use language::{
    LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, PromptResponseContext, Toolchain,
};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use project::Fs;
use regex::Regex;
use semver::Version;
use serde_json::Value;
use serde_json::json;
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use util::{ResultExt, maybe};

const ACTION_ALWAYS: &str = "Always";
const ACTION_NEVER: &str = "Never";
const UPDATE_IMPORTS_MESSAGE_PATTERN: &str = "Update imports for";
const VTSLS_SERVER_NAME: &str = "vtsls";

fn typescript_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

/// vtsls 语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct VtslsLspAdapter {
    fs: Arc<dyn Fs>,
}

impl VtslsLspAdapter {
    const PACKAGE_NAME: &'static str = "@vtsls/language-server";
    const SERVER_PATH: &'static str = "node_modules/@vtsls/language-server/bin/vtsls.js";

    const TYPESCRIPT_PACKAGE_NAME: &'static str = "typescript";
    const TYPESCRIPT_TSDK_PATH: &'static str = "node_modules/typescript/lib";
    const TYPESCRIPT_YARN_TSDK_PATH: &'static str = ".yarn/sdks/typescript/lib";

    pub fn new(fs: Arc<dyn Fs>) -> Self {
        VtslsLspAdapter { fs }
    }

    async fn tsdk_path(&self, adapter: &Arc<dyn LspAdapterDelegate>) -> Option<&'static str> {
        let yarn_sdk = adapter
            .worktree_root_path()
            .join(Self::TYPESCRIPT_YARN_TSDK_PATH);

        let tsdk_path = if self.fs.is_dir(&yarn_sdk).await {
            Self::TYPESCRIPT_YARN_TSDK_PATH
        } else {
            Self::TYPESCRIPT_TSDK_PATH
        };

        if self
            .fs
            .is_dir(&adapter.worktree_root_path().join(tsdk_path))
            .await
        {
            Some(tsdk_path)
        } else {
            None
        }
    }

    pub fn enhance_diagnostic_message(message: &str) -> Option<String> {
        static SINGLE_WORD_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"'([^\s']*)'").expect("Failed to create REGEX"));

        static MULTI_WORD_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"'([^']+\s+[^']*)'").expect("Failed to create REGEX"));

        let first = SINGLE_WORD_REGEX.replace_all(message, "`$1`").to_string();
        let second = MULTI_WORD_REGEX
            .replace_all(&first, "\n```typescript\n$1\n```\n")
            .to_string();
        Some(second)
    }
}

pub struct TypeScriptVersions {
    typescript_version: Version,
    server_version: Version,
}

const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("vtsls");

impl LspInstaller for VtslsLspAdapter {
    type BinaryVersion = TypeScriptVersions;

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
        let env = delegate.shell_env().await;
        let path = delegate.which(SERVER_NAME.as_ref()).await?;
        Some(LanguageServerBinary {
            path: path.clone(),
            arguments: typescript_server_binary_arguments(&path),
            env: Some(env),
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
impl LspAdapter for VtslsLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME
    }

    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::REFACTOR,
            CodeActionKind::REFACTOR_EXTRACT,
            CodeActionKind::SOURCE,
        ])
    }

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        use lsp::CompletionItemKind as Kind;
        let label_len = item.label.len();
        let grammar = language.grammar()?;
        let highlight_id = match item.kind? {
            Kind::CLASS | Kind::INTERFACE | Kind::ENUM => grammar.highlight_id_for_name("type"),
            Kind::CONSTRUCTOR => grammar.highlight_id_for_name("type"),
            Kind::CONSTANT => grammar.highlight_id_for_name("constant"),
            Kind::FUNCTION | Kind::METHOD => grammar.highlight_id_for_name("function"),
            Kind::PROPERTY | Kind::FIELD => grammar.highlight_id_for_name("property"),
            Kind::VARIABLE => grammar.highlight_id_for_name("variable"),
            _ => None,
        }?;

        let text = if let Some(description) = item
            .label_details
            .as_ref()
            .and_then(|label_details| label_details.description.as_ref())
        {
            format!("{} {}", item.label, description)
        } else if let Some(detail) = &item.detail {
            format!("{} {}", item.label, detail)
        } else {
            item.label.clone()
        };
        Some(language::CodeLabel::filtered(
            text,
            label_len,
            item.filter_text.as_deref(),
            vec![(0..label_len, highlight_id)],
        ))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let tsdk_path = self.tsdk_path(delegate).await;
        let config = serde_json::json!({
            "tsdk": tsdk_path,
            "suggest": {
                "completeFunctionCalls": true
            },
            "inlayHints": {
                "parameterNames": {
                    "enabled": "all",
                    "suppressWhenArgumentMatchesName": false
                },
                "parameterTypes": {
                    "enabled": true
                },
                "variableTypes": {
                    "enabled": true,
                    "suppressWhenTypeMatchesName": false
                },
                "propertyDeclarationTypes": {
                    "enabled": true
                },
                "functionLikeReturnTypes": {
                    "enabled": true
                },
                "enumMemberValues": {
                    "enabled": true
                }
            },
            "implementationsCodeLens": {
                "enabled": true,
                "showOnAllClassMethods": true,
                "showOnInterfaceMethods": true
            },
            "referencesCodeLens": {
                "enabled": true,
                "showOnAllFunctions": true
            },
            "tsserver": {
                "maxTsServerMemory": 8092
            },
        });

        let default_workspace_configuration = serde_json::json!({
            "typescript": config,
            "javascript": config,
            "vtsls": {
                "experimental": {
                    "completion": {
                        "enableServerSideFuzzyMatch": true,
                        "entriesLimit": 5000,
                    }
                },
               "autoUseWorkspaceTsdk": true
            }
        });

        // lsp_store 已删除，跳过 language_server_settings 覆盖
        Ok(default_workspace_configuration)
    }

    fn diagnostic_message_to_markdown(&self, message: &str) -> Option<String> {
        VtslsLspAdapter::enhance_diagnostic_message(message)
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("TypeScript"), "typescript".into()),
            (LanguageName::new_static("JavaScript"), "javascript".into()),
            (LanguageName::new_static("TSX"), "typescriptreact".into()),
        ])
    }

    // lsp_store 已删除，无法写入 LSP 设置
    fn process_prompt_response(&self, _context: &PromptResponseContext, _cx: &mut AsyncApp) {
        // 用户偏好设置不再持久化到 settings 文件 (lsp_store 已删除)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enhance_diagnostic_message_single_word() {
        let result = VtslsLspAdapter::enhance_diagnostic_message(
            "Cannot find name 'foo'. Did you mean 'bar'?",
        )
        .unwrap();
        assert!(result.contains("`foo`"));
        assert!(result.contains("`bar`"));
    }

    #[test]
    fn test_enhance_diagnostic_message_multi_word() {
        let result = VtslsLspAdapter::enhance_diagnostic_message(
            "Property 'foo bar' does not exist on type 'Baz'.",
        )
        .unwrap();
        assert!(result.contains("```typescript"));
    }
}
