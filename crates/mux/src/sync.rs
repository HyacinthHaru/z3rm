//! # sync
//!
//! 扩展同步模块（§16.6 / Plan 19）。
//! 将本地扩展同步到远程服务器，支持服务端扩展安装。

use anyhow::{Context, Result, anyhow};
use mux_protocol::request::Body as RequestBody;
use mux_protocol::response::Body as ResponseBody;
use std::path::{Path, PathBuf};

// ============================================================================
// §16.6 扩展信息结构
// ============================================================================

/// §16.6 扩展信息：名称、版本、运行时类型。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExtensionInfo {
    /// 扩展名称（目录名）。
    pub name: String,
    /// 扩展版本。
    pub version: String,
    /// 运行时类型（ServerSide / ClientSide / Both）。
    pub runtime_side: ExtensionRuntimeSide,
    /// 是否需要同步。
    pub sync: bool,
    /// 扩展源目录路径。
    pub source_dir: PathBuf,
}

/// §16.6 扩展运行时位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum ExtensionRuntimeSide {
    /// 仅客户端运行。
    ClientSide,
    /// 仅服务端运行。
    ServerSide,
    /// 客户端和服务端都运行。
    Both,
}

// ============================================================================
// §16.6 扩展目录扫描
// ============================================================================

/// §16.6 扫描本地扩展目录，返回需要同步的扩展列表。
pub fn scan_extensions_dir(base_dir: &Path) -> Result<Vec<ExtensionInfo>> {
    let mut extensions = Vec::new();

    if !base_dir.exists() {
        tracing::debug!(path = %base_dir.display(), "扩展目录不存在");
        return Ok(extensions);
    }

    for entry in std::fs::read_dir(base_dir).with_context(|| {
        format!("读取扩展目录失败: {}", base_dir.display())
    })? {
        let entry = entry.context("读取目录条目失败")?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        // §16.6 读取扩展 manifest（JSON）。
        let manifest_path = path.join("extension.json");
        if !manifest_path.exists() {
            continue;
        }

        let manifest = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("读取扩展 manifest 失败: {}", manifest_path.display()))?;

        let info: ExtensionInfo = serde_json::from_str(&manifest).map_or_else(
            |e| {
                tracing::warn!(error = %e, "解析扩展 manifest 失败，使用默认值");
                ExtensionInfo {
                    name: entry.file_name().to_string_lossy().to_string(),
                    version: "0.0.0".to_string(),
                    runtime_side: ExtensionRuntimeSide::ClientSide,
                    sync: true,
                    source_dir: path,
                }
            },
            |m| m,
        );

        // §16.6 仅同步服务端扩展和双端扩展。
        if matches!(info.runtime_side, ExtensionRuntimeSide::ServerSide | ExtensionRuntimeSide::Both)
            && info.sync
        {
            extensions.push(info);
        }
    }

    tracing::info!(
        count = extensions.len(),
        path = %base_dir.display(),
        "扩展扫描完成"
    );
    Ok(extensions)
}

#[allow(dead_code)]
pub fn default_extensions_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("z3rm")
        .join("extensions")
}

// ============================================================================
// §16.6 扩展打包
// ============================================================================

/// §16.6 将扩展目录打包为字节数组（tar.gz）。
pub fn pack_extension(source_dir: &Path) -> Result<Vec<u8>> {


    // §16.6 实际打包逻辑（简化版）。
    let mut archive = tar::Builder::new(Vec::new());
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let mut file = std::fs::File::open(&path)?;
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            archive.append_file(&name, &mut file)?;
        }
    }
    let packed = archive.into_inner()?;

    // §16.6 用 gzip 压缩。
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &packed)?;
    let compressed = encoder.finish()?;

    Ok(compressed)
}

// ============================================================================
// §16.6 通过 MuxDomain 安装远程扩展
// ============================================================================

use crate::MuxDomain;

/// §16.6 通过 mux 协议向远程服务器安装扩展。
///
/// 发送 InstallExtensionRequest → 等待 InstallExtensionResponse。
pub async fn install_remote_extension(
    domain: &MuxDomain,
    name: &str,
    manifest: &[u8],
    source: &[u8],
) -> Result<()> {
    // §16.6 构建 InstallExtensionRequest。
    let body = RequestBody::InstallExtension(
        mux_protocol::InstallExtensionRequest {
            name: name.to_string(),
            manifest: manifest.to_vec(),
            source: source.to_vec(),
        },
    );

    // §16.6 发送请求并等待响应。
    let resp = domain.send_request(body).await
        .context("发送扩展安装请求失败")?;

    // §16.6 检查响应结果。
    if let Some(ResponseBody::ExtensionInstalled(installed)) = &resp.body {
        if installed.success {
            tracing::info!(name = %name, "远程扩展安装成功");
            Ok(())
        } else {
            Err(anyhow!("远程扩展安装失败: {}", installed.error))
        }
    } else {
        Err(anyhow!("远程扩展安装: 意外的响应类型"))
    }
}

// ============================================================================
// §16.6 扩展同步入口
// ============================================================================

/// §16.6 同步本地扩展到远程服务器。
///
/// 扫描本地扩展 → 过滤服务端扩展 → 打包 → 通过 MuxDomain 安装。
pub async fn sync_extensions_to_remote(domain: &MuxDomain, base_dir: &Path) -> Result<()> {
    // §16.6 步骤1：扫描本地扩展。
    let extensions = scan_extensions_dir(base_dir)?;

    if extensions.is_empty() {
        tracing::info!("无需要同步的服务端扩展");
        return Ok(());
    }

    // §16.6 步骤2：逐个同步扩展。
    for ext in &extensions {
        tracing::info!(
            name = %ext.name,
            version = %ext.version,
            "同步扩展到远程"
        );

        // §16.6 读取 manifest。
        let manifest_path = ext.source_dir.join("extension.json");
        let manifest = std::fs::read(&manifest_path)
            .with_context(|| format!("读取扩展 manifest 失败: {:?}", manifest_path))?;

        // §16.6 打包扩展源。
        let source = pack_extension(&ext.source_dir)
            .with_context(|| format!("打包扩展失败: {}", ext.name))?;

        // §16.6 安装到远程。
        install_remote_extension(domain, &ext.name, &manifest, &source)
            .await
            .with_context(|| format!("安装远程扩展失败: {}", ext.name))?;
    }

    tracing::info!(
        count = extensions.len(),
        "扩展同步完成"
    );
    Ok(())
}

// ============================================================================
// §16.6 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_runtime_side_debug() {
        let client = ExtensionRuntimeSide::ClientSide;
        let server = ExtensionRuntimeSide::ServerSide;
        let both = ExtensionRuntimeSide::Both;

        assert_ne!(client, server);
        assert_ne!(client, both);
        assert_ne!(server, both);
    }

    #[test]
    fn test_scan_nonexistent_dir() {
        let temp = tempfile::tempdir().unwrap();
        let nonexistent = temp.path().join("nonexistent");
        let result = scan_extensions_dir(&nonexistent).unwrap();
        assert!(result.is_empty());
    }
}
