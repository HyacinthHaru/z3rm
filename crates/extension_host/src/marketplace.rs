// 扩展市场注册表
// 来源: spec §16.11, Plan 28

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt;
use http_client::{AsyncBody, HttpClient};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::ExtensionMetadata;

/// 市场注册表默认 URL
pub const DEFAULT_REGISTRY_URL: &str = "https://extensions.z3rm.dev/registry.json";

/// 市场扩展条目 (registry.json 中的单条记录)
/// 来源: spec §16.11
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    /// 扩展唯一标识符
    pub id: String,
    /// 扩展显示名称
    pub name: String,
    /// 版本号 (semver)
    pub version: Version,
    /// 扩展描述
    pub description: String,
    /// 作者
    pub author: String,
    /// 源码仓库 URL
    pub repository: Option<String>,
    /// 下载 tar.gz 的 URL
    pub download_url: String,
    /// SHA256 校验和
    pub checksum: String,
}

/// 市场注册表响应格式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceRegistry {
    /// 注册表版本号
    #[serde(default)]
    pub registry_version: u32,
    /// 扩展列表
    #[serde(default)]
    pub entries: Vec<MarketplaceEntry>,
}

impl MarketplaceRegistry {
    /// 按 ID 查找扩展条目
    pub fn get(&self, id: &str) -> Option<&MarketplaceEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// 搜索扩展 (按名称/描述模糊匹配)
    pub fn search(&self, query: &str) -> Vec<&MarketplaceEntry> {
        let query_lower = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&query_lower)
                    || e.description.to_lowercase().contains(&query_lower)
                    || e.id.to_lowercase().contains(&query_lower)
            })
            .collect()
    }
}

/// 从 URL 获取市场注册表
/// 来源: spec §16.11
pub async fn fetch_registry(
    http_client: &dyn HttpClient,
    url: &str,
) -> Result<MarketplaceRegistry> {
    let mut response = http_client
        .get(url, AsyncBody::default(), true)
        .await
        .context("failed to fetch marketplace registry")?;

    if !response.status().is_success() {
        bail!("marketplace registry request failed: {}", response.status());
    }

    let mut body_bytes = Vec::new();
    response.body_mut().read_to_end(&mut body_bytes).await?;

    let registry: MarketplaceRegistry =
        serde_json::from_slice(&body_bytes).context("failed to parse marketplace registry")?;

    Ok(registry)
}

/// 从 URL 下载扩展 tar.gz，并校验 SHA256
/// 来源: spec §16.11
pub async fn download_extension(
    http_client: &dyn HttpClient,
    download_url: &str,
    expected_checksum: &str,
) -> Result<Vec<u8>> {
    let mut response = http_client
        .get(download_url, AsyncBody::default(), true)
        .await
        .context("failed to download extension")?;

    if !response.status().is_success() {
        bail!("extension download failed: {}", response.status());
    }

    let mut body_bytes = Vec::new();
    response.body_mut().read_to_end(&mut body_bytes).await?;

    // 校验 SHA256 校验和
    let computed_checksum = format!("{:x}", sha2::Sha256::digest(&body_bytes));
    if computed_checksum != expected_checksum {
        bail!(
            "extension checksum mismatch: expected {}, got {}",
            expected_checksum,
            computed_checksum
        );
    }

    Ok(body_bytes)
}

/// 将 MarketplaceEntry 转换为 ExtensionMetadata (用于兼容现有 ExtensionStore API)
/// 来源: spec §16.11
pub fn marketplace_entry_to_metadata(entry: &MarketplaceEntry) -> ExtensionMetadata {
    use crate::ExtensionMetadataManifest;

    ExtensionMetadata {
        id: Arc::from(entry.id.clone()),
        dev: false,
        manifest: ExtensionMetadataManifest {
            version: Arc::from(entry.version.to_string()),
            schema_version: None,
            wasm_api_version: None,
            name: entry.name.clone(),
            description: Some(entry.description.clone()),
            repository: entry.repository.clone(),
            authors: vec![entry.author.clone()],
            provides_list: Vec::new(),
        },
        published_at: None,
        download_count: 0,
    }
}

#[cfg(all(test, feature = "z3rm-migration"))]
mod tests {
    use super::*;
    use http_client::FakeHttpClient;
    use std::io::Write;

    fn make_registry_json(entries: Vec<MarketplaceEntry>) -> String {
        let registry = MarketplaceRegistry {
            registry_version: 1,
            entries,
        };
        serde_json::to_string(&registry).unwrap()
    }

    fn make_tar_gz() -> Vec<u8> {
        // 最小可接受的 tar.gz 文件 (空归档)
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        // 写入最小 tar header (512 字节零)
        let tar_header = [0u8; 512];
        encoder.write_all(&tar_header).unwrap();
        encoder.finish().unwrap()
    }

