//! # ssh
//!
//! SSH 会话建立与 socket 转发模块（§16.6 / Plan 19）。
//! 使用系统 `ssh` 命令通过 ControlMaster 建立持久连接，
//! 并通过 SSH 通道转发远程 mux_server Unix socket。

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const REMOTE_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn parse_remote_daemon_probe(output: &str) -> Result<bool> {
    match output.trim() {
        "available" => Ok(true),
        "unavailable" => Ok(false),
        other => Err(anyhow!(
            "远程 mux_server 可用性探测返回未知结果: {other:?}"
        )),
    }
}

fn remote_daemon_probe_command(remote_socket: &str) -> String {
    let pid_file = format!("{remote_socket}.pid");
    format!(
        "socket={}; pid_file={}; \
         if [ -S \"$socket\" ] && [ -r \"$pid_file\" ]; then \
           IFS= read -r pid < \"$pid_file\"; \
           case \"$pid\" in \
             ''|*[!0-9]*) printf unavailable ;; \
             *) if kill -0 \"$pid\" 2>/dev/null; then printf available; \
                else printf unavailable; fi ;; \
           esac; \
         else printf unavailable; fi",
        shell_escape(remote_socket),
        shell_escape(&pid_file),
    )
}

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
        let uri = uri
            .strip_prefix("ssh://")
            .ok_or_else(|| anyhow!("invalid SSH URI, must start with ssh://: {}", uri))?;

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
///
/// 拥有 ControlMaster 临时目录与 forward 转发子进程及其本地 socket 目录，
/// Drop 时一并清理，避免进程与 socket 泄漏。
pub struct SshSession {
    /// §16.6 连接选项。
    options: SshConnectionOptions,
    /// §16.6 Control socket 目录，须随会话存活以保留 control socket 文件。
    /// 该字段从不被读取，仅靠其 Drop 删除目录，故标注 dead_code。
    #[allow(dead_code)]
    control_dir: Option<tempfile::TempDir>,
    /// §16.6 Control socket 路径（用于复用连接）。
    control_path: PathBuf,
    /// §16.6 SSH 主进程。
    master_process: Option<tokio::process::Child>,
    /// §16.6 forward 转发本地 socket 目录，须随会话存活以保留 socket 文件。
    /// 该字段从不被读取，仅靠其 Drop 删除目录，故标注 dead_code。
    #[allow(dead_code)]
    forward_dir: Option<tempfile::TempDir>,
    /// §16.6 forward 转发 ssh 子进程，Drop 时一并终止。
    forward_process: Option<tokio::process::Child>,
}

