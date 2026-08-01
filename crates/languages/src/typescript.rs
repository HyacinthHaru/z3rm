use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use futures::future::join_all;
use gpui::{AsyncApp, Entity};
use itertools::Itertools as _;
use language::{Buffer, LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{CodeActionKind, LanguageServerBinary, LanguageServerName, Uri};
use project::Fs;
use semver::Version;
use serde_json::{Value, json};
use smol::lock::RwLock;
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use util::rel_path::RelPath;
use util::{ResultExt, maybe};

use crate::{PackageJson, PackageJsonData};

fn typescript_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

fn replace_test_name_parameters(test_name: &str) -> String {
    static PATTERN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(\$([A-Za-z0-9_\.]+|[\#])|%[psdifjo#\$%])").unwrap());
    PATTERN.split(test_name).map(regex::escape).join("(.+?)")
}

#[derive(Clone, Debug, Default)]
struct PackageJsonContents(Arc<RwLock<HashMap<PathBuf, PackageJson>>>);

impl PackageJsonData {
    fn fill_task_templates(&self, _task_templates: &mut Vec<TaskTemplateStub>) {
        // task crate 已删除，不再填充 task templates
    }
}

/// 占位类型，替代已删除的 task::TaskTemplate
#[derive(Debug, Default, Clone)]
pub struct TaskTemplateStub;

/// TypeScript/JavaScript 语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct TypeScriptLspAdapter {
    fs: Arc<dyn Fs>,
}

impl TypeScriptLspAdapter {
    const OLD_SERVER_PATH: &str = "node_modules/typescript-language-server/lib/cli.js";
    const NEW_SERVER_PATH: &str = "node_modules/typescript-language-server/lib/cli.mjs";

    const PACKAGE_NAME: &str = "typescript";
    const SERVER_PACKAGE_NAME: &str = "typescript-language-server";

    const SERVER_NAME: LanguageServerName =
        LanguageServerName::new_static(Self::SERVER_PACKAGE_NAME);

    pub fn new(fs: Arc<dyn Fs>) -> Self {
        TypeScriptLspAdapter { fs }
    }

    async fn tsdk_path(&self, adapter: &Arc<dyn LspAdapterDelegate>) -> Option<&'static str> {
        let is_yarn = adapter
            .read_text_file(RelPath::unix(".yarn/sdks/typescript/lib/typescript.js").unwrap())
            .await
            .is_ok();

        let tsdk_path = if is_yarn {
            ".yarn/sdks/typescript/lib"
        } else {
            "node_modules/typescript/lib"
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
}

pub struct TypeScriptVersions {
    typescript_version: Version,
    server_version: Version,
}

impl LspInstaller for TypeScriptLspAdapter {
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
        let path = delegate.which(Self::SERVER_PACKAGE_NAME.as_ref()).await?;
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
impl LspAdapter for TypeScriptLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
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

    async fn initialization_options(
        self: Arc<Self>,
        adapter: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        let tsdk_path = self.tsdk_path(adapter).await;
        Ok(Some(json!({
            "provideFormatter": true,
            "hostInfo": "zed",
            "tsserver": {
                "path": tsdk_path,
            },
            "preferences": {
                "includeInlayParameterNameHints": "all",
                "includeInlayParameterNameHintsWhenArgumentMatchesName": true,
                "includeInlayFunctionParameterTypeHints": true,
                "includeInlayVariableTypeHints": true,
                "includeInlayVariableTypeHintsWhenTypeMatchesName": true,
                "includeInlayPropertyDeclarationTypeHints": true,
                "includeInlayFunctionLikeReturnTypeHints": true,
                "includeInlayEnumMemberValueHints": true,
            }
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
        Ok(json!({
            "completions": {
              "completeFunctionCalls": true
            }
        }))
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("TypeScript"), "typescript".into()),
            (LanguageName::new_static("JavaScript"), "javascript".into()),
            (LanguageName::new_static("TSX"), "typescriptreact".into()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};
    use unindent::Unindent;

    #[gpui::test]
    async fn test_outline(cx: &mut TestAppContext) {
        for language in [
            crate::language(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ),
            crate::language("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        ] {
            let text = r#"
            function a() {
              // local variables are included
              let a1 = 1;
              // all functions are included
              async function a2() {}
            }
            // top-level variables are included
            let b: C
            function getB() {}
            // exported variables are included
            export const d = e;
        "#
            .unindent();

            let buffer = cx.new(|cx| language::Buffer::local(text, cx).with_language(language, cx));
            let outline = buffer.read_with(cx, |buffer, _| buffer.snapshot().outline(None));
            assert_eq!(
                outline
                    .items
                    .iter()
                    .map(|item| (item.text.as_str(), item.depth))
                    .collect::<Vec<_>>(),
                &[
                    ("function a()", 0),
                    ("let a1", 1),
                    ("async function a2()", 1),
                    ("let b", 0),
                    ("function getB()", 0),
                    ("const d", 0),
                ]
            );
        }
    }

    #[gpui::test]
    async fn test_outline_with_destructuring(cx: &mut TestAppContext) {
        for language in [
            crate::language(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ),
            crate::language("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        ] {
            let text = r#"
            // Top-level destructuring
            const { a1, a2 } = a;
            const [b1, b2] = b;

            // Defaults and rest
            const [c1 = 1, , c2, ...rest1] = c;
            const { d1, d2: e1, f1 = 2, g1: h1 = 3, ...rest2 } = d;

            function processData() {
              // Nested object destructuring
              const { c1, c2 } = c;
              // Nested array destructuring
              const [d1, d2, d3] = d;
              // Destructuring with renaming
              const { f1: g1 } = f;
              // With defaults
              const [x = 10, y] = xy;
            }

            class DataHandler {
              method() {
                // Destructuring in class method
                const { a1, a2 } = a;
                const [b1, ...b2] = b;
              }
            }
        "#
            .unindent();

            let buffer = cx.new(|cx| language::Buffer::local(text, cx).with_language(language, cx));
            let outline = buffer.read_with(cx, |buffer, _| buffer.snapshot().outline(None));
            assert_eq!(
                outline
                    .items
                    .iter()
                    .map(|item| (item.text.as_str(), item.depth))
                    .collect::<Vec<_>>(),
                &[
                    ("const a1", 0),
                    ("const a2", 0),
                    ("const b1", 0),
                    ("const b2", 0),
                    ("const c1", 0),
                    ("const c2", 0),
                    ("const rest1", 0),
                    ("const d1", 0),
                    ("const e1", 0),
                    ("const h1", 0),
                    ("const rest2", 0),
                    ("function processData()", 0),
                    ("const c1", 1),
                    ("const c2", 1),
                    ("const d1", 1),
                    ("const d2", 1),
                    ("const d3", 1),
                    ("const g1", 1),
                    ("const x", 1),
                    ("const y", 1),
                    ("class DataHandler", 0),
                    ("method()", 1),
                    ("const a1", 2),
                    ("const a2", 2),
                    ("const b1", 2),
                    ("const b2", 2),
                ]
            );
        }
    }

    #[gpui::test]
    async fn test_outline_with_object_properties(cx: &mut TestAppContext) {
        for language in [
            crate::language(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ),
            crate::language("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        ] {
            let text = r#"
            // Object with function properties
            const o = { m() {}, async n() {}, g: function* () {}, h: () => {}, k: function () {} };

            // Object with primitive properties
            const p = { p1: 1, p2: "hello", p3: true };

            // Nested objects
            const q = {
                r: {
                    // won't be included due to one-level depth limit
                    s: 1
                },
                t: 2
            };

            function getData() {
                const local = { x: 1, y: 2 };
                return local;
            }
        "#
            .unindent();

            let buffer = cx.new(|cx| language::Buffer::local(text, cx).with_language(language, cx));
            cx.run_until_parked();
            let outline = buffer.read_with(cx, |buffer, _| buffer.snapshot().outline(None));
            assert_eq!(
                outline
                    .items
                    .iter()
                    .map(|item| (item.text.as_str(), item.depth))
                    .collect::<Vec<_>>(),
                &[
                    ("const o", 0),
                    ("m()", 1),
                    ("async n()", 1),
                    ("g", 1),
                    ("h", 1),
                    ("k", 1),
                    ("const p", 0),
                    ("p1", 1),
                    ("p2", 1),
                    ("p3", 1),
                    ("const q", 0),
                    ("r", 1),
                    ("s", 2),
                    ("t", 1),
                    ("function getData()", 0),
                    ("const local", 1),
                    ("x", 2),
                    ("y", 2),
                ]
            );
        }
    }

    #[gpui::test]
    async fn test_outline_with_nested_object_methods(cx: &mut TestAppContext) {
        for language in [
            crate::language(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ),
            crate::language("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
            crate::language("javascript", tree_sitter_typescript::LANGUAGE_TSX.into()),
        ] {
            let text = r#"
            const o = {
                m() {
                    function nested() {}
                },
                async n() {
                    let a = async () => {};
                },
                g: function* () {
                    let b = () => {};
                },
                h: () => {
                    let c = () => {};
                },
                k: function () {
                    let d = () => {};
                },
            };
        "#
            .unindent();

            let buffer = cx.new(|cx| language::Buffer::local(text, cx).with_language(language, cx));
            let outline = buffer.read_with(cx, |buffer, _| buffer.snapshot().outline(None));
            assert_eq!(
                outline
                    .items
                    .iter()
                    .map(|item| (item.text.as_str(), item.depth))
                    .collect::<Vec<_>>(),
                &[
                    ("const o", 0),
                    ("m()", 1),
                    ("function nested()", 2),
                    ("async n()", 1),
                    ("let a", 2),
                    ("g", 1),
                    ("let b", 2),
                    ("h", 1),
                    ("let c", 2),
                    ("k", 1),
                    ("let d", 2),
                ]
            );
        }
    }
}
