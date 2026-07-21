// §3.1 z3rm-server daemon 入口点。
// 绑定本地 socket，接受连接，服务 mux protocol RPC。

use anyhow::Result;
use mux_server::run;
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // §16.12 解析 CLI 子命令
    match args.get(1).map(String::as_str) {
        Some("status") => cmd_status(),
        Some("kill") => cmd_kill(),
        _ => {
            // 默认行为: 运行 daemon
            run()
        }
    }
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
        eprintln!("z3rm-server is not running (socket not found: {})", socket_path.display());
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

        let uptime = socket_path.metadata().ok().and_then(|m| {
            m.modified().ok().map(|t| {
                let elapsed = std::time::SystemTime::now().duration_since(t).unwrap_or_default();
                let hours = elapsed.as_secs() / 3600;
                let mins = (elapsed.as_secs() % 3600) / 60;
                format!("{hours}h {mins}m")
            })
        }).unwrap_or_else(|| "unknown".to_string());

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

/// §16.12 z3rm-server kill 子命令
fn cmd_kill() -> Result<()> {
    let socket_path = default_socket_path();

    if !socket_path.exists() {
        eprintln!("z3rm-server is not running");
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let domain = mux::connect_local(Some(socket_path.as_path())).await?;
        let sessions = domain.list_sessions().await?;
        for session in &sessions {
            let _ = domain.kill_session(&session.id).await;
        }

        drop(domain);

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = std::fs::remove_file(&socket_path);

        println!("z3rm-server killed successfully");
        Ok::<_, anyhow::Error>(())
    })
}
