// 扩展市场 CLI 命令
// 来源: spec §16.11, Plan 28

use anyhow::{Context as _, Result};
use clap::Parser;
use reqwest_client::ReqwestClient;
use std::path::{Path, PathBuf};

use extension_host::marketplace::{MarketplaceEntry, fetch_registry};

/// z3rm extension marketplace 子命令
/// 来源: spec §16.11
#[derive(Parser, Debug)]
#[command(name = "extension")]
pub struct ExtensionArgs {
    #[command(subcommand)]
    command: ExtensionCommand,
}

#[derive(clap::Subcommand, Debug)]
enum ExtensionCommand {
    /// 搜索市场中的扩展
    Search {
        /// 搜索关键词
        query: String,
        /// 市场注册表 URL
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// 从市场安装扩展
    Install {
        /// 扩展 ID
        id: String,
        /// 扩展安装目录
        #[arg(long)]
        extensions_dir: Option<PathBuf>,
        /// 市场注册表 URL
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// 更新已安装的扩展 (不指定 ID 则更新全部)
    Update {
        /// 只更新这个扩展 ID
        id: Option<String>,
        /// 扩展安装目录
        #[arg(long)]
        extensions_dir: Option<PathBuf>,
        /// 市场注册表 URL
        #[arg(long)]
        registry_url: Option<String>,
        /// 不询问直接更新 (非交互场景必须带)
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// 卸载已安装的扩展
    Uninstall {
        /// 扩展 ID
        id: String,
        /// 扩展安装目录
        #[arg(long)]
        extensions_dir: Option<PathBuf>,
        /// 不询问直接卸载 (非交互场景必须带)
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// 列出已安装的扩展
    List {
        /// 扩展安装目录
        #[arg(long)]
        extensions_dir: Option<PathBuf>,
    },
}

/// 解析 extension marketplace CLI 参数
pub fn parse_extension_args() -> Result<Option<ExtensionArgs>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args[1] != "extension" {
        return Ok(None);
    }

    match ExtensionArgs::try_parse_from(&args[1..]) {
        Ok(parsed) => Ok(Some(parsed)),
        // clap 用 Err 表达 `--help`/`--version`。`exit()` 把 usage 打到 stdout
        // 并以 0 退出，真正的解析错误打到 stderr 并以 2 退出。这里以前返回
        // Ok(None)，于是 `z3rm extension --help` 会掉进 GUI 启动路径。
        Err(error) => error.exit(),
    }
}

/// 运行 extension marketplace 命令
pub async fn run_extension_command(args: ExtensionArgs) -> Result<()> {
    match args.command {
        ExtensionCommand::Search {
            query,
            registry_url,
        } => run_search(&query, registry_url.as_deref()).await,
        ExtensionCommand::Install {
            id,
            extensions_dir,
            registry_url,
        } => run_install(&id, extensions_dir, registry_url.as_deref()).await,
        ExtensionCommand::Update {
            id,
            extensions_dir,
            registry_url,
            yes,
        } => run_update(id.as_deref(), extensions_dir, registry_url.as_deref(), yes).await,
        ExtensionCommand::Uninstall {
            id,
            extensions_dir,
            yes,
        } => run_uninstall(&id, extensions_dir, yes).await,
        ExtensionCommand::List { extensions_dir } => run_list(extensions_dir).await,
    }
}

/// 读取已安装扩展的 `extension.toml` 版本号。
async fn installed_version(extension_dir: &Path) -> Result<Option<String>> {
    let manifest_path = extension_dir.join("extension.toml");
    let manifest = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", manifest_path.display()));
        }
    };
    version_from_manifest(&manifest)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))
}

/// 从 `extension.toml` 里取顶层 `version`。
///
/// 用 toml 解析而不是裁字符串: `version` 也会出现在 `[grammars.x]` 这类子表里，
/// 按行前缀匹配会读到错的那一条。
fn version_from_manifest(manifest: &str) -> Result<Option<String>> {
    let manifest: toml::Value = toml::from_str(manifest)?;
    Ok(manifest
        .get("version")
        .and_then(toml::Value::as_str)
        .map(str::to_string))
}

/// 读版本号，读不出来时报告到 stderr 并返回 `"unknown"`，
/// 让一个坏掉的扩展不至于让整条 list/update 罢工。
async fn installed_version_or_unknown(extension_dir: &Path, id: &str) -> String {
    match installed_version(extension_dir).await {
        Ok(Some(version)) => version,
        Ok(None) => "unknown".to_string(),
        Err(error) => {
            eprintln!("warning: {id}: {error:#}");
            "unknown".to_string()
        }
    }
}

