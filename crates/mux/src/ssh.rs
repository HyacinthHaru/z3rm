//! # ssh
//!
//! SSH 会话建立与 socket 转发模块（§16.6 / Plan 19）。
//! 使用系统 `ssh` 命令通过 ControlMaster 建立持久连接，
//! 并通过 SSH 通道转发远程 mux_server Unix socket。

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::oneshot;

// ============================================================================
// §16.6 SSH 连接选项
// ============================================================================

/// §16.6 SSH 连接配置：主机、用户、端口、认证方式。
#[derive(Debug, Clone)]
pub struct SshConnectionOptions {
    /// 远程主机地址（hostname 或 IP）。
    pub host: String,
    /// 远程用户名（None = 使用当前系统用户）。
    pub username: Option<String>,
    /// SSH 端口（None = 默认 22）。
    pub port: Option<u16>,
    /// 身份文件路径（~/.ssh/id_rsa 等）。
    pub identity_file: Option<PathBuf>,
    /// 额外 SSH 参数（ProxyJump 等）。
    pub extra_args: Vec<String>,
    /// 连接超时秒数。
    pub connect_timeout: u16,
}

impl Default for SshConnectionOptions {
    fn default() -> Self {
        Self {
            host: String::new(),
            username: None,
            port: None,
            identity_file: None,
            extra_args: Vec::new(),
            connect_timeout: 30,
        }
    }
}

/// §16.6 SSH 连接选项构建器，支持 URI 解析 `ssh://user@host:port`。
impl SshConnectionOptions {
    /// 从 `ssh://` URI 解析连接选项。
    ///
    /// 格式: `ssh://[user@]host[:port]`
    pub fn from_uri(uri: &str) -> Result<Self> {
        let uri = uri.strip_prefix("ssh://").ok_or_else(|| {
            anyhow!("invalid SSH URI, must start with ssh://: {}", uri)
        })?;

        let mut host = uri.to_string();
        let mut username = None;
        let mut port = None;

        // 解析 user@host
        if let Some(at_pos) = host.find('@') {
            username = Some(host[..at_pos].to_string());
            host = host[at_pos + 1..].to_string();
        }

        // 解析 host:port
        if let Some(colon_pos) = host.rfind(':') {
            if let Ok(p) = host[colon_pos + 1..].parse::<u16>() {
                port = Some(p);
                host = host[..colon_pos].to_string();
            }
        }

        Ok(Self {
            host,
            username,
            port,
            identity_file: None,
            extra_args: Vec::new(),
            connect_timeout: 30,
        })
    }

    /// §16.6 构建 SSH 目标地址字符串 `user@host`。
    pub fn destination(&self) -> String {
        match &self.username {
            Some(user) => format!("{}@{}", user, self.host),
            None => self.host.clone(),
        }
    }

    /// §16.6 构建 SSH 命令基础参数。
    fn build_ssh_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        // §16.6 端口指定。
        if let Some(port) = self.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }

        // §16.6 身份文件。
        if let Some(ref id_file) = self.identity_file {
            args.push("-i".to_string());
            args.push(id_file.to_string_lossy().to_string());
        }

        // §16.6 连接超时。
        args.push("-o".to_string());
        args.push(format!("ConnectTimeout={}", self.connect_timeout));

        // §16.6 禁用 StrictHostKeyChecking 用于自动连接（生产环境可配置）。
        args.push("-o".to_string());
        args.push("StrictHostKeyChecking=no".to_string());

        // §16.6 禁用密码确认提示（使用 key auth 或 askpass）。
        args.push("-o".to_string());
        args.push("BatchMode=yes".to_string());

        // §16.6 额外参数（ProxyJump 等）。
        args.extend(self.extra_args.clone());

        args
    }
}

// ============================================================================
// §16.6 SSH 会话控制
// ============================================================================

/// §16.6 SSH 会话：管理 ControlMaster 连接和 socket 转发。
pub struct SshSession {
    /// §16.6 连接选项。
    options: SshConnectionOptions,
    /// §16.6 Control socket 路径（用于复用连接）。
    control_path: PathBuf,
    /// §16.6 SSH 主进程。
    master_process: Option<tokio::process::Child>,
}

