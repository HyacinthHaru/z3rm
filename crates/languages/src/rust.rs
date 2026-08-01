use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use futures::StreamExt;
use futures::lock::OwnedMutexGuard;
use gpui::{App, AppContext, AsyncApp, Entity, SharedString, Task};
use http_client::github::AssetKind;
use http_client::github::{GitHubLspBinaryVersion, latest_github_release};
use http_client::github_download::{GithubBinaryMetadata, download_server_binary};
pub use language::*;
use lsp::{InitializeParams, LanguageServerBinary, LanguageServerBinaryOptions};
use project::project_settings::ProjectSettings;
use regex::Regex;
use serde_json::json;
use settings::{SemanticTokenRules, Settings as _};
use smallvec::SmallVec;
use smol::fs::{self};
use std::borrow::Cow;
use std::cmp::Reverse;
use std::fmt::Display;
use std::future::Future;
use std::ops::Range;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use util::command::{Stdio, new_command};
use util::fs::{make_file_executable, remove_matching};
use util::merge_json_value_into;
use util::rel_path::RelPath;
use util::{ResultExt, maybe};

/// snippet crate 已删除，替代 stub (spec §3.1 L1)
mod snippet {
    use anyhow::Result;

    pub struct Snippet {
        pub text: String,
        pub tabstops: Vec<Tabstop>,
    }

    pub struct Tabstop {
        pub start: usize,
        pub ranges: Vec<std::ops::Range<usize>>,
    }

    impl Snippet {
        pub fn parse(_text: &str) -> Result<Self> {
            // snippet crate 已删除，始终返回错误
            Err(anyhow::anyhow!("snippet parsing unavailable"))
        }
    }
}
use crate::language_settings::LanguageSettings;

pub(crate) fn semantic_token_rules() -> SemanticTokenRules {
    let content = grammars::get_file("rust/semantic_token_rules.json")
        .expect("missing rust/semantic_token_rules.json");
    let json = std::str::from_utf8(&content.data).expect("invalid utf-8 in semantic_token_rules");
    settings::parse_json_with_comments::<SemanticTokenRules>(json)
        .expect("failed to parse rust semantic_token_rules.json")
}

pub struct RustLspAdapter;

#[cfg(target_os = "macos")]
impl RustLspAdapter {
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Gz;
    const ARCH_SERVER_NAME: &str = "apple-darwin";
}

#[cfg(target_os = "linux")]
impl RustLspAdapter {
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Gz;
    const ARCH_SERVER_NAME: &str = "unknown-linux";
}

#[cfg(target_os = "freebsd")]
impl RustLspAdapter {
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Gz;
    const ARCH_SERVER_NAME: &str = "unknown-freebsd";
}

#[cfg(target_os = "windows")]
impl RustLspAdapter {
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Zip;
    const ARCH_SERVER_NAME: &str = "pc-windows-msvc";
}

const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("rust-analyzer");

#[cfg(target_os = "linux")]
enum LibcType {
    Gnu,
    Musl,
}

impl RustLspAdapter {
    fn convert_rust_analyzer_schema(raw_schema: &serde_json::Value) -> serde_json::Value {
        let Some(schema_array) = raw_schema.as_array() else {
            return raw_schema.clone();
        };

        let mut root_properties = serde_json::Map::new();

        for item in schema_array {
            if let Some(props) = item.get("properties").and_then(|p| p.as_object()) {
                for (key, value) in props {
                    let parts: Vec<&str> = key.split('.').collect();

                    if parts.is_empty() {
                        continue;
                    }

                    let parts_to_process = if parts.first() == Some(&"rust-analyzer") {
                        &parts[1..]
                    } else {
                        &parts[..]
                    };

                    if parts_to_process.is_empty() {
                        continue;
                    }

                    let mut current = &mut root_properties;

                    for (i, part) in parts_to_process.iter().enumerate() {
                        let is_last = i == parts_to_process.len() - 1;

                        if is_last {
                            current.insert(part.to_string(), value.clone());
                        } else {
                            let next_current = current
                                .entry(part.to_string())
                                .or_insert_with(|| {
                                    serde_json::json!({
                                        "type": "object",
                                        "properties": {}
                                    })
                                })
                                .as_object_mut()
                                .expect("should be an object")
                                .entry("properties")
                                .or_insert_with(|| serde_json::json!({}))
                                .as_object_mut()
                                .expect("properties should be an object");

                            current = next_current;
                        }
                    }
                }
            }
        }

        serde_json::json!({
            "type": "object",
            "properties": root_properties
        })
    }

