//! # remote_install
//!
//! 远程服务器自动探测与安装模块（§16.6 / Plan 19）。
//! 支持同架构 scp 上传和跨架构下载安装。

use crate::ssh::SshSession;
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

// ============================================================================
// §16.6 远程服务器探测
// ============================================================================

/// §16.6 探测远程主机上是否存在 z3rm-server。
///
/// 检查 PATH 中的 `z3rm-server` 和 `~/.z3rm-server/z3rm-server`。
/// 返回服务器路径（如存在）。
pub async fn probe_remote_server(session: &SshSession) -> Result<Option<String>> {
    // §16.6 检查 PATH 中的 z3rm-server。
    let output = session
        .exec("command -v z3rm-server 2>/dev/null || echo ''")
        .await?;
    let path = output.trim().to_string();

    if !path.is_empty() {
        tracing::info!(path = %path, "远程 z3rm-server 已存在于 PATH");
        return Ok(Some(path));
    }

    // §16.6 检查 ~/.z3rm-server/z3rm-server。
    let output = session
        .exec("ls ~/.z3rm-server/z3rm-server 2>/dev/null || echo ''")
        .await?;
    let path = output.trim().to_string();

    if !path.is_empty() {
        tracing::info!(path = %path, "远程 z3rm-server 存在于 ~/.z3rm-server/");
        return Ok(Some(path));
    }

    tracing::info!("远程主机未发现 z3rm-server");
    Ok(None)
}

// ============================================================================
// §16.6 远程架构检测
// ============================================================================

/// §16.6 检测远程主机架构（`uname -m`）。
pub async fn detect_remote_arch(session: &SshSession) -> Result<String> {
    let output = session.exec("uname -m").await?;
    Ok(output.trim().to_string())
}

/// §16.6 获取本地架构。
fn detect_local_arch() -> Result<String> {
    Ok(std::env::consts::ARCH.to_string())
}

/// §16.6 比较本地与远程架构是否相同。
pub async fn is_same_arch(session: &SshSession) -> Result<bool> {
    let local = detect_local_arch()?;
    let remote = detect_remote_arch(session).await?;
    Ok(local == remote)
}

// ============================================================================
// §16.6 远程版本检测
// ============================================================================

/// §16.6 获取远程 z3rm-server 版本号。
pub async fn get_remote_version(session: &SshSession, server_path: &str) -> Result<Option<String>> {
    let output = session
        .exec(&format!(
            "{} --version 2>/dev/null || echo ''",
            shell_escape(server_path)
        ))
        .await?;
    let version = output.trim();
    if version.is_empty() {
        Ok(None)
    } else {
        Ok(Some(version.to_string()))
    }
}

/// §16.6 获取本地 z3rm-server 版本号。
pub fn get_local_version() -> Option<&'static str> {
    option_env!("ZERM_VERSION")
}

// ============================================================================
// §16.6 同架构安装（SCP 上传）
// ============================================================================

/// §16.6 同架构下通过 SCP 上传本地二进制到远程。
pub async fn install_same_arch(session: &SshSession, local_binary: &Path) -> Result<String> {
    let remote_dir = "~/.z3rm-server";

    // §16.6 创建远程目录。
    session
        .exec(&format!("mkdir -p {remote_dir}"))
        .await
        .context("创建远程目录失败")?;

    // §16.6 SCP 上传本地二进制。
    session
        .scp_upload(local_binary, &format!("{remote_dir}/z3rm-server"))
        .await
        .context("SCP 上传失败")?;

    // §16.6 设置可执行权限。
    session
        .exec(&format!("chmod +x {remote_dir}/z3rm-server"))
        .await
        .context("设置可执行权限失败")?;

    let remote_path = format!("{remote_dir}/z3rm-server");
    tracing::info!(path = %remote_path, "同架构安装完成 (SCP)");
    Ok(remote_path)
}