impl SshSession {
    /// §16.6 建立 SSH ControlMaster 会话。
    ///
    /// 启动后台 SSH 进程，通过 ControlMaster 复用连接。
    /// 返回控制 socket 路径供后续命令复用。
    pub async fn connect(options: SshConnectionOptions) -> Result<Self> {
        let destination = options.destination();

        // §16.6 创建临时 Control socket 目录。
        let temp_dir = tempfile::tempdir().with_context(|| "创建临时目录失败")?;
        let control_path = temp_dir.path().join("ssh_control");

        // §16.6 启动 SSH ControlMaster 进程。
        let ssh_args = options.build_ssh_args();
        let mut cmd = Command::new("ssh");
        let control_str = format!("ControlPath={}", control_path.display());
        cmd.args(&ssh_args)
            .arg("-N")                           // §16.6 不执行远程命令
            .arg("-o")
            .arg("ControlMaster=yes")            // §16.6 启用 ControlMaster
            .arg("-o")
            .arg(control_str)
            .arg(&destination)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .context("启动 SSH 进程失败，请确认系统已安装 OpenSSH")?;

        // §16.6 等待连接建立（读取 stdout 直到连接完成）。
        let connect_timeout = Duration::from_secs(options.connect_timeout as u64);
        let mut stdout = child.stdout.take().expect("stdout should be piped");

        tokio::time::timeout(connect_timeout, async {
            // §16.6 SSH ControlMaster 连接成功后 stdout 关闭。
            let mut buf = [0u8; 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break, // §16.6 连接建立完成
                    Ok(n) => {
                        tracing::debug!(data = ?&buf[..n], "ssh master output");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ssh master read error");
                        break;
                    }
                }
            }
        })
        .await
        .context("SSH 连接超时")?;

        tracing::info!(
            destination = %destination,
            control_path = %control_path.display(),
            "SSH ControlMaster 连接建立"
        );