    #[cfg(target_os = "linux")]
    async fn determine_libc_type() -> LibcType {
        use futures::pin_mut;

        async fn from_ldd_version() -> Option<LibcType> {
            use util::command::new_command;

            let ldd_output = new_command("ldd").arg("--version").output().await.ok()?;
            let ldd_version = String::from_utf8_lossy(&ldd_output.stdout);

            if ldd_version.contains("GNU libc") || ldd_version.contains("GLIBC") {
                Some(LibcType::Gnu)
            } else if ldd_version.contains("musl") {
                Some(LibcType::Musl)
            } else {
                None
            }
        }

        if let Some(libc_type) = from_ldd_version().await {
            return libc_type;
        }

        let Ok(dir_entries) = smol::fs::read_dir("/lib").await else {
            // defaulting to gnu because nix doesn't have /lib files due to not following FHS
            return LibcType::Gnu;
        };
        let dir_entries = dir_entries.filter_map(async move |e| e.ok());
        pin_mut!(dir_entries);

        let mut has_musl = false;
        let mut has_gnu = false;

        while let Some(entry) = dir_entries.next().await {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with("ld-musl-") {
                has_musl = true;
            } else if file_name.starts_with("ld-linux-") {
                has_gnu = true;
            }
        }

        match (has_musl, has_gnu) {
            (true, _) => LibcType::Musl,
            (_, true) => LibcType::Gnu,
            _ => LibcType::Gnu,
        }
    }

    #[cfg(target_os = "linux")]
    async fn build_arch_server_name_linux() -> String {
        let libc = match Self::determine_libc_type().await {
            LibcType::Musl => "musl",
            LibcType::Gnu => "gnu",
        };

        format!("{}-{}", Self::ARCH_SERVER_NAME, libc)
    }

    async fn rustup_rust_analyzer_for_worktree(
        delegate: &dyn LspAdapterDelegate,
    ) -> Option<PathBuf> {
        if !Self::workspace_has_rust_toolchain_override(delegate).await {
            return None;
        }

        let rustup = delegate.which("rustup".as_ref()).await?;
        let env = delegate.shell_env().await;
        let worktree_root = delegate.worktree_root_path();
        let output = new_command(rustup)
            .args(["which", "rust-analyzer"])
            .envs(env.iter())
            .current_dir(worktree_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        let output = match output {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                log::debug!(
                    "failed to locate rust-analyzer through rustup in {worktree_root:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                return None;
            }
            Err(err) => {
                log::debug!(
                    "failed to run `rustup which rust-analyzer` in {worktree_root:?}: {err:#}"
                );
                return None;
            }
        };

        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        Some(path).filter(|p| !p.as_os_str().is_empty())
    }

    async fn workspace_has_rust_toolchain_override(delegate: &dyn LspAdapterDelegate) -> bool {
        for file_name in ["rust-toolchain.toml", "rust-toolchain"] {
            if fs::metadata(delegate.resolve_relative_path(PathBuf::from(file_name)))
                .await
                .is_ok()
            {
                return true;
            }
        }

        false
    }

    async fn build_asset_name() -> String {
        let extension = match Self::GITHUB_ASSET_KIND {
            AssetKind::TarGz => "tar.gz",
            AssetKind::TarBz2 => "tar.bz2",
            AssetKind::Gz => "gz",
            AssetKind::Zip => "zip",
        };

        #[cfg(target_os = "linux")]
        let arch_server_name = Self::build_arch_server_name_linux().await;
        #[cfg(not(target_os = "linux"))]
        let arch_server_name = Self::ARCH_SERVER_NAME.to_string();

        format!(
            "{}-{}-{}.{}",
            SERVER_NAME,
            std::env::consts::ARCH,
            &arch_server_name,
            extension
        )
    }
}

pub(crate) struct CargoManifestProvider;

impl ManifestProvider for CargoManifestProvider {
    fn name(&self) -> ManifestName {
        SharedString::new_static("Cargo.toml").into()
    }