impl SshSession {
    /// §16.6 建立 SSH ControlMaster 会话。
    ///
    /// 启动后台 SSH 进程，通过 ControlMaster 复用连接。
    /// 返回控制 socket 路径供后续命令复用。
    pub async fn connect(options: SshConnectionOptions) -> Result<Self> {
        let destination = options.destination();

        // §16.6 创建临时 Control socket 目录，由 SshSession 持有以保留 socket 文件。
        let control_dir = tempfile::tempdir().with_context(|| "创建临时目录失败")?;
        let control_path = control_dir.path().join("ssh_control");

        // §16.6 启动 SSH ControlMaster 进程。
        let ssh_args = options.build_ssh_args();
        let mut cmd = Command::new("ssh");
        let control_str = format!("ControlPath={}", control_path.display());
        cmd.args(&ssh_args)
            .arg("-N") // §16.6 不执行远程命令
            .arg("-o")
            .arg("ControlMaster=yes") // §16.6 启用 ControlMaster
            .arg("-o")
            .arg(control_str)
            .arg(&destination)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .context("启动 SSH 进程失败，请确认系统已安装 OpenSSH")?;

        // §16.6 ControlMaster 就绪判断：不依赖 stdout EOF（ssh -N 下 stdout 不会关闭），
        // 改为轮询 control socket 是否出现。同时检查 ssh 是否已提前退出（连接失败）。
        // stderr 留在 child 中以便失败时读取，并向调用者报告可诊断错误。
        let connect_timeout = Duration::from_secs(options.connect_timeout as u64);
        let ready = tokio::time::timeout(connect_timeout, async {
            loop {
                // §16.6 ssh 提前退出说明连接失败：读取 stderr 后返回明确错误。
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let stderr_msg = match child.stderr.as_mut() {
                            Some(s) => {
                                use tokio::io::AsyncReadExt;
                                let mut buf = vec![0u8; 4096];
                                match s.read(&mut buf).await {
                                    Ok(n) => String::from_utf8_lossy(&buf[..n]).to_string(),
                                    Err(_) => String::new(),
                                }
                            }
                            None => String::new(),
                        };
                        return Err(anyhow!(
                            "SSH ControlMaster 退出，连接失败: status={} stderr={}",
                            status,
                            stderr_msg.trim()
                        ));
                    }
                    Ok(None) => {} // §16.6 进程仍在运行，继续等待 socket 出现。
                    Err(e) => return Err(anyhow!("检查 SSH 进程状态失败: {e}")),
                }
                if control_path.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
        .await;
        match ready {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // §16.6 失败路径：尽力终止已 spawn 的 ControlMaster，不静默吞错。
                if let Err(kill_err) = child.start_kill() {
                    tracing::warn!(error = %kill_err, "失败清理时终止 SSH ControlMaster 失败");
                }
                return Err(e);
            }
            Err(_) => {
                if let Err(kill_err) = child.start_kill() {
                    tracing::warn!(error = %kill_err, "超时清理时终止 SSH ControlMaster 失败");
                }
                return Err(anyhow!("SSH ControlMaster 连接超时: {}", destination));
            }
        }

        tracing::info!(
            destination = %destination,
            control_path = %control_path.display(),
            "SSH ControlMaster 连接建立"
        );

        Ok(Self {
            options,
            control_dir: Some(control_dir),
            control_path,
            master_process: Some(child),
            forward_dir: None,
            forward_process: None,
        })
    }

    fn control_master_is_live(&mut self) -> Result<bool> {
        let Some(master) = self.master_process.as_mut() else {
            return Ok(false);
        };
        if master
            .try_wait()
            .context("检查 SSH ControlMaster 进程状态失败")?
            .is_some()
        {
            return Ok(false);
        }
        self.control_path
            .try_exists()
            .with_context(|| format!("检查 SSH control socket {} 失败", self.control_path.display()))
    }

    fn take_master(&mut self) -> Result<()> {
        if let Some(mut child) = self.master_process.take() {
            match child
                .try_wait()
                .context("检查待清理 SSH ControlMaster 状态失败")?
            {
                Some(_) => {}
                None => {
                    if let Err(error) = child.start_kill() {
                        self.master_process = Some(child);
                        return Err(error).context("终止 SSH ControlMaster 子进程失败");
                    }
                }
            }
        }
        self.control_dir = None;
        self.control_path.clear();
        Ok(())
    }

    async fn recreate_control_master(&mut self) -> Result<()> {
        let mut replacement = Self::connect(self.options.clone())
            .await
            .context("重新建立 SSH ControlMaster 失败")?;

        self.take_master()
            .context("替换失效 SSH ControlMaster 时清理旧进程失败")?;
        self.take_forward();
        self.control_dir = replacement.control_dir.take();
        self.control_path = std::mem::take(&mut replacement.control_path);
        self.master_process = replacement.master_process.take();
        Ok(())
    }

    async fn ensure_control_master(&mut self) -> Result<()> {
        if self.control_master_is_live()? {
            return Ok(());
        }

        tracing::warn!(
            destination = %self.options.destination(),
            "SSH ControlMaster 不可用，正在重新建立"
        );
        self.recreate_control_master().await
    }

    async fn resolve_remote_socket(&self) -> Result<String> {
        let remote_socket = self
            .exec("printf '%s' \"${XDG_RUNTIME_DIR:-/tmp}/z3rm/mux.sock\"")
            .await
            .context("解析远程 mux socket 路径失败")?;
        let remote_socket = remote_socket.trim();
        anyhow::ensure!(!remote_socket.is_empty(), "远程 mux socket 路径为空");
        Ok(remote_socket.to_string())
    }