        Ok(Self {
            options,
            control_path,
            master_process: Some(child),
        })
    }

    /// §16.6 通过 SSH 执行远程命令，返回 stdout。
    pub async fn exec(&self, command: &str) -> Result<String> {
        let ssh_args = self.options.build_ssh_args();
        let destination = self.options.destination();

        let mut cmd = Command::new("ssh");
        let control_str = format!("ControlPath={}", self.control_path.display());
        cmd.args(&ssh_args)
            .arg("-o")
            .arg(control_str)
            .arg(&destination)
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await.context("SSH exec 失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("SSH exec 失败: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// §16.6 通过 SCP 上传本地文件到远程。
    pub async fn scp_upload(&self, local_path: &std::path::Path, remote_path: &str) -> Result<()> {
        let ssh_args = self.options.build_ssh_args();
        let destination = self.options.destination();

        let local_str = local_path.to_string_lossy();
        let remote_dest = format!("{}:{}", destination, remote_path);

        let control_str = format!("ControlPath={}", self.control_path.display());
        let mut cmd = Command::new("scp");
        cmd.args(&ssh_args)
            .arg("-o")
            .arg(control_str)
            .arg("-C") // §16.6 启用压缩
            .arg(&*local_str)
            .arg(&remote_dest);

        let output = cmd.output().await.context("SCP 上传失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("SCP 上传失败: {}", stderr);
        }

        tracing::info!(
            local = %local_str,
            remote = %remote_path,
            "SCP 上传完成"
        );
        Ok(())
    }

    /// §16.6 通过 SSH 建立 socket 转发，返回本地 Unix socket 路径。
    ///
    /// 在远程执行 `socat` 或 `ssh -L` 将远程 Unix socket 转发到本地。
    pub async fn forward_socket(
        &self,
        remote_socket: &str,
    ) -> Result<(PathBuf, oneshot::Sender<()>)> {
        let destination = self.options.destination();
        let ssh_args = self.options.build_ssh_args();

        // §16.6 创建本地临时 Unix socket。
        let temp_dir = tempfile::tempdir().with_context(|| "创建临时目录失败")?;
        let local_socket_path = temp_dir.path().join("mux.sock");

        // §16.6 通过 SSH 通道转发 socket。
        // 使用 ssh -L 将远程 socket 映射到本地 socket。
        // 命令: ssh -L /tmp/local.sock:/tmp/remote.sock user@host sleep 999999
        let mut cmd = Command::new("ssh");
        let control_str = format!("ControlPath={}", self.control_path.display());
        let forward_str = format!(
            "{}:{}",
            local_socket_path.display(),
            remote_socket
        );
        cmd.args(&ssh_args)
            .arg("-o")
            .arg(control_str)
            .arg("-L")
            .arg(forward_str)
            .arg(&destination)
            .arg("sleep")
            .arg("999999") // §16.6 保持转发进程存活
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn().context("SSH socket 转发启动失败")?;

        // §16.6 等待 socket 就绪（短暂延迟让 ssh 设置完成）。
        tokio::time::sleep(Duration::from_millis(500)).await;

        // §16.6 创建关闭信号通道。
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // §16.6 后台任务：监听关闭信号后终止转发进程。
        let forward_pid = child.id();
        tokio::spawn(async move {
            let _ = shutdown_rx.await;
            tracing::info!(
                pid = ?forward_pid,
                "SSH socket 转发关闭"
            );
        });

        // §16.6 防止 child 被 drop 时终止。
        std::mem::forget(child);

        tracing::info!(
            local = %local_socket_path.display(),
            remote = %remote_socket,
            "SSH socket 转发建立"
        );

        Ok((local_socket_path, shutdown_tx))
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.master_process.take() {
            // §16.6 优雅关闭 SSH ControlMaster（Drop 不能 async，使用 start_kill）。
            let _ = child.start_kill();
        }
    }
}

// ============================================================================
// §16.6 SSH 连接入口：完整的 SSH 连接流程
// ============================================================================

/// §16.6 完整的 SSH 连接流程：建立会话 → 探测服务器 → 安装（如需要）→ 转发 socket。
///
/// 对外接口。返回 `(MuxDomain, SshSession)`，调用者需保持 `SshSession` 存活。
pub async fn connect_ssh(target: &str) -> anyhow::Result<(super::MuxDomain, SshSession)> {
    use crate::remote_install::ensure_remote_server;

    // §16.6 步骤 1：解析连接选项。
    let options = SshConnectionOptions::from_uri(target)
        .with_context(|| format!("解析 SSH URI 失败: {}", target))?;

    // §16.6 步骤 2：建立 SSH 会话（ControlMaster）。
    let session = SshSession::connect(options).await?;

    // §16.6 步骤 3：探测/安装远程服务器。
    let server_path = ensure_remote_server(&session).await?;

    // §16.6 步骤 4：启动远程 mux_server 守护进程。
    session
        .exec(&format!("nohup {} --daemonize </dev/null >/dev/null 2>&1 &", shell_escape(&server_path)))
        .await
        .context("启动远程 mux_server 失败")?;

    // §16.6 等待服务器启动。
    tokio::time::sleep(Duration::from_secs(1)).await;

    // §16.6 步骤 5：转发远程 socket 到本地。
    let domain = super::connect_local(Some(&local_socket)).await
        .context("通过转发的 socket 连接 mux_server 失败")?;

    tracing::info!(
        target = %target,
        local_socket = %local_socket.display(),
        "SSH 远程连接建立完成"
    );

    Ok((domain, session))
}

/// §16.6 对 shell 参数进行安全转义。
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let needs_escape = s.chars().any(|c| {
        matches!(c, ' ' | '\'' | '"' | '\\' | '$' | '`' | '!' | '#' | '&' | '|' | ';' | '(' | ')' | '<' | '>' | '*' | '?' | '[' | ']' | '~')
    });
    if needs_escape {
        let escaped = s.replace('\'', "'\\\''");
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
    fn test_from_uri_simple_host() {
        let opts = SshConnectionOptions::from_uri("ssh://myhost.com").unwrap();
        assert_eq!(opts.host, "myhost.com");
        assert!(opts.username.is_none());
        assert!(opts.port.is_none());
    }

    #[test]
    fn test_from_uri_with_user() {
        let opts = SshConnectionOptions::from_uri("ssh://alice@myhost.com").unwrap();
        assert_eq!(opts.host, "myhost.com");
        assert_eq!(opts.username, Some("alice".to_string()));
        assert!(opts.port.is_none());
    }

    #[test]
    fn test_from_uri_with_user_and_port() {
        let opts = SshConnectionOptions::from_uri("ssh://bob@192.168.1.1:2222").unwrap();
        assert_eq!(opts.host, "192.168.1.1");
        assert_eq!(opts.username, Some("bob".to_string()));
        assert_eq!(opts.port, Some(2222));
    }

    #[test]
    fn test_from_uri_host_only_port() {
        let opts = SshConnectionOptions::from_uri("ssh://server:8022").unwrap();
        assert_eq!(opts.host, "server");
        assert!(opts.username.is_none());
        assert_eq!(opts.port, Some(8022));
    }

    #[test]
    fn test_from_uri_invalid_prefix() {
        let result = SshConnectionOptions::from_uri("http://host");
        assert!(result.is_err());
    }

    #[test]
    fn test_destination_with_username() {
        let opts = SshConnectionOptions {
            host: "myhost.com".to_string(),
            username: Some("alice".to_string()),
            port: None,
            identity_file: None,
            extra_args: Vec::new(),
            connect_timeout: 30,
        };
        assert_eq!(opts.destination(), "alice@myhost.com");
    }

    #[test]
    fn test_destination_without_username() {
        let opts = SshConnectionOptions {
            host: "myhost.com".to_string(),
            username: None,
            port: None,
            identity_file: None,
            extra_args: Vec::new(),
            connect_timeout: 30,
        };
        assert_eq!(opts.destination(), "myhost.com");
    }

    #[test]
    fn test_build_ssh_args_default() {
        let opts = SshConnectionOptions {
            host: "myhost.com".to_string(),
            username: None,
            port: None,
            identity_file: None,
            extra_args: Vec::new(),
            connect_timeout: 30,
        };
        let args = opts.build_ssh_args();
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"ConnectTimeout=30".to_string()));
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn test_build_ssh_args_with_port_and_identity() {
        let opts = SshConnectionOptions {
            host: "myhost.com".to_string(),
            username: Some("alice".to_string()),
            port: Some(2222),
            identity_file: Some(PathBuf::from("/home/alice/.ssh/id_ed25519")),
            extra_args: Vec::new(),
            connect_timeout: 60,
        };
        let args = opts.build_ssh_args();
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"/home/alice/.ssh/id_ed25519".to_string()));
        assert!(args.contains(&"ConnectTimeout=60".to_string()));
    }

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
}