/// 枚举 `extensions_dir` 下已安装的扩展目录。
async fn installed_extension_dirs(extensions_path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = match tokio::fs::read_dir(extensions_path).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", extensions_path.display()));
        }
    };
    let mut dirs = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .context("failed to read extensions directory")?
    {
        let path = entry.path();
        // 点开头的目录是安装用的临时目录 (`.<id>.incoming`) 或编辑器杂物，
        // 不是已安装的扩展。
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if path.is_dir() && !hidden {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn extension_id_from_dir(extension_dir: &Path) -> Option<String> {
    extension_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
}

/// 下载、校验 SHA256 并把扩展落到 `target_dir`。
///
/// install 和 update 走同一条路径。先解压到同级临时目录再整体改名，
/// 中途失败不会留下半个扩展，也不会把新旧版本的文件混在一起。
async fn install_extension_archive(
    http_client: &ReqwestClient,
    entry: &MarketplaceEntry,
    target_dir: &Path,
) -> Result<()> {
    let tar_bytes = extension_host::marketplace::download_extension(
        http_client,
        &entry.download_url,
        &entry.checksum,
    )
    .await
    .context("failed to download extension")?;

    let parent = target_dir.parent().ok_or_else(|| {
        anyhow::anyhow!("invalid extension install path: {}", target_dir.display())
    })?;
    tokio::fs::create_dir_all(parent)
        .await
        .context("failed to create extensions directory")?;

    let staging_dir = parent.join(format!(".{}.incoming", entry.id));
    if tokio::fs::metadata(&staging_dir).await.is_ok() {
        tokio::fs::remove_dir_all(&staging_dir)
            .await
            .context("failed to clear a stale staging directory")?;
    }
    tokio::fs::create_dir_all(&staging_dir)
        .await
        .context("failed to create the staging directory")?;

    let unpack_dir = staging_dir.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::BufReader;
        let cursor = std::io::Cursor::new(tar_bytes);
        let buf_reader = BufReader::new(cursor);
        let decompressed = flate2::bufread::GzDecoder::new(buf_reader);
        let mut archive = tar::Archive::new(decompressed);
        archive.unpack(&unpack_dir).map_err(Into::into)
    })
    .await
    .context("spawn_blocking failed")?
    .context("failed to unpack extension archive")?;

    if tokio::fs::metadata(target_dir).await.is_ok() {
        tokio::fs::remove_dir_all(target_dir)
            .await
            .context("failed to remove the previously installed version")?;
    }
    tokio::fs::rename(&staging_dir, target_dir)
        .await
        .context("failed to move the extension into place")?;
    Ok(())
}

/// 交互确认。非交互场景 (管道/CI) 直接报错要求显式 `--yes`，
/// 免得命令悄悄挂在等一个永远不会来的回车上。
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("{prompt} — stdin is not a terminal; pass --yes to confirm");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout()
        .flush()
        .context("failed to flush the confirmation prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed to read the confirmation answer")?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// 搜索扩展
/// 来源: spec §16.11
async fn run_search(query: &str, registry_url: Option<&str>) -> Result<()> {
    let url = registry_url.unwrap_or(extension_host::marketplace::DEFAULT_REGISTRY_URL);
    let http_client = ReqwestClient::new();

    let registry = fetch_registry(&http_client, url)
        .await
        .context("failed to fetch marketplace registry")?;

    let results = registry.search(query);

    if results.is_empty() {
        println!("no extensions found matching '{}'", query);
        return Ok(());
    }

    println!(
        "{:<20} {:<20} {:<12} {:<15} {}",
        "ID", "NAME", "VERSION", "AUTHOR", "DESCRIPTION"
    );
    println!("{}", "-".repeat(87));
    for entry in &results {
        println!(
            "{:<20} {:<20} {:<12} {:<15} {}",
            entry.id, entry.name, entry.version, entry.author, entry.description
        );
    }

    println!("\nfound {} extension(s)", results.len());
    Ok(())
}

/// 从市场安装扩展
/// 来源: spec §16.11
async fn run_install(
    id: &str,
    extensions_dir: Option<PathBuf>,
    registry_url: Option<&str>,
) -> Result<()> {
    let url = registry_url.unwrap_or(extension_host::marketplace::DEFAULT_REGISTRY_URL);
    let http_client = ReqwestClient::new();

    let registry = fetch_registry(&http_client, url)
        .await
        .context("failed to fetch marketplace registry")?;

    let entry = registry
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("extension '{}' not found in marketplace", id))?;

    println!(
        "downloading {} {} from marketplace...",
        entry.name, entry.version
    );

    let target_dir = extensions_dir
        .unwrap_or_else(|| paths::extensions_dir().clone())
        .join(&entry.id);
    install_extension_archive(&http_client, entry, &target_dir).await?;

    println!(
        "installed {} {} to {:?}",
        entry.name, entry.version, target_dir
    );
    Ok(())
}