    async fn remote_daemon_available(&self, remote_socket: &str) -> Result<bool> {
        let output = self
            .exec(&remote_daemon_probe_command(remote_socket))
            .await
            .context("探测远程 mux_server 可用性失败")?;
        parse_remote_daemon_probe(&output)
    }

    async fn start_remote_daemon_if_unavailable(&self, remote_socket: &str) -> Result<()> {
        if self.remote_daemon_available(remote_socket).await? {
            return Ok(());
        }

        let server_path = crate::remote_install::ensure_remote_server(self)
            .await
            .context("确保远程 z3rm-server 可用失败")?;

        if self.remote_daemon_available(remote_socket).await? {
            return Ok(());
        }

        self.exec(&format!(
            "nohup {} --daemonize </dev/null >/dev/null 2>&1 &",
            shell_escape(&server_path)
        ))
        .await
        .context("启动远程 mux_server 失败")?;

        match tokio::time::timeout(REMOTE_DAEMON_START_TIMEOUT, async {
            loop {
                if self.remote_daemon_available(remote_socket).await? {
                    return Ok(());
                }
                tokio::time::sleep(REMOTE_DAEMON_POLL_INTERVAL).await;
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "远程 mux_server 启动后未在 {:?} 内可用: {}",
                REMOTE_DAEMON_START_TIMEOUT,
                remote_socket
            )),
        }
    }

