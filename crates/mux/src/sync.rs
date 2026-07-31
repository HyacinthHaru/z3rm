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

/// §16.8 扩展 manifest 文件名。`[runtime] side` 就声明在这里。
const EXTENSION_MANIFEST: &str = "extension.toml";

/// §16.6 扩展信息：名称、版本、运行时类型。
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
///
/// manifest 解析失败会中止整次扫描：静默降级成"客户端扩展"会让一个真正的
/// 服务端扩展被悄悄跳过，同步"成功"但远端什么都没装。
pub fn scan_extensions_dir(base_dir: &Path) -> Result<Vec<ExtensionInfo>> {
    let mut extensions = Vec::new();

    if !base_dir.exists() {
        tracing::debug!(path = %base_dir.display(), "扩展目录不存在");
        return Ok(extensions);
    }

    for entry in std::fs::read_dir(base_dir)
        .with_context(|| format!("读取扩展目录失败: {}", base_dir.display()))?
    {
        let entry = entry.context("读取目录条目失败")?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        if !path.join(EXTENSION_MANIFEST).exists() {
            continue;
        }

        let info = read_extension_manifest(&path)?;

        // §16.6 仅同步服务端扩展和双端扩展。
        if matches!(
            info.runtime_side,
            ExtensionRuntimeSide::ServerSide | ExtensionRuntimeSide::Both
        ) && info.sync
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

/// §16.8 从扩展目录读取 `extension.toml` 并解析出同步需要的字段。
fn read_extension_manifest(source_dir: &Path) -> Result<ExtensionInfo> {
    let manifest_path = source_dir.join(EXTENSION_MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("读取扩展 manifest 失败: {}", manifest_path.display()))?;
    let fields = parse_manifest_fields(&manifest)
        .with_context(|| format!("解析扩展 manifest 失败: {}", manifest_path.display()))?;

    let side = fields.runtime_side.ok_or_else(|| {
        anyhow!(
            "扩展 manifest 缺少 [runtime] side: {}",
            manifest_path.display()
        )
    })?;
    let runtime_side = match side.as_str() {
        "client" => ExtensionRuntimeSide::ClientSide,
        "server" => ExtensionRuntimeSide::ServerSide,
        "both" => ExtensionRuntimeSide::Both,
        other => {
            return Err(anyhow!(
                "扩展 manifest 的 [runtime] side 取值无法识别 '{}': {}",
                other,
                manifest_path.display()
            ));
        }
    };

    let directory_name = source_dir
        .file_name()
        .ok_or_else(|| anyhow!("扩展目录没有名字: {}", source_dir.display()))?
        .to_string_lossy()
        .to_string();

    Ok(ExtensionInfo {
        name: fields.name.unwrap_or(directory_name),
        version: fields.version.unwrap_or_else(|| "0.0.0".to_string()),
        runtime_side,
        // 未声明时默认参与同步：runtime_side 已经限定了范围。
        sync: fields.sync.unwrap_or(true),
        source_dir: source_dir.to_path_buf(),
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ManifestFields {
    name: Option<String>,
    version: Option<String>,
    runtime_side: Option<String>,
    sync: Option<bool>,
}

/// §16.8 从 `extension.toml` 中取出同步需要的标量字段。
///
/// mux crate 没有 TOML 依赖，而同步只关心四个标量
/// （`name` / `version` / `[runtime] side` / `[runtime] sync`），所以这里只读
/// 这个子集：逐行扫描 `key = value`，记录当前 section，跳过注释、数组与内联
/// 表这些用不到的形态。`name` / `version` 同时接受顶层和 `[extension]` 下的
/// 写法（仓库里两种 manifest 都存在）。真正的必需字段缺失或取值无法识别由
/// `read_extension_manifest` 报错，不做静默降级。
fn parse_manifest_fields(manifest: &str) -> Result<ManifestFields> {
    let mut fields = ManifestFields::default();
    let mut section = String::new();

    for line in manifest.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            // `[[array.of.tables]]` 里没有我们要的标量，按未知 section 处理。
            section = header
                .strip_suffix(']')
                .unwrap_or(header)
                .trim_matches(['[', ']'])
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match (section.as_str(), key) {
            ("" | "extension", "name") => fields.name = parse_toml_string(value),
            ("" | "extension", "version") => fields.version = parse_toml_string(value),
            ("runtime", "side") => fields.runtime_side = parse_toml_string(value),
            ("runtime", "sync") => fields.sync = value.parse::<bool>().ok(),
            _ => {}
        }
    }

    Ok(fields)
}

/// 去掉行尾注释。字符串字面量里的 `#` 必须保留（例如 `name = "a#b"`）。
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (index, character) in line.char_indices() {
        match character {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..index],
            _ => {}
        }
    }
    line
}

/// 只接受单行双引号字符串；数组、内联表等形态返回 `None`（调用方按缺失处理）。
fn parse_toml_string(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('"') {
        return None;
    }
    Some(inner.to_string())
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
///
/// 打包整棵目录树：扩展的 JS 入口、assets 通常在子目录里，只 append 顶层文件
/// 会打出一个装到远端也跑不起来的残包。归档内的路径相对于 `source_dir`。
pub fn pack_extension(source_dir: &Path) -> Result<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());
    // 不跟随 symlink：扩展目录里的 symlink 可能指向 worktree 之外，跟随会把
    // 无关文件打进包里。
    archive.follow_symlinks(false);
    archive
        .append_dir_all("", source_dir)
        .with_context(|| format!("打包扩展目录失败: {}", source_dir.display()))?;
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
        let manifest_path = ext.source_dir.join(EXTENSION_MANIFEST);
        let manifest = std::fs::read(&manifest_path)
            .with_context(|| format!("读取扩展 manifest 失败: {}", manifest_path.display()))?;

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

    fn write_extension(base_dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let directory = base_dir.join(name);
        std::fs::create_dir_all(&directory).expect("create extension directory");
        std::fs::write(directory.join(EXTENSION_MANIFEST), manifest).expect("write manifest");
        directory
    }

    /// 仓库里所有扩展用的都是 `extension.toml`；找 `extension.json` 会让扫描
    /// 恒返回空列表，同步链路整条失效。
    #[test]
    fn scan_reads_toml_manifests_and_filters_by_runtime_side() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_extension(
            temp.path(),
            "server-side",
            "[extension]\nname = \"server-side\"\nversion = \"1.2.3\"\n\n[runtime]\nside = \"server\"\n",
        );
        write_extension(
            temp.path(),
            "both-sides",
            "id = \"both-sides\"\nname = \"both-sides\"\nversion = \"0.2.0\"\n\n[runtime]\nside = \"both\"\n",
        );
        write_extension(
            temp.path(),
            "client-side",
            "[extension]\nname = \"client-side\"\nversion = \"0.1.0\"\n\n[runtime]\nside = \"client\"\n",
        );
        // 没有 manifest 的目录直接跳过。
        std::fs::create_dir_all(temp.path().join("not-an-extension")).expect("create plain dir");

        let mut found = scan_extensions_dir(temp.path()).expect("scan");
        found.sort_by(|left, right| left.name.cmp(&right.name));

        let names: Vec<&str> = found.iter().map(|info| info.name.as_str()).collect();
        assert_eq!(names, vec!["both-sides", "server-side"]);
        assert_eq!(found[1].version, "1.2.3");
        assert_eq!(found[0].runtime_side, ExtensionRuntimeSide::Both);
        assert_eq!(found[1].runtime_side, ExtensionRuntimeSide::ServerSide);
    }

    #[test]
    fn scan_fails_loudly_on_a_manifest_it_cannot_understand() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_extension(
            temp.path(),
            "no-runtime",
            "[extension]\nname = \"no-runtime\"\nversion = \"0.1.0\"\n",
        );
        let error = scan_extensions_dir(temp.path()).expect_err("missing [runtime] side must fail");
        assert!(
            error.to_string().contains("[runtime] side"),
            "unexpected error: {error:#}"
        );

        let temp = tempfile::tempdir().expect("temp dir");
        write_extension(
            temp.path(),
            "bad-side",
            "[extension]\nname = \"bad-side\"\n\n[runtime]\nside = \"middle\"\n",
        );
        let error = scan_extensions_dir(temp.path()).expect_err("unknown side must fail");
        assert!(
            error.to_string().contains("middle"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn manifest_parser_reads_scalars_and_ignores_unrelated_shapes() {
        let manifest = "\
id = \"demo\"           # trailing comment
name = \"demo\"
version = \"0.4.0\"
authors = [\"someone\"]

[runtime]
side = \"both\"
sync = false

[[capabilities]]
kind = \"process:exec\"
name = \"not-the-extension-name\"
";
        let fields = parse_manifest_fields(manifest).expect("parse manifest");
        assert_eq!(
            fields,
            ManifestFields {
                name: Some("demo".to_string()),
                version: Some("0.4.0".to_string()),
                runtime_side: Some("both".to_string()),
                sync: Some(false),
            }
        );
    }

    #[test]
    fn manifest_sync_false_is_honored() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_extension(
            temp.path(),
            "opted-out",
            "[extension]\nname = \"opted-out\"\n\n[runtime]\nside = \"server\"\nsync = false\n",
        );

        assert!(scan_extensions_dir(temp.path()).expect("scan").is_empty());
    }

    /// 只 `read_dir` 一层会静默丢掉所有子目录，打出来的是残包。
    #[test]
    fn pack_extension_includes_nested_files() {
        let temp = tempfile::tempdir().expect("temp dir");
        let source = write_extension(
            temp.path(),
            "nested",
            "[extension]\nname = \"nested\"\n\n[runtime]\nside = \"server\"\n",
        );
        std::fs::create_dir_all(source.join("src/handlers")).expect("create nested dirs");
        std::fs::write(source.join("src/main.js"), b"export default {}").expect("write entry");
        std::fs::write(source.join("src/handlers/on_key.js"), b"// handler")
            .expect("write handler");

        let packed = pack_extension(&source).expect("pack");

        let decoder = flate2::read::GzDecoder::new(&packed[..]);
        let mut archive = tar::Archive::new(decoder);
        let mut packed_paths: Vec<String> = archive
            .entries()
            .expect("archive entries")
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| Some(entry.path().ok()?.to_string_lossy().into_owned()))
            .collect();
        packed_paths.sort();

        for expected in [
            EXTENSION_MANIFEST,
            "src/main.js",
            "src/handlers/on_key.js",
        ] {
            assert!(
                packed_paths.iter().any(|path| path == expected),
                "{expected} missing from archive: {packed_paths:?}"
            );
        }
    }
}
