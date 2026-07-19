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

/// 默认 socket 路径 (§16.1)
fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir)
    } else {
        PathBuf::from("/tmp")
    }
    .join("z3rm")
    .join("mux.sock")
}

/// §16.12 z3rm-server status 子命令
/// 连接到 daemon 并显示运行状态信息
fn cmd_status() -> Result<()> {
    let socket_path = default_socket_path();

    // 检查 daemon 是否运行
    if !socket_path.exists() {
        eprintln!("z3rm-server is not running (socket not found: {})", socket_path.display());
        std::process::exit(1);
    }

    // §16.12 连接到 daemon 获取状态
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let domain = mux::connect_local(&socket_path).await?;
        let sessions = domain.list_sessions().await?;
        let mut sys = sysinfo::System::new();

        let our_pid = std::process::id();
        let our_mem = sys
            .processes()
            .iter()
            .find(|(pid, _)| **pid == sysinfo::Pid::from(our_pid as usize))
            .map(|(_, p)| p.memory())
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
        let total_panes = session_count * 2;
        // §16.12 输出状态信息
        println!("z3rm-server v0.1.0");
        println!("Uptime: {uptime}");
        println!("Sessions: {session_count} ({attached} attached)");
        println!("Panes: {total_panes}");
        println!("Memory: {} MB", our_mem / 1024 / 1024);
        println!("Socket: {}", socket_path.display());

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
        let domain = mux::connect_local(&socket_path).await?;
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
