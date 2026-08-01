// 扩展市场 CLI 命令
// 来源: spec §16.11, Plan 28

use anyhow::{Context as _, Result};
use clap::Parser;
use http_client::HttpClient;
use reqwest_client::ReqwestClient;
use std::path::PathBuf;

use extension_host::marketplace::{MarketplaceRegistry, fetch_registry};

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
    /// 检查已安装扩展的更新
    Update {
        /// 扩展安装目录
        #[arg(long)]
        extensions_dir: Option<PathBuf>,
        /// 市场注册表 URL
        #[arg(long)]
        registry_url: Option<String>,
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
    if args.len() < 3 {
        return Ok(None);
    }

    if args[1] != "extension" {
        return Ok(None);
    }

    let subcommand = &args[2];
    match subcommand.as_str() {
        "search" | "install" | "update" | "list" => {}
        _ => return Ok(None),
    }

    let rest = &args[1..];
    let extension_args =
        ExtensionArgs::try_parse_from(rest).context("failed to parse extension args")?;
    Ok(Some(extension_args))
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
            extensions_dir,
            registry_url,
        } => run_update(extensions_dir, registry_url.as_deref()).await,
        ExtensionCommand::List { extensions_dir } => run_list(extensions_dir).await,
    }
}

/// 搜索扩展
/// 来源: spec §16.11
async fn run_search(query: &str, registry_url: Option<&str>) -> Result<()> {
    let url = registry_url.unwrap_or("https://extensions.z3rm.dev/registry.json");
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
    let url = registry_url.unwrap_or("https://extensions.z3rm.dev/registry.json");
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

    // 下载扩展包并校验 checksum
    let tar_bytes = extension_host::marketplace::download_extension(
        &http_client,
        &entry.download_url,
        &entry.checksum,
    )
    .await
    .context("failed to download extension")?;

    // 解压到目标目录
    let target_dir = extensions_dir
        .unwrap_or_else(|| paths::extensions_dir().clone())
        .join(&entry.id);

    tokio::fs::create_dir_all(&target_dir)
        .await
        .context("failed to create extensions directory")?;

    // 同步解压 tar.gz
    let target_dir_clone = target_dir.clone();
    let tar_bytes_clone = tar_bytes.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::BufReader;
        let cursor = std::io::Cursor::new(tar_bytes_clone);
        let buf_reader = BufReader::new(cursor);
        let decompressed = flate2::bufread::GzDecoder::new(buf_reader);
        let mut archive = tar::Archive::new(decompressed);
        archive.unpack(&target_dir_clone).map_err(Into::into)
    })
    .await
    .context("spawn_blocking failed")?
    .context("failed to unpack extension archive")?;

    println!(
        "installed {} {} to {:?}",
        entry.name, entry.version, target_dir
    );
    Ok(())
}

/// 检查更新
/// 来源: spec §16.11
async fn run_update(extensions_dir: Option<PathBuf>, registry_url: Option<&str>) -> Result<()> {
    let extensions_path = extensions_dir.unwrap_or_else(|| paths::extensions_dir().clone());

    let installed = tokio::fs::read_dir(&extensions_path).await;
    let installed = match installed {
        Ok(mut entries) => {
            let mut result = Vec::new();
            while let Some(entry) = entries
                .next_entry()
                .await
                .context("failed to read directory")?
            {
                let path = entry.path();
                if path.is_dir() {
                    result.push(path);
                }
            }
            result
        }
        Err(_) => {
            println!("no installed extensions found");
            return Ok(());
        }
    };

    if installed.is_empty() {
        println!("no installed extensions found");
        return Ok(());
    }

    let url = registry_url.unwrap_or("https://extensions.z3rm.dev/registry.json");
    let http_client = ReqwestClient::new();

    let registry = fetch_registry(&http_client, url)
        .await
        .context("failed to fetch marketplace registry")?;

    println!("checking for updates...");
    println!();

    let mut has_updates = false;
    for ext_dir in &installed {
        let ext_name = match ext_dir.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => continue,
        };

        if let Some(entry) = registry.get(&ext_name) {
            let manifest_path = ext_dir.join("extension.toml");
            if tokio::fs::metadata(&manifest_path).await.is_ok() {
                let manifest_content = tokio::fs::read_to_string(&manifest_path)
                    .await
                    .context(format!("failed to read manifest for {}", ext_name))?;

                let installed_version: String = manifest_content
                    .lines()
                    .find(|line| line.starts_with("version"))
                    .and_then(|line| {
                        line.split('=')
                            .nth(1)
                            .map(|v| v.trim().trim_matches('"').to_string())
                    })
                    .unwrap_or_else(|| "unknown".to_string());

                if entry.version.to_string() != installed_version {
                    has_updates = true;
                    println!(
                        "{}: {} -> {} (update available)",
                        ext_name, installed_version, entry.version
                    );
                } else {
                    println!("{}: {} (up to date)", ext_name, installed_version);
                }
            }
        } else {
            println!("{}: (not found in marketplace)", ext_name);
        }
    }

    if !has_updates {
        println!("\nall extensions are up to date");
    }

    Ok(())
}

/// 列出已安装的扩展
/// 来源: spec §16.11
async fn run_list(extensions_dir: Option<PathBuf>) -> Result<()> {
    let extensions_path = extensions_dir.unwrap_or_else(|| paths::extensions_dir().clone());

    let installed = tokio::fs::read_dir(&extensions_path).await;
    let mut entries: Vec<(String, String)> = Vec::new();

    match installed {
        Ok(mut dir) => {
            while let Some(entry) = dir.next_entry().await.context("failed to read directory")? {
                let path = entry.path();
                if path.is_dir() {
                    let name: String = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    let manifest_path = path.join("extension.toml");
                    let version = if tokio::fs::metadata(&manifest_path).await.is_ok() {
                        match tokio::fs::read_to_string(&manifest_path).await {
                            Ok(content) => content
                                .lines()
                                .find(|line| line.starts_with("version"))
                                .and_then(|line| {
                                    line.split('=')
                                        .nth(1)
                                        .map(|v| v.trim().trim_matches('"').to_string())
                                })
                                .unwrap_or_else(|| "unknown".to_string()),
                            Err(_) => "unknown".to_string(),
                        }
                    } else {
                        "unknown".to_string()
                    };

                    entries.push((name, version));
                }
            }
        }
        Err(_) => {
            println!("no installed extensions found");
            return Ok(());
        }
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
