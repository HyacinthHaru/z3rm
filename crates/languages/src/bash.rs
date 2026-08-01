use anyhow::Result;
use async_trait::async_trait;
use language::{LanguageServerName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::LanguageServerBinary;
use semver::Version;
use std::{future::Future, path::PathBuf, sync::Arc};
use util::ResultExt;

/// 巴什语言服务器适配器 (spec §3.1 L1)
/// node_runtime crate 已删除，不再支持 npm 安装
pub struct BashLspAdapter;

impl BashLspAdapter {
    const PACKAGE_NAME: &str = "bash-language-server";
}

impl LspInstaller for BashLspAdapter {
    type BinaryVersion = Version;

    async fn cached_server_binary(
        &self,
        _container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        // node_runtime 已删除，无法检查缓存的 npm 包
        None
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &gpui::AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which(Self::PACKAGE_NAME.as_ref()).await?;
        let env = delegate.shell_env().await;

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["start".into()],
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

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut gpui::AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        // node_runtime 已删除，无法查询 npm 版本
        anyhow::bail!("npm package version lookup unavailable (node_runtime removed)")
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        _container_dir: PathBuf,
        _delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async { anyhow::bail!("language server installation unavailable (node_runtime removed)") }
    }
}

#[async_trait(?Send)]
impl LspAdapter for BashLspAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static(Self::PACKAGE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, BorrowAppContext, Context, TestAppContext};
    use language::{AutoindentMode, Buffer};
    use settings::SettingsStore;
    use std::num::NonZeroU32;
    use unindent::Unindent;
    use util::test::marked_text_offsets;

    #[gpui::test]
    async fn test_bash_autoindent(cx: &mut TestAppContext) {
        cx.executor().set_block_on_ticks(usize::MAX..=usize::MAX);
        let language = crate::language("bash", tree_sitter_bash::LANGUAGE.into());
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2)
                });
            });
        });

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            let expect_indents_to =
                |buffer: &mut Buffer, cx: &mut Context<Buffer>, input: &str, expected: &str| {
                    buffer.edit(
                        [(0..buffer.len(), input)],
                        Some(AutoindentMode::EachLine),
                        cx,
                    );
                    assert_eq!(buffer.text(), expected);
                };

            // Do not indent after shebang
            expect_indents_to(
                &mut buffer,
                cx,
                "#!/usr/bin/env bash\n#",
                "#!/usr/bin/env bash\n#",
            );

            // indent function correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "function name() {\necho \"Hello, World!\"\n}",
                "function name() {\n  echo \"Hello, World!\"\n}",
            );

            // indent if-else correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "if true;then\nfoo\nelse\nbar\nfi",
                "if true;then\n  foo\nelse\n  bar\nfi",
            );

            // indent if-elif-else correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "if true;then\nfoo\nelif true;then\nbar\nelse\nbar\nfi",
                "if true;then\n  foo\nelif true;then\n  bar\nelse\n  bar\nfi",
            );

            // indent case-when-else correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "case $1 in\nfoo) echo \"Hello, World!\";;\n*) echo \"Unknown argument\";;\nesac",
                "case $1 in\n  foo) echo \"Hello, World!\";;\n  *) echo \"Unknown argument\";;\nesac",
            );

            // indent for-loop correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "for i in {1..10};do\nfoo\ndone",
                "for i in {1..10};do\n  foo\ndone",
            );

            // indent while-loop correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "while true; do\nfoo\ndone",
                "while true; do\n  foo\ndone",
            );

            // indent array correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "array=(\n1\n2\n3\n)",
                "array=(\n  1\n  2\n  3\n)",
            );

            // indents non-"function" function correctly
            expect_indents_to(
                &mut buffer,
                cx,
                "foo() {\necho \"Hello, World!\"\n}",
                "foo() {\n  echo \"Hello, World!\"\n}",
            );

            let (input, offsets) = marked_text_offsets(
                &r#"
                if foo; then
                  1ˇ
                else
                  3
                fi
                "#
                .unindent(),
            );

            buffer.edit([(0..buffer.len(), input)], None, cx);
            buffer.edit(
                [(offsets[0]..offsets[0], "\n")],
                Some(AutoindentMode::EachLine),
                cx,
            );

            assert_eq!(buffer.text(), "if foo; then\n  1\n  \nelse\n  3\nfi");
        })
        .await;
    }
}
