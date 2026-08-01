use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use futures::StreamExt;
use gpui::AsyncApp;
use http_client::github::latest_github_release;
pub use language::*;
use language::{
    LanguageName, LspAdapterDelegate, LspInstaller, language_settings::LanguageSettings,
};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use regex::Regex;
use semver::Version;
use serde_json::{Value, json};
use settings::SemanticTokenRules;
use smol::fs;
use std::{
    ffi::{OsStr, OsString},
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    process::Output,
    str,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
};
use util::{ResultExt, fs::remove_matching, maybe, merge_json_value_into};

pub(crate) fn semantic_token_rules() -> SemanticTokenRules {
    let content = grammars::get_file("go/semantic_token_rules.json")
        .expect("missing go/semantic_token_rules.json");
    let json = std::str::from_utf8(&content.data).expect("invalid utf-8 in semantic_token_rules");
    settings::parse_json_with_comments::<SemanticTokenRules>(json)
        .expect("failed to parse go semantic_token_rules.json")
}

fn server_binary_arguments() -> Vec<OsString> {
    vec!["-mode=stdio".into()]
}

#[derive(Copy, Clone)]
pub struct GoLspAdapter;

impl GoLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("gopls");
}

static VERSION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+").expect("Failed to create VERSION_REGEX"));

static GO_ESCAPE_SUBTEST_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[.*+?^${}()|\[\]\\"']"#).expect("Failed to create GO_ESCAPE_SUBTEST_NAME_REGEX")
});

const BINARY: &str = if cfg!(target_os = "windows") {
    "gopls.exe"
} else {
    "gopls"
};

impl LspInstaller for GoLspAdapter {
    type BinaryVersion = Option<String>;

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Option<String>> {
        // spawn_command 不可用 (node_runtime crate 已删除)
        // 通过 which 检查 go 是否可用
        if delegate.which("go".as_ref()).await.is_none() {
            anyhow::bail!("`go` was not found");
        }
        Ok(Some("latest".to_string()))
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        delegate
            .which(BINARY.as_ref())
            .await
            .map(|path| LanguageServerBinary {
                path,
                arguments: server_binary_arguments(),
                env: None,
            })
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(&container_dir).await
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        _container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();
        async move {
            // spawn_command 不可用 (node_runtime crate 已删除)
            // 检查 gopls 是否已在 PATH 中
            if delegate.which("go".as_ref()).await.is_none() {
                anyhow::bail!("`go` was not found");
            }
            let path = delegate.which(BINARY.as_ref()).await
                .context("gopls not found in PATH. Install with `go install golang.org/x/tools/cmd/gopls@latest`")?;
            Ok(LanguageServerBinary {
                path,
                arguments: server_binary_arguments(),
                env: None,
            })
        }
    }
}

/// 解析 gopls 版本输出，替代已删除的工具函数
fn parse_version_output(output: &str) -> Option<Version> {
    let stdout = output.trim();
    // gopls v0.16.0
    let version_str = stdout.split_whitespace().find(|t| t.starts_with('v'))?;
    let version_str = version_str.strip_prefix('v')?;
    Version::parse(version_str).ok()
}

#[async_trait(?Send)]
impl LspAdapter for GoLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::REFACTOR_EXTRACT,
            CodeActionKind::REFACTOR_INLINE,
        ])
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let project_initialization_options = cx.update(|cx| {
            // lsp_store 已删除，跳过 language_server_settings
            None
        });

        let mut default_workspace_configuration = json!({
            "usePlaceholders": true
        });

        if let Some(options) = project_initialization_options {
            merge_json_value_into(options, &mut default_workspace_configuration)
        }

        Ok(default_workspace_configuration)
    }

    async fn labels_for_symbols(
        self: Arc<Self>,
        symbols: &[language::Symbol],
        language: &Arc<language::Language>,
    ) -> Result<Vec<Option<language::CodeLabel>>> {
        let mut labels = Vec::new();
        for symbol in symbols {
            let label = self.label_for_symbol(symbol, language).await;
            labels.push(label);
        }
        Ok(labels)
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

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("Go"), "go".into()),
            (LanguageName::new_static("Go Mod "), "go.mod".into()),
            (LanguageName::new_static("Go Work"), "go.work".into()),
        ])
    }
}

async fn get_cached_server_binary(container_dir: &Path) -> Option<LanguageServerBinary> {
    maybe!(async {
        let mut last_binary_path = None;
        let mut entries = fs::read_dir(container_dir).await?;
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            if entry.file_type().await?.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("gopls_"))
            {
                last_binary_path = Some(entry.path());
            }
        }
        let msg = "missing binary";
        let path = last_binary_path.context(msg)?;
        anyhow::Ok(LanguageServerBinary {
            path,
            arguments: server_binary_arguments(),
            env: None,
        })
    })
    .await
    .log_err()
}

fn adjust_runs(
    delta: usize,
    mut runs: Vec<(Range<usize>, HighlightId)>,
) -> Vec<(Range<usize>, HighlightId)> {
    for (range, _) in &mut runs {
        range.start += delta;
        range.end += delta;
    }
    runs
}