    /// Re-establish any dead SSH control connection, preserve a live remote
    /// daemon, and replace the local Unix-socket forwarding endpoint.
    pub async fn reconnect(&mut self) -> Result<PathBuf> {
        self.ensure_control_master().await?;
        let remote_socket = self.resolve_remote_socket().await?;
        self.start_remote_daemon_if_unavailable(&remote_socket)
            .await?;
        self.forward_socket(&remote_socket)
            .await
            .context("建立远程 mux socket 转发失败")
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

    /// §16.6 通过 SSH 建立 socket 转发，将本地 Unix socket 转发到远程 socket。
    ///
    /// 转发子进程及其本地 socket 临时目录由本会话拥有，随 `SshSession` Drop 一并清理。
    /// 返回本地 socket 路径供后续连接。多次调用会替换上一次的转发资源。
    pub async fn forward_socket(&mut self, remote_socket: &str) -> Result<PathBuf> {
        // §16.6 先清理上一次的转发资源（若有），避免遗留进程与 socket。
        self.take_forward();

        let destination = self.options.destination();
        let ssh_args = self.options.build_ssh_args();

        // §16.6 创建本地临时 Unix socket 目录，由 SshSession 持有以保留 socket 文件。
        let forward_dir = tempfile::tempdir().with_context(|| "创建临时目录失败")?;
        let local_socket_path = forward_dir.path().join("mux.sock");

        // §16.6 通过 SSH 控制通道转发 socket：ssh -L local:remote 复用 ControlMaster。
        let mut cmd = Command::new("ssh");
        let control_str = format!("ControlPath={}", self.control_path.display());
        let forward_str = format!("{}:{}", local_socket_path.display(), remote_socket);
        cmd.args(&ssh_args)
            .arg("-o")
            .arg(control_str)
            .arg("-L")
            .arg(forward_str)
            .arg(&destination)
            .arg("sleep")
            .arg("999999") // §16.6 保持转发进程存活，由 Drop 终止。
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("SSH socket 转发启动失败")?;

        // §16.6 等待本地 socket 就绪：轮询文件出现，同时报告转发进程提前退出。
        let forward_timeout = Duration::from_secs(5);
        let ready = tokio::time::timeout(forward_timeout, async {
            loop {
                if local_socket_path.exists() {
                    return Ok(());
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let stderr_msg = match child.stderr.as_mut() {
                            Some(stderr) => {
                                use tokio::io::AsyncReadExt;
                                let mut buffer = vec![0u8; 4096];
                                match stderr.read(&mut buffer).await {
                                    Ok(length) => {
                                        String::from_utf8_lossy(&buffer[..length]).to_string()
                                    }
                                    Err(error) => format!("<读取 stderr 失败: {error}>"),
                                }
                            }
                            None => String::new(),
                        };
                        return Err(anyhow!(
                            "SSH socket 转发进程提前退出: status={} stderr={}",
                            status,
                            stderr_msg.trim()
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => return Err(anyhow!("检查 SSH forward 进程状态失败: {error}")),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match ready {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if let Err(kill_error) = child.start_kill() {
                    tracing::warn!(error = %kill_error, "失败清理时终止 SSH forward 子进程失败");
                }
                return Err(error);
            }
            Err(_) => {
                if let Err(kill_error) = child.start_kill() {
                    tracing::warn!(error = %kill_error, "超时清理时终止 SSH forward 子进程失败");
                }
                return Err(anyhow!("SSH socket 转发等待本地 socket 超时"));
            }
        }

        // §16.6 资源所有权移交会话。
        self.forward_dir = Some(forward_dir);
        self.forward_process = Some(child);

        tracing::info!(
            local = %local_socket_path.display(),
            remote = %remote_socket,
            "SSH socket 转发建立"
        );

        Ok(local_socket_path)
    }

    /// §16.6 取出并清理 forward 转发资源（终止子进程、丢弃临时目录）。
    ///
    /// 供 Drop 与 `forward_socket` 重置时复用。不静默吞错：kill 失败记录 tracing 警告。
    fn take_forward(&mut self) {
        if let Some(mut child) = self.forward_process.take() {
            // §16.6 Drop 不能 async，使用 start_kill 发送 SIGTERM。
            if let Err(e) = child.start_kill() {
                tracing::warn!(error = %e, "终止 SSH forward 子进程失败");
            }
        }
        // §16.6 丢弃 forward_dir 会删除本地 socket 目录。
        self.forward_dir = None;
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        // §16.6 先终止 forward 转发子进程与本地 socket 目录。
        self.take_forward();
        // §16.6 再终止 SSH ControlMaster 主进程。
        if let Err(error) = self.take_master() {
            tracing::warn!(error = %error, "清理 SSH ControlMaster 失败");
        }
        // §16.6 control_dir 在结构体析构时一并删除其临时目录。
    }
}

// ============================================================================
// §16.6 SSH 连接入口：完整的 SSH 连接流程
// ============================================================================

/// §16.6 完整的 SSH 连接流程：建立会话 → 探测服务器 → 安装（如需要）→ 转发 socket。
///
/// 对外接口。返回 `(MuxDomain, SshSession)`，调用者需保持 `SshSession` 存活。
pub async fn connect_ssh(target: &str) -> anyhow::Result<(super::MuxDomain, SshSession)> {
    let options = SshConnectionOptions::from_uri(target)
        .with_context(|| format!("解析 SSH URI 失败: {}", target))?;
    let mut session = SshSession::connect(options).await?;
    let local_socket = session
        .reconnect()
        .await
        .context("准备远程 mux_server 连接失败")?;
    let domain = super::connect_local(Some(&local_socket))
        .await
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
                | '~'
        )
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

    #[test]
    fn remote_daemon_probe_requires_an_explicit_state() {
        assert!(parse_remote_daemon_probe("available\n").unwrap());
        assert!(!parse_remote_daemon_probe("unavailable").unwrap());
        assert!(parse_remote_daemon_probe("unexpected output").is_err());
    }

    // §16.6 这些测试验证 SshSession 对 control_dir / forward_dir / forward 进程的
    // 所有权与 Drop 清理行为，不依赖真实 ssh 二进制（CI 沙箱中常不可用）。

    /// 构造一个持有指定临时目录与子进程的 SshSession，便于测试 Drop 行为。
    fn fake_session(
        control_dir: Option<tempfile::TempDir>,
        master: Option<tokio::process::Child>,
        forward_dir: Option<tempfile::TempDir>,
        forward: Option<tokio::process::Child>,
    ) -> SshSession {
        let options = SshConnectionOptions {
            host: "test".to_string(),
            username: None,
            port: None,
            identity_file: None,
            extra_args: Vec::new(),
            connect_timeout: 30,
        };
        let control_path = control_dir
            .as_ref()
            .map(|d| d.path().join("ssh_control"))
            .unwrap_or_default();
        SshSession {
            options,
            control_dir,
            control_path,
            master_process: master,
            forward_dir,
            forward_process: forward,
        }
    }

    /// 启动一个长寿命子进程并返回 (child, pid)，用于验证 Drop 是否终止它。
    fn long_sleep_child() -> (tokio::process::Child, u32) {
        let child = tokio::process::Command::new("sleep")
            .arg("9999")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("pid");
        (child, pid)
    }

    /// 进程是否仍存活：`kill -0 pid` 成功表示存在。
    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn live_control_master_is_reused() {
        let control_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(control_dir.path().join("ssh_control"), b"")
            .expect("create fake control socket");
        let (master, _pid) = long_sleep_child();
        let mut session = fake_session(Some(control_dir), Some(master), None, None);

        assert!(session.control_master_is_live().unwrap());
    }

    #[tokio::test]
    async fn exited_control_master_requires_recreation() {
        let control_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(control_dir.path().join("ssh_control"), b"")
            .expect("create fake control socket");
        let mut master = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn short-lived child");
        master.wait().await.expect("wait for short-lived child");
        let mut session = fake_session(Some(control_dir), Some(master), None, None);

        assert!(!session.control_master_is_live().unwrap());
    }

    #[tokio::test]
    async fn test_control_dir_lives_with_session() {
        let control_dir = tempfile::tempdir().expect("tempdir");
        let dir_path = control_dir.path().to_path_buf();
        let session = fake_session(Some(control_dir), None, None, None);
        assert!(dir_path.exists(), "control 目录在 session 存活时应存在");
        drop(session);
        assert!(!dir_path.exists(), "control 目录在 session drop 后应被删除");
    }

    #[tokio::test]
    async fn test_forward_dir_lives_with_session() {
        let forward_dir = tempfile::tempdir().expect("tempdir");
        let dir_path = forward_dir.path().to_path_buf();
        let session = fake_session(None, None, Some(forward_dir), None);
        assert!(dir_path.exists(), "forward 目录在 session 存活时应存在");
        drop(session);
        assert!(!dir_path.exists(), "forward 目录在 session drop 后应被删除");
    }

    #[tokio::test]
    async fn test_take_forward_clears_fields() {
        let forward_dir = tempfile::tempdir().expect("tempdir");
        let dir_path = forward_dir.path().to_path_buf();
        let (forward_child, _pid) = long_sleep_child();
        let mut session = fake_session(None, None, Some(forward_dir), Some(forward_child));
        session.take_forward();
        assert!(
            session.forward_process.is_none(),
            "take_forward 后 forward_process 应为 None"
        );
        assert!(
            session.forward_dir.is_none(),
            "take_forward 后 forward_dir 应为 None"
        );
        assert!(
            !dir_path.exists(),
            "take_forward 后本地 socket 目录应被删除"
        );
    }

    /// Drop 必须终止 forward 子进程，而非 mem::forget。
    #[tokio::test]
    async fn test_drop_kills_forward_process() {
        let (forward_child, pid) = long_sleep_child();
        assert!(process_alive(pid), "forward 子进程刚启动时应存活");
        let session = fake_session(None, None, None, Some(forward_child));
        drop(session);
        // §16.6 start_kill 异步投递信号，轮询确认终止。
        for _ in 0..100 {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("forward 子进程在 session drop 后未被终止");
    }

    /// Drop 必须终止 master 子进程。
    #[tokio::test]
    async fn test_drop_kills_master_process() {
        let (master_child, pid) = long_sleep_child();
        assert!(process_alive(pid), "master 子进程刚启动时应存活");
        let session = fake_session(None, Some(master_child), None, None);
        drop(session);
        for _ in 0..100 {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("master 子进程在 session drop 后未被终止");
    }

    /// 重复 take_forward 不应 panic 且不应泄漏。
    #[tokio::test]
    async fn test_take_forward_idempotent() {
        let mut session = fake_session(None, None, None, None);
        session.take_forward();
        session.take_forward();
        assert!(session.forward_process.is_none());
        assert!(session.forward_dir.is_none());
    }
}