/// §16.6 查找本地 z3rm-server 二进制路径。
pub fn find_local_server_binary() -> Result<PathBuf> {
    // §16.6 尝试从当前可执行文件推导。
    if let Ok(exe) = std::env::current_exe() {
        let parent = exe.parent().unwrap_or(&exe);
        let candidate = parent.join("z3rm-server");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // §16.6 尝试常见安装路径。
    let candidates = ["/usr/local/bin/z3rm-server", "/usr/bin/z3rm-server"];
    for candidate in &candidates {
        let path = Path::new(candidate).to_path_buf();
        if path.exists() {
            return Ok(path);
        }
    }

    // §16.6 检查 PATH 中的 z3rm-server。
    if let Some(server) = which::which("z3rm-server").ok() {
        if server.exists() {
            return Ok(server);
        }
    }

    Err(anyhow!("未找到本地 z3rm-server 二进制"))
}

// ============================================================================
// §16.6 跨架构安装（远程下载）
// ============================================================================

/// §16.6 跨架构下通过远程下载安装。
pub async fn install_cross_arch(session: &SshSession, target_arch: &str) -> Result<String> {
    let remote_dir = "~/.z3rm-server";
    let target_os = detect_remote_os(session).await?;

    // §16.6 构建下载 URL。
    let url = format!(
        "https://github.com/z3rm/zerm/releases/latest/download/z3rm-server-{target_arch}-{target_os}"
    );

    // §16.6 在远程执行下载命令。
    let cmd = format!(
        "mkdir -p {remote_dir} && \
         curl -sfL {url} -o {remote_dir}/z3rm-server && \
         chmod +x {remote_dir}/z3rm-server"
    );

    session.exec(&cmd).await.context("远程下载安装失败")?;

    tracing::info!(
        arch = %target_arch,
        os = %target_os,
        url = %url,
        "跨架构安装完成 (远程下载)"
    );
    Ok(format!("{remote_dir}/z3rm-server"))
}

/// §16.6 检测远程操作系统。
async fn detect_remote_os(session: &SshSession) -> Result<String> {
    let output = session.exec("uname -s").await?;
    let os = output.trim().to_lowercase();
    match os.as_str() {
        "linux" => Ok("linux".to_string()),
        "darwin" => Ok("macos".to_string()),
        _ => Ok(os),
    }
}

// ============================================================================
// §16.6 自动安装入口
// ============================================================================

/// §16.6 完整的自动安装流程：探测 → 判断架构 → SCP 或下载。
///
/// 返回安装的远程服务器路径。
pub async fn auto_install_server(session: &SshSession) -> Result<String> {
    // §16.6 检查架构是否匹配。
    let same_arch = is_same_arch(session).await?;

    if same_arch {
        // §16.6 同架构：SCP 上传。
        let local_binary = find_local_server_binary()
            .with_context(|| "需要本地 z3rm-server 二进制以进行同架构安装")?;
        install_same_arch(session, local_binary.as_ref()).await
    } else {
        // §16.6 跨架构：远程下载。
        let remote_arch = detect_remote_arch(session).await?;
        install_cross_arch(session, &remote_arch).await
    }
}

/// §16.6 完整流程：探测 → 版本检查 → 安装（需要时）。
///
/// 返回远程服务器路径。
pub async fn ensure_remote_server(session: &SshSession) -> Result<String> {
    // §16.6 步骤1：探测现有服务器。
    if let Some(path) = probe_remote_server(session).await? {
        // §16.6 检查版本是否匹配。
        if let Some(remote_ver) = get_remote_version(session, &path).await? {
            if let Some(local_ver) = get_local_version() {
                if remote_ver != local_ver {
                    tracing::warn!(
                        local = %local_ver,
                        remote = %remote_ver,
                        "版本不匹配，重新安装"
                    );
                    return auto_install_server(session).await;
                }
            }
            tracing::info!(path = %path, version = %remote_ver, "远程服务器版本匹配");
            return Ok(path);
        }
        // §16.6 无法获取远程版本，假设匹配。
        tracing::info!(path = %path, "无法获取远程版本，使用现有服务器");
        return Ok(path);
    }

    // §16.6 步骤2：未找到服务器，自动安装。
    tracing::info!("远程服务器未找到，开始自动安装");
    auto_install_server(session).await
}

// ============================================================================
// §16.6 Shell 转义工具函数
// ============================================================================

/// §16.6 对 shell 参数进行安全转义。
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // §16.6 检查是否需要转义。
    let needs_escape = s.chars().any(|c| {
        matches!(
            c,
            ' ' | '\''
                | '"'
                | '\\'
                | '$'
                | '`'
                | '!'
                | '#'
                | '&'
                | '|'
                | ';'
                | '('
                | ')'
                | '<'
                | '>'
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '~'
        )
    });

    if needs_escape {
        // §16.6 使用单引号包裹，内部单引号转义为 '\''。
        let escaped = s.replace('\'', "'\\''");
        format!("'{escaped}'")
    } else {
        s.to_string()
    }
}

// ============================================================================
// §16.6 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello".to_string());
    }

    #[test]
    fn test_shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'".to_string());
    }

    #[test]
    fn test_shell_escape_with_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'".to_string());
    }

    #[test]
    fn test_shell_escape_empty() {
        assert_eq!(shell_escape(""), "''".to_string());
    }

    #[test]
    fn test_shell_escape_special_chars() {
        assert_eq!(shell_escape("hello $world"), "'hello $world'".to_string());
    }
}