    fn search(
        &self,
        ManifestQuery {
            path,
            depth,
            delegate,
        }: ManifestQuery,
    ) -> Option<Arc<RelPath>> {
        let mut outermost_cargo_toml = None;
        for path in path.ancestors().take(depth) {
            let p = path.join(RelPath::unix("Cargo.toml").unwrap());
            if delegate.exists(&p, Some(false)) {
                outermost_cargo_toml = Some(Arc::from(path));
            }
        }

        outermost_cargo_toml
    }
}

#[async_trait(?Send)]
impl LspAdapter for RustLspAdapter {
    fn name(&self) -> LanguageServerName {
        SERVER_NAME
    }
    fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        vec!["cargo".to_owned()]
    }

    fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        Some("rust-analyzer/flycheck".into())
    }

    fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams, _: LanguageServerId) {
        static REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)`([^`]+)\n`$").expect("Failed to create REGEX"));

        for diagnostic in &mut params.diagnostics {
            for message in diagnostic
                .related_information
                .iter_mut()
                .flatten()
                .map(|info| &mut info.message)
                .chain([&mut diagnostic.message])
            {
                if let Cow::Owned(sanitized) = REGEX.replace_all(message, "`$1`") {
                    *message = sanitized;
                }
            }
        }
    }

    fn diagnostic_message_to_markdown(&self, message: &str) -> Option<String> {
        static REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)\n *").expect("Failed to create REGEX"));
        Some(REGEX.replace_all(message, "\n\n").to_string())
    }

    async fn label_for_completion(
        &self,
        completion: &lsp::CompletionItem,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        // rust-analyzer calls these detail left and detail right in terms of where it expects things to be rendered
        // this usually contains signatures of the thing to be completed
        let detail_right = completion
            .label_details
            .as_ref()
            .and_then(|detail| detail.description.as_ref())
            .or(completion.detail.as_ref())
            .map(|detail| detail.trim());
        // this tends to contain alias and import information
        let mut detail_left = completion
            .label_details
            .as_ref()
            .and_then(|detail| detail.detail.as_deref());
        let mk_label = |text: String, filter_range: &dyn Fn() -> Range<usize>, runs| {
            let filter_range = completion
                .filter_text
                .as_deref()
                .and_then(|filter| text.find(filter).map(|ix| ix..ix + filter.len()))
                .or_else(|| {
                    text.find(&completion.label)
                        .map(|ix| ix..ix + completion.label.len())
                })
                .unwrap_or_else(filter_range);

            CodeLabel::new(text, filter_range, runs)
        };
        let mut label = match (detail_right, completion.kind) {
            (Some(signature), Some(lsp::CompletionItemKind::FIELD)) => {
                let name = &completion.label;
                let text = format!("{name}: {signature}");
                let prefix = "struct S { ";
                let source = Rope::from_iter([prefix, &text, " }"]);
                let runs =
                    language.highlight_text(&source, prefix.len()..prefix.len() + text.len());
                mk_label(text, &|| 0..completion.label.len(), runs)
            }
            (
                Some(signature),
                Some(lsp::CompletionItemKind::CONSTANT | lsp::CompletionItemKind::VARIABLE),
            ) if completion.insert_text_format != Some(lsp::InsertTextFormat::SNIPPET) => {
                let name = &completion.label;
                let text = format!("{name}: {signature}",);
                let prefix = "let ";
                let source = Rope::from_iter([prefix, &text, " = ();"]);
                let runs =
                    language.highlight_text(&source, prefix.len()..prefix.len() + text.len());
                mk_label(text, &|| 0..completion.label.len(), runs)
            }
            (
                function_signature,
                Some(lsp::CompletionItemKind::FUNCTION | lsp::CompletionItemKind::METHOD),
            ) => {
                const FUNCTION_PREFIXES: [&str; 6] = [
                    "async fn",
                    "async unsafe fn",
                    "const fn",
                    "const unsafe fn",
                    "unsafe fn",
                    "fn",
                ];
                let fn_prefixed = FUNCTION_PREFIXES.iter().find_map(|&prefix| {
                    function_signature?
                        .strip_prefix(prefix)
                        .map(|suffix| (prefix, suffix))
                });
                let label = if let Some(label) = completion
                    .label
                    .strip_suffix("(…)")
                    .or_else(|| completion.label.strip_suffix("()"))
                {
                    label
                } else {
                    &completion.label
                };

                static FULL_SIGNATURE_REGEX: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"fn (.+?)\(").expect("Failed to create REGEX"));
                if let Some((function_signature, match_)) = function_signature
                    .filter(|it| it.contains(&label))
                    .and_then(|it| Some((it, FULL_SIGNATURE_REGEX.find(it)?)))
                {
                    let source = Rope::from(function_signature);
                    let runs = language.highlight_text(&source, 0..function_signature.len());
                    mk_label(
                        function_signature.to_owned(),
                        &|| match_.range().start + 3..match_.range().end - 1,
                        runs,
                    )
                } else if let Some((prefix, suffix)) = fn_prefixed {
                    let text = format!("{label}{suffix}");
                    let source = Rope::from_iter([prefix, " ", &text, " {}"]);
                    let run_start = prefix.len() + 1;
                    let runs = language.highlight_text(&source, run_start..run_start + text.len());
                    mk_label(text, &|| 0..label.len(), runs)
                } else if completion
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.starts_with("macro_rules! "))
                {
                    let text = completion.label.clone();
                    let len = text.len();
                    let source = Rope::from(text.as_str());
                    let runs = language.highlight_text(&source, 0..len);
                    mk_label(text, &|| 0..completion.label.len(), runs)
                } else if detail_left.is_none() {
                    return None;
                } else {
                    mk_label(
                        completion.label.clone(),
                        &|| 0..completion.label.len(),
                        vec![],
                    )
                }
            }
            (_, kind) => {
                let mut label;
                let mut runs = vec![];

                if completion.insert_text_format == Some(lsp::InsertTextFormat::SNIPPET)
                    && let Some(
                        lsp::CompletionTextEdit::InsertAndReplace(lsp::InsertReplaceEdit {
                            new_text,
                            ..
                        })
                        | lsp::CompletionTextEdit::Edit(lsp::TextEdit { new_text, .. }),
                    ) = completion.text_edit.as_ref()
                    && let Ok(mut snippet) = snippet::Snippet::parse(new_text)
                    && snippet.tabstops.len() > 1
                {
                    label = String::new();

                    // we never display the final tabstop
                    snippet.tabstops.remove(snippet.tabstops.len() - 1);

                    let mut text_pos = 0;

                    let mut all_stop_ranges = snippet
                        .tabstops
                        .into_iter()
                        .flat_map(|stop| stop.ranges)
                        .collect::<SmallVec<[_; 8]>>();
                    all_stop_ranges.sort_unstable_by_key(|a| (a.start, Reverse(a.end)));

                    for range in &all_stop_ranges {
                        let start_pos = range.start as usize;
                        let end_pos = range.end as usize;

                        label.push_str(&snippet.text[text_pos..start_pos]);

                        if start_pos == end_pos {
                            let caret_start = label.len();
                            label.push('…');
                            runs.push((caret_start..label.len(), HighlightId::TABSTOP_INSERT_ID));
                        } else {
                            let label_start = label.len();
                            label.push_str(&snippet.text[start_pos..end_pos]);
                            let label_end = label.len();
                            runs.push((label_start..label_end, HighlightId::TABSTOP_REPLACE_ID));
                        }

                        text_pos = end_pos;
                    }

                    label.push_str(&snippet.text[text_pos..]);

                    if detail_left.is_some_and(|detail_left| detail_left == new_text) {
                        // We only include the left detail if it isn't the snippet again
                        detail_left.take();
                    }

                    runs.extend(language.highlight_text(&Rope::from(&label), 0..label.len()));
                } else {
                    let highlight_name = kind.and_then(|kind| match kind {
                        lsp::CompletionItemKind::STRUCT
                        | lsp::CompletionItemKind::INTERFACE
                        | lsp::CompletionItemKind::ENUM => Some("type"),
                        lsp::CompletionItemKind::ENUM_MEMBER => Some("variant"),
                        lsp::CompletionItemKind::KEYWORD => Some("keyword"),
                        lsp::CompletionItemKind::VALUE | lsp::CompletionItemKind::CONSTANT => {
                            Some("constant")
                        }
                        _ => None,
                    });

                    label = completion.label.clone();

                    if let Some(highlight_name) = highlight_name {
                        let highlight_id =
                            language.grammar()?.highlight_id_for_name(highlight_name)?;
                        runs.push((
                            0..label.rfind('(').unwrap_or(completion.label.len()),
                            highlight_id,
                        ));
                    } else if detail_left.is_none()
                        && kind != Some(lsp::CompletionItemKind::SNIPPET)
                    {
                        return None;
                    }
                }

                let label_len = label.len();

                mk_label(label, &|| 0..label_len, runs)
            }
        };

        if let Some(detail_left) = detail_left {
            label.text.push(' ');
            if !detail_left.starts_with('(') {
                label.text.push('(');
            }
            label.text.push_str(detail_left);
            if !detail_left.ends_with(')') {
                label.text.push(')');
            }
        }

        Some(label)
    }

    async fn initialization_options_schema(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        cached_binary: OwnedMutexGuard<Option<(bool, LanguageServerBinary)>>,
        cx: &mut AsyncApp,
    ) -> Option<serde_json::Value> {
        let binary = self
            .get_language_server_command(
                delegate.clone(),
                None,
                LanguageServerBinaryOptions {
                    allow_path_lookup: true,
                    allow_binary_download: false,
                    pre_release: false,
                },
                cached_binary,
                cx.clone(),
            )
            .await
            .0
            .ok()?;

        let mut command = util::command::new_command(&binary.path);
        command
            .arg("--print-config-schema")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let cmd = command
            .spawn()
            .map_err(|e| log::debug!("failed to spawn command {command:?}: {e}"))
            .ok()?;
        let output = cmd
            .output()
            .await
            .map_err(|e| log::debug!("failed to execute command {command:?}: {e}"))
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let raw_schema: serde_json::Value = serde_json::from_slice(output.stdout.as_slice())
            .map_err(|e| log::debug!("failed to parse rust-analyzer's JSON schema output: {e}"))
            .ok()?;

        // Convert rust-analyzer's array-based schema format to nested JSON Schema
        let converted_schema = Self::convert_rust_analyzer_schema(&raw_schema);
        Some(converted_schema)
    }

    async fn label_for_symbol(
        &self,
        symbol: &language::Symbol,
        language: &Arc<Language>,
    ) -> Option<CodeLabel> {
        let name = &symbol.name;
        let (prefix, suffix) = match symbol.kind {
            lsp::SymbolKind::METHOD | lsp::SymbolKind::FUNCTION => ("fn ", "();"),
            lsp::SymbolKind::STRUCT => ("struct ", ";"),
            lsp::SymbolKind::ENUM => ("enum ", "{}"),
            lsp::SymbolKind::INTERFACE => ("trait ", "{}"),
            lsp::SymbolKind::CONSTANT => ("const ", ":()=();"),
            lsp::SymbolKind::MODULE => ("mod ", ";"),
            lsp::SymbolKind::PACKAGE => ("extern crate ", ";"),
            lsp::SymbolKind::TYPE_PARAMETER => ("type ", "=();"),
            lsp::SymbolKind::ENUM_MEMBER => {
                let prefix = "enum E {";
                return Some(CodeLabel::new(
                    name.to_string(),
                    0..name.len(),
                    language.highlight_text(
                        &Rope::from_iter([prefix, name, "}"]),
                        prefix.len()..prefix.len() + name.len(),
                    ),
                ));
            }
            _ => return None,
        };

        let filter_range = prefix.len()..prefix.len() + name.len();
        let display_range = 0..filter_range.end;
        Some(CodeLabel::new(
            format!("{prefix}{name}"),
            filter_range,
            language.highlight_text(&Rope::from_iter([prefix, name, suffix]), display_range),
        ))
    }

    fn prepare_initialize_params(
        &self,
        mut original: InitializeParams,
        cx: &App,
    ) -> Result<InitializeParams> {
        // enable_lsp_tasks 字段已从 LspSettings 移除 (spec §16 Plan 16)
        let enable_lsp_tasks = false;

        let mut experimental = json!({
            "commands": {
                "commands": [
                    "rust-analyzer.showReferences",
                    "rust-analyzer.gotoLocation",
                    "rust-analyzer.triggerParameterHints",
                    "rust-analyzer.rename",
                ]
            }
        });

        if enable_lsp_tasks {
            merge_json_value_into(
                json!({
                    "runnables": {
                        "kinds": [ "cargo", "shell" ],
                    },
                    "commands": {
                        "commands": [
                            "rust-analyzer.runSingle",
                        ]
                    }
                }),
                &mut experimental,
            );
        }

        if let Some(original_experimental) = &mut original.capabilities.experimental {
            merge_json_value_into(experimental, original_experimental);
        } else {
            original.capabilities.experimental = Some(experimental);
        }

        Ok(original)
    }

    fn client_command(
        &self,
        command_name: &str,
        arguments: &[serde_json::Value],
    ) -> Option<ClientCommand> {
        match command_name {
            "rust-analyzer.showReferences" => Some(ClientCommand::ShowLocations),
            "rust-analyzer.runSingle" => {
                // lsp_ext_command 模块已删除 (lsp_store 已移除)
                None
            }
            _ => None,
        }
    }
}

impl LspInstaller for RustLspAdapter {
    type BinaryVersion = GitHubLspBinaryVersion;
    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        cx: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let delegate = delegate.clone();
        cx.background_spawn(async move {
            let env = delegate.shell_env().await;
            if let Some(path) = Self::rustup_rust_analyzer_for_worktree(delegate.as_ref()).await {
                let result = delegate
                    .try_exec(LanguageServerBinary {
                        path: path.clone(),
                        arguments: vec!["--help".into()],
                        env: Some(env.clone()),
                    })
                    .await;
                if result.is_ok() {
                    log::debug!("found rust-analyzer in rustup toolchain override");
                    return Some(LanguageServerBinary {
                        path,
                        env: Some(env),
                        arguments: vec![],
                    });
                }
            }

            let path = delegate.which("rust-analyzer".as_ref()).await?;

            // It is surprisingly common for ~/.cargo/bin/rust-analyzer to be a symlink to
            // /usr/bin/rust-analyzer that fails when you run it; so we need to test it.
            log::debug!("found rust-analyzer in PATH. trying to run `rust-analyzer --help`");
            let result = delegate
                .try_exec(LanguageServerBinary {
                    path: path.clone(),
                    arguments: vec!["--help".into()],
                    env: Some(env.clone()),
                })
                .await;
            if let Err(err) = result {
                log::debug!(
                    "failed to run rust-analyzer after detecting it in PATH: binary: {:?}: {}",
                    path,
                    err
                );
                return None;
            }

            Some(LanguageServerBinary {
                path,
                env: Some(env),
                arguments: vec![],
            })
        })
        .await
    }

    async fn fetch_latest_server_version(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        pre_release: bool,
        _: &mut AsyncApp,
    ) -> Result<GitHubLspBinaryVersion> {
        let release = latest_github_release(
            "rust-lang/rust-analyzer",
            true,
            pre_release,
            delegate.http_client(),
        )
        .await?;
        let asset_name = Self::build_asset_name().await;
        let asset = release
            .assets
            .into_iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| format!("no asset found matching `{asset_name:?}`"))?;
        Ok(GitHubLspBinaryVersion {
            name: release.tag_name,
            url: asset.browser_download_url,
            digest: asset.digest,
        })
    }

    fn fetch_server_binary(
        &self,
        version: GitHubLspBinaryVersion,
        container_dir: PathBuf,
        delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let delegate = delegate.clone();

        async move {
            let GitHubLspBinaryVersion {
                name,
                url,
                digest: expected_digest,
            } = version;
            let destination_path = container_dir.join(format!("rust-analyzer-{name}"));
            let server_path = match Self::GITHUB_ASSET_KIND {
                AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::Gz => destination_path.clone(), // Tar and gzip extract in place.
                AssetKind::Zip => destination_path.clone().join("rust-analyzer.exe"), // zip contains a .exe
            };

            let binary = LanguageServerBinary {
                path: server_path.clone(),
                env: None,
                arguments: Default::default(),
            };

            let metadata_path = destination_path.with_extension("metadata");
            let metadata = GithubBinaryMetadata::read_from_file(&metadata_path)
                .await
                .ok();
            if let Some(metadata) = metadata {
                let validity_check = async || {
                    delegate
                        .try_exec(LanguageServerBinary {
                            path: server_path.clone(),
                            arguments: vec!["--version".into()],
                            env: None,
                        })
                        .await
                        .inspect_err(|err| {
                            log::warn!(
                                "Unable to run {server_path:?} asset, redownloading: {err:#}",
                            )
                        })
                };
                if let (Some(actual_digest), Some(expected_digest)) =
                    (&metadata.digest, &expected_digest)
                {
                    if actual_digest == expected_digest {
                        if validity_check().await.is_ok() {
                            return Ok(binary);
                        }
                    } else {
                        log::info!(
                            "SHA-256 mismatch for {destination_path:?} asset, downloading new asset. Expected: {expected_digest}, Got: {actual_digest}"
                        );
                    }
                } else if validity_check().await.is_ok() {
                    return Ok(binary);
                }
            }

            download_server_binary(
                &*delegate.http_client(),
                &url,
                expected_digest.as_deref(),
                &destination_path,
                Self::GITHUB_ASSET_KIND,
            )
            .await?;
            make_file_executable(&server_path).await?;
            remove_matching(&container_dir, |path| path != destination_path).await;
            GithubBinaryMetadata::write_to_file(
                &GithubBinaryMetadata {
                    metadata_version: 1,
                    digest: expected_digest,
                },
                &metadata_path,
            )
            .await?;

            Ok(LanguageServerBinary {
                path: server_path,
                env: None,
                arguments: Default::default(),
            })
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(container_dir).await
    }
}

/// Part of the data structure of Cargo metadata
#[derive(Debug, serde::Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, serde::Deserialize)]
struct CargoPackage {
    id: String,
    targets: Vec<CargoTarget>,
    manifest_path: Arc<Path>,
}

#[derive(Debug, serde::Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
    src_path: String,
    #[serde(rename = "required-features", default)]
    required_features: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum TargetKind {
    Bin,
    Example,
}

impl Display for TargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetKind::Bin => write!(f, "bin"),
            TargetKind::Example => write!(f, "example"),
        }
    }
}

impl TryFrom<&str> for TargetKind {
    type Error = ();
    fn try_from(value: &str) -> Result<Self, ()> {
        match value {
            "bin" => Ok(Self::Bin),
            "example" => Ok(Self::Example),
            _ => Err(()),
        }
    }
}
/// Which package and binary target are we in?
#[derive(Debug, PartialEq)]
struct TargetInfo {
    package_name: String,
    target_name: String,
    target_kind: TargetKind,
    required_features: Vec<String>,
}

async fn target_info_from_abs_path(
    abs_path: &Path,
    project_env: Option<&HashMap<String, String>>,
) -> Result<Option<(Option<TargetInfo>, Arc<Path>)>> {
    let mut command = util::command::new_command("cargo");
    if let Some(envs) = project_env {
        command.envs(envs);
    }
    let output = command
        .current_dir(
            abs_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("failed to get parent directory"))?,
        )
        .arg("metadata")
        .arg("--no-deps")
        .arg("--format-version")
        .arg("1")
        .output()
        .await?;

    if !output.status.success() {
        let stderr_msg = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Cargo metadata failed\n {stderr_msg}");
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)?;
    Ok(target_info_from_metadata(metadata, abs_path))
}

fn target_info_from_metadata(
    metadata: CargoMetadata,
    abs_path: &Path,
) -> Option<(Option<TargetInfo>, Arc<Path>)> {
    let mut manifest_path = None;
    for package in metadata.packages {
        let Some(manifest_dir_path) = package.manifest_path.parent() else {
            continue;
        };

        let Some(path_from_manifest_dir) = abs_path.strip_prefix(manifest_dir_path).ok() else {
            continue;
        };
        let candidate_path_length = path_from_manifest_dir.components().count();
        // Pick the most specific manifest path
        if let Some((path, current_length)) = &mut manifest_path {
            if candidate_path_length > *current_length {
                *path = Arc::from(manifest_dir_path);
                *current_length = candidate_path_length;
            }
        } else {
            manifest_path = Some((Arc::from(manifest_dir_path), candidate_path_length));
        };

        for target in package.targets {
            let Some(bin_kind) = target
                .kind
                .iter()
                .find_map(|kind| TargetKind::try_from(kind.as_ref()).ok())
            else {
                continue;
            };
            let target_path = PathBuf::from(target.src_path);
            if target_path == abs_path {
                return manifest_path.map(|(path, _)| {
                    (
                        package_name_from_pkgid(&package.id).map(|package_name| TargetInfo {
                            package_name: package_name.to_owned(),
                            target_name: target.name,
                            required_features: target.required_features,
                            target_kind: bin_kind,
                        }),
                        path,
                    )
                });
            }
        }
    }

    manifest_path.map(|(path, _)| (None, path))
}

async fn human_readable_package_name(
    package_directory: &Path,
    project_env: Option<&HashMap<String, String>>,
) -> Option<String> {
    let mut command = util::command::new_command("cargo");
    if let Some(envs) = project_env {
        command.envs(envs);
    }
    let pkgid = String::from_utf8(
        command
            .current_dir(package_directory)
            .arg("pkgid")
            .output()
            .await
            .log_err()?
            .stdout,
    )
    .ok()?;
    Some(package_name_from_pkgid(&pkgid)?.to_owned())
}

// For providing local `cargo check -p $pkgid` task, we do not need most of the information we have returned.
// Output example in the root of Zed project:
// ```sh
// ❯ cargo pkgid zed
// path+file:///absolute/path/to/project/z3rm/crates/z3rm#0.131.0
// ```
// Another variant, if a project has a custom package name or hyphen in the name:
// ```
// path+file:///absolute/path/to/project/custom-package#my-custom-package@0.1.0
// ```
//
// Extracts the package name from the output according to the spec:
// https://doc.rust-lang.org/cargo/reference/pkgid-spec.html#specification-grammar
fn package_name_from_pkgid(pkgid: &str) -> Option<&str> {
    fn split_off_suffix(input: &str, suffix_start: char) -> &str {
        match input.rsplit_once(suffix_start) {
            Some((without_suffix, _)) => without_suffix,
            None => input,
        }
    }

    let (version_prefix, version_suffix) = pkgid.trim().rsplit_once('#')?;
    let package_name = match version_suffix.rsplit_once('@') {
        Some((custom_package_name, _version)) => custom_package_name,
        None => {
            let host_and_path = split_off_suffix(version_prefix, '?');
            let (_, package_name) = host_and_path.rsplit_once('/')?;
            package_name
        }
    };
    Some(package_name)
}

async fn get_cached_server_binary(container_dir: PathBuf) -> Option<LanguageServerBinary> {
    let binary_result = maybe!(async {
        let mut last = None;
        let mut entries = fs::read_dir(&container_dir)
            .await
            .with_context(|| format!("listing {container_dir:?}"))?;
        while let Some(entry) = entries.next().await {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "metadata") {
                continue;
            }
            last = Some(path);
        }

        let path = match last {
            Some(last) => last,
            None => return Ok(None),
        };
        let path = match RustLspAdapter::GITHUB_ASSET_KIND {
            AssetKind::TarGz | AssetKind::TarBz2 | AssetKind::Gz => path, // Tar and gzip extract in place.
            AssetKind::Zip => path.join("rust-analyzer.exe"),             // zip contains a .exe
        };

        anyhow::Ok(Some(LanguageServerBinary {
            path,
            env: None,
            arguments: Vec::new(),
        }))
    })
    .await;

    match binary_result {
        Ok(Some(binary)) => Some(binary),
        Ok(None) => {
            log::info!("No cached rust-analyzer binary found");
            None
        }
        Err(e) => {
            log::error!("Failed to look up cached rust-analyzer binary: {e:#}");
            None
        }
    }
}