    fn compute_checksum(bytes: &[u8]) -> String {
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    #[tokio::test]
    async fn test_fetch_registry() {
        let entries = vec![
            MarketplaceEntry {
                id: "rust".into(),
                name: "Rust".into(),
                version: Version::new(1, 0, 0),
                description: "Rust language support".into(),
                author: "z3rm".into(),
                repository: Some("https://github.com/z3rm/rust-ext".into()),
                download_url: "https://extensions.z3rm.dev/rust-1.0.0.tar.gz".into(),
                checksum: "abc123".into(),
            },
            MarketplaceEntry {
                id: "python".into(),
                name: "Python".into(),
                version: Version::new(2, 1, 0),
                description: "Python language support".into(),
                author: "z3rm".into(),
                repository: None,
                download_url: "https://extensions.z3rm.dev/python-2.1.0.tar.gz".into(),
                checksum: "def456".into(),
            },
        ];

        let json = make_registry_json(entries.clone());
        let json_bytes: Vec<u8> = json.clone().into_bytes();
        let fake = FakeHttpClient::create(move |_| {
            let body = json_bytes.clone();
            async move { Ok(http::Response::new(AsyncBody::from(body))) }
        });

        let registry = fetch_registry(&fake, "https://extensions.z3rm.dev/registry.json").await?;
        assert_eq!(registry.entries.len(), 2);
        assert_eq!(registry.entries[0].id, "rust");
        assert_eq!(registry.entries[1].id, "python");
    }

    #[test]
    fn test_search() {
        let registry = MarketplaceRegistry {
            registry_version: 1,
            entries: vec![
                MarketplaceEntry {
                    id: "rust".into(),
                    name: "Rust".into(),
                    version: Version::new(1, 0, 0),
                    description: "Rust language support".into(),
                    author: "z3rm".into(),
                    repository: None,
                    download_url: "https://example.com/rust.tar.gz".into(),
                    checksum: "abc".into(),
                },
                MarketplaceEntry {
                    id: "python".into(),
                    name: "Python".into(),
                    version: Version::new(1, 0, 0),
                    description: "Python language support".into(),
                    author: "z3rm".into(),
                    repository: None,
                    download_url: "https://example.com/python.tar.gz".into(),
                    checksum: "def".into(),
                },
            ],
        };

        // 搜索 "rust" 应匹配 rust
        let results = registry.search("rust");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "rust");

        // 搜索 "language" 应匹配两条 (description 都包含 "language")
        let results = registry.search("language");
        assert_eq!(results.len(), 2);

        // 搜索 "java" 应无匹配
        let results = registry.search("java");
        assert!(results.is_empty());
    }

    #[test]
    fn test_get_by_id() {
        let registry = MarketplaceRegistry {
            registry_version: 1,
            entries: vec![MarketplaceEntry {
                id: "go".into(),
                name: "Go".into(),
                version: Version::new(1, 0, 0),
                description: "Go language support".into(),
                author: "z3rm".into(),
                repository: None,
                download_url: "https://example.com/go.tar.gz".into(),
                checksum: "xyz".into(),
            }],
        };

        assert!(registry.get("go").is_some());
        assert!(registry.get("rust").is_none());
    }

    #[tokio::test]
    async fn test_download_with_checksum() {
        let tar_bytes = make_tar_gz();
        let checksum = compute_checksum(&tar_bytes);

        let tar_bytes_clone = tar_bytes.clone();
        let fake = FakeHttpClient::create(move |_| {
            let body = tar_bytes_clone.clone();
            async move { Ok(http::Response::new(AsyncBody::from(body))) }
        });

        // 正确的校验和应成功
        let result = download_extension(&fake, "https://example.com/ext.tar.gz", &checksum).await;
        assert!(result.is_ok());

        // 错误的校验和应失败
        let tar_bytes_clone2 = tar_bytes.clone();
        let fake2 = FakeHttpClient::create(move |_| {
            let body = tar_bytes_clone2.clone();
            async move { Ok(http::Response::new(AsyncBody::from(body))) }
        });
        let result =
            download_extension(&fake2, "https://example.com/ext.tar.gz", "wrong_checksum").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[test]
    fn test_entry_to_metadata() {
        let entry = MarketplaceEntry {
            id: "test".into(),
            name: "Test Extension".into(),
            version: Version::new(0, 1, 0),
            description: "A test extension".into(),
            author: "tester".into(),
            repository: Some("https://github.com/test".into()),
            download_url: "https://example.com/test.tar.gz".into(),
            checksum: "abc".into(),
        };

        let metadata = marketplace_entry_to_metadata(&entry);
        assert_eq!(metadata.id.as_ref(), "test");
        assert!(!metadata.dev);
        assert_eq!(metadata.manifest.name, "Test Extension");
        assert_eq!(metadata.manifest.authors, vec!["tester"]);
    }
}
