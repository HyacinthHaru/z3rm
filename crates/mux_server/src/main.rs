// §3.1 z3rm-server daemon 入口点。
// 绑定本地 socket，接受连接，服务 mux protocol RPC。
//
// The daemon binds a Unix socket and supervises PTY child processes, neither of
// which exists in a browser. The wasm build carries the library only; the
// server there is driven by a pump from JS (see `mux_server::rt`).
#![cfg_attr(target_family = "wasm", allow(unused))]

#[cfg(target_family = "wasm")]
fn main() {}

#[cfg(not(target_family = "wasm"))]
mod daemon {

    use anyhow::Result;
    use mux_server::run;
    use std::path::PathBuf;

    pub fn main() -> Result<()> {
        let args: Vec<String> = std::env::args().collect();

        // The SSH installer probes this before deciding whether an existing
        // daemon binary can be reused.
        match args.get(1).map(String::as_str) {
            Some("--version") | Some("-V") => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("status") => return cmd_status(),
            Some("kill") => return cmd_kill(&args[2..]),
            _ => {}
        }

        // Default behavior: run the daemon.
        run()
    }

    /// 默认 socket 路径 (§16.1)。测试可用 Z3RM_MUX_SOCKET 覆盖。
    fn default_socket_path() -> PathBuf {
        if let Ok(p) = std::env::var("Z3RM_MUX_SOCKET") {
            return PathBuf::from(p);
        }
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime_dir)
        } else {
            PathBuf::from("/tmp")
        }
        .join("z3rm")
        .join("mux.sock")
    }

    /// §16.14 z3rm-server status 子命令
    /// 连接到 daemon,枚举所有 session 的 pane,显示真实统计 + daemon 内存。
    fn cmd_status() -> Result<()> {
        let socket_path = default_socket_path();

        if !socket_path.exists() {
            eprintln!(
                "z3rm-server is not running (socket not found: {})",
                socket_path.display()
            );
            std::process::exit(1);
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        rt.block_on(async {
            let domain = mux::connect_local(Some(socket_path.as_path())).await?;
            let sessions = domain.list_sessions().await?;

            // §16.14 真实 pane 数量:attach 每个 session,数 tabs[].panes[]。
            // 旧实现是 session_count * 2 编的数字,严重误导诊断。
            let mut total_panes: usize = 0;
            for s in &sessions {
                if let Ok(attach) = domain.attach(&s.id, mux::AttachMode::ReadOnly).await {
                    if let Some(snap) = &attach.snapshot {
                        for tab in &snap.tabs {
                            total_panes += tab.panes.len();
                        }
                    }
                }
            }

            // §16.14 daemon 内存:从 PID 文件读取 daemon PID,再用 sysinfo 查。
            // 旧实现查的是 status 命令自己的 PID,永远是几 MB (status 进程本身)。
            let pid_path = socket_path.with_extension("pid");
            let daemon_pid = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok());
            let mut sys = sysinfo::System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let daemon_mem = daemon_pid
                .and_then(|pid| sys.process(sysinfo::Pid::from(pid)).map(|p| p.memory()))
                .unwrap_or(0);

            let uptime = socket_path
                .metadata()
                .ok()
                .and_then(|m| {
                    m.modified().ok().map(|t| {
                        let elapsed = std::time::SystemTime::now()
                            .duration_since(t)
                            .unwrap_or_default();
                        let hours = elapsed.as_secs() / 3600;
                        let mins = (elapsed.as_secs() % 3600) / 60;
                        format!("{hours}h {mins}m")
                    })
                })
                .unwrap_or_else(|| "unknown".to_string());

            let session_count = sessions.len();
            let attached = sessions.iter().filter(|s| s.attached_clients > 0).count();
            println!("z3rm-server v0.1.0");
            println!("Uptime: {uptime}");
            println!("Sessions: {session_count} ({attached} attached)");
            println!("Panes: {total_panes}");
            println!("Memory: {} MB", daemon_mem / 1024 / 1024);
            println!("Socket: {}", socket_path.display());
            if let Some(pid) = daemon_pid {
                println!("PID: {pid}");
            }

            Ok::<_, anyhow::Error>(())
        })
    }

    /// §3.5 z3rm-server kill 子命令
    ///
    /// - `z3rm-server kill` → 终止整个 daemon (kill 全部 session + 移除 socket)
    /// - `z3rm-server kill --session <id>` → 仅结束指定 session,daemon 继续运行
    ///
    /// §3.5 keep_alive 默认 true: session 全部结束后 daemon 仍存活,
    /// 由 keep_alive_seconds 空闲计时自动退出。
    fn cmd_kill(args: &[String]) -> Result<()> {
        let socket_path = default_socket_path();

        if !socket_path.exists() {
            eprintln!("z3rm-server is not running");
            std::process::exit(1);
        }

        // §3.5 解析 --session <id>:仅结束该 session。
        // 形态: `kill --session <id>` / `kill --session=<id>`。
        let session_id = parse_session_flag(args);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        rt.block_on(async {
            let domain = mux::connect_local(Some(socket_path.as_path())).await?;

            if let Some(id) = &session_id {
                // §3.5 z3rm-server kill --session <id>:发送 KillSession RPC,daemon 继续运行。
                domain.kill_session(id).await?;
                println!("session {id} killed successfully");
            } else {
                // §3.5 z3rm-server kill: request an acknowledged process shutdown.
                domain.shutdown().await?;
                println!("z3rm-server killed successfully");
            }

            Ok::<_, anyhow::Error>(())
        })
    }

    /// §3.5 解析 `--session <id>` / `--session=<id>` flag。
    ///
    /// 返回 None 表示未提供 --session (kill 整个 daemon)。
    /// 提供了但缺少值时报错退出 (而非 panic)。
    fn parse_session_flag(args: &[String]) -> Option<String> {
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if let Some(val) = arg.strip_prefix("--session=") {
                return Some(val.to_string());
            }
            if arg == "--session" {
                match iter.next() {
                    Some(v) if !v.is_empty() => return Some(v.clone()),
                    _ => {
                        eprintln!("error: --session requires a value");
                        std::process::exit(2);
                    }
                }
            }
        }
        None
    }

}

#[cfg(not(target_family = "wasm"))]
fn main() -> anyhow::Result<()> {
    daemon::main()
}