/// 更新已安装的扩展
/// 来源: spec §16.11
async fn run_update(
    only_id: Option<&str>,
    extensions_dir: Option<PathBuf>,
    registry_url: Option<&str>,
    assume_yes: bool,
) -> Result<()> {
    let extensions_path = extensions_dir.unwrap_or_else(|| paths::extensions_dir().clone());
    let installed = installed_extension_dirs(&extensions_path).await?;
    let installed: Vec<PathBuf> = match only_id {
        Some(id) => installed
            .into_iter()
            .filter(|dir| extension_id_from_dir(dir).as_deref() == Some(id))
            .collect(),
        None => installed,
    };

    if installed.is_empty() {
        match only_id {
            Some(id) => anyhow::bail!("extension '{id}' is not installed in {extensions_path:?}"),
            None => {
                println!("no installed extensions found");
                return Ok(());
            }
        }
    }

    let url = registry_url.unwrap_or(extension_host::marketplace::DEFAULT_REGISTRY_URL);
    let http_client = ReqwestClient::new();

    let registry = fetch_registry(&http_client, url)
        .await
        .context("failed to fetch marketplace registry")?;

    println!("checking for updates...");
    println!();

    let mut updated = 0usize;
    for extension_dir in &installed {
        let Some(id) = extension_id_from_dir(extension_dir) else {
            continue;
        };
        let Some(entry) = registry.get(&id) else {
            println!("{}: (not found in marketplace)", id);
            continue;
        };

        let current = installed_version_or_unknown(extension_dir, &id).await;
        if !update_is_available(&current, &entry.version) {
            println!("{}: {} (up to date)", id, current);
            continue;
        }

        println!(
            "{}: {} -> {} (update available)",
            id, current, entry.version
        );
        let prompt = format!("update {id} to {}?", entry.version);
        if !assume_yes && !confirm(&prompt)? {
            println!("{}: skipped", id);
            continue;
        }
        install_extension_archive(&http_client, entry, extension_dir)
            .await
            .with_context(|| format!("failed to update extension '{id}'"))?;
        println!("{}: updated to {}", id, entry.version);
        updated += 1;
    }

    if updated == 0 {
        println!("\nno extensions were updated");
    } else {
        println!("\nupdated {} extension(s)", updated);
    }

    Ok(())
}

/// 判断市场版本是否比已安装版本新。
///
/// 两边都能解析成 semver 时按 semver 比较；解析不出来 (例如 manifest 缺
/// `version`) 则退化成"不相等即视为有更新"，宁可多提示一次也不漏掉。
fn update_is_available(installed: &str, available: &semver::Version) -> bool {
    match semver::Version::parse(installed) {
        Ok(installed) => *available > installed,
        Err(_) => installed != available.to_string(),
    }
}

/// 卸载已安装的扩展
/// 来源: spec §16.11
async fn run_uninstall(id: &str, extensions_dir: Option<PathBuf>, assume_yes: bool) -> Result<()> {
    let extensions_path = extensions_dir.unwrap_or_else(|| paths::extensions_dir().clone());
    let target_dir = extensions_path.join(id);

    if tokio::fs::metadata(&target_dir).await.is_err() {
        anyhow::bail!("extension '{id}' is not installed in {extensions_path:?}");
    }

    let version = installed_version_or_unknown(&target_dir, id).await;
    let prompt = format!("uninstall {id} {version} from {target_dir:?}?");
    if !assume_yes && !confirm(&prompt)? {
        println!("{}: kept", id);
        return Ok(());
    }

    tokio::fs::remove_dir_all(&target_dir)
        .await
        .with_context(|| format!("failed to remove {}", target_dir.display()))?;
    println!("uninstalled {} {}", id, version);
    Ok(())
}

/// 列出已安装的扩展
/// 来源: spec §16.11
async fn run_list(extensions_dir: Option<PathBuf>) -> Result<()> {
    let extensions_path = extensions_dir.unwrap_or_else(|| paths::extensions_dir().clone());
    let installed = installed_extension_dirs(&extensions_path).await?;

    let mut entries: Vec<(String, String)> = Vec::new();
    for extension_dir in &installed {
        let Some(id) = extension_id_from_dir(extension_dir) else {
            continue;
        };
        let version = installed_version_or_unknown(extension_dir, &id).await;
        entries.push((id, version));
    }

    if entries.is_empty() {
        println!("no installed extensions found");
        return Ok(());
    }

    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    println!("{:<20} {:<15}", "EXTENSION", "VERSION");
    println!("{}", "-".repeat(36));
    for (name, version) in &entries {
        println!("{:<20} {:<15}", name, version);
    }

    println!("\n{} extension(s) installed", entries.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_version_comes_from_the_top_level_table() {
        // `[grammars.demo]` 里也有 version;按行前缀裁字符串会读到 9.9.9。
        let manifest = "id = \"demo\"\nname = \"Demo\"\nversion = \"1.2.3\"\n\n\
             [grammars.demo]\nversion = \"9.9.9\"\n";
        assert_eq!(
            version_from_manifest(manifest).expect("parse"),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn manifest_without_version_is_not_an_error() {
        assert_eq!(
            version_from_manifest("id = \"demo\"\n").expect("parse"),
            None
        );
    }

    #[test]
    fn malformed_manifest_is_reported() {
        version_from_manifest("id = \n").expect_err("malformed toml must fail");
    }

    #[test]
    fn update_is_available_compares_semver_not_strings() {
        let available = semver::Version::parse("1.10.0").expect("semver");
        // 字符串比较会认为 "1.9.0" > "1.10.0";semver 不会。
        assert!(update_is_available("1.9.0", &available));
        assert!(!update_is_available("1.10.0", &available));
        assert!(!update_is_available("2.0.0", &available));
        // 版本号读不出来时退化成"不相等即视为有更新"。
        assert!(update_is_available("unknown", &available));
    }
}
