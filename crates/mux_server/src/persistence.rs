// §3.6 Persistence 模块 — SQLite 布局元数据持久化。
// 每 10s 快照 session layout metadata。grid 内容不持久化 (§3.6)。

use sqlez::connection::Connection;
use sqlez::statement::Statement;
use std::sync::Arc;
use std::time::Duration;

// §3.6 SQLite schema: session 元数据表
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    cwd TEXT NOT NULL,
    layout_snapshot TEXT,  -- §3.7 序列化 layout tree
    last_snapshot_timestamp INTEGER NOT NULL  -- Unix 毫秒
)
"#;

// §3.6 布局节点表 (可选: 用于更细粒度恢复)
const LAYOUT_NODES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS layout_nodes (
    session_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    node_type TEXT NOT NULL,  -- 'pane' 或 'split'
    pane_id TEXT,             -- 仅 pane 节点有值
    direction TEXT,           -- 仅 split 节点: 'H' 或 'V'
    ratio REAL,               -- §3.7 尺寸比例
    parent_node_id TEXT,      -- §3.7 父节点 ID
    PRIMARY KEY (session_id, node_id),
    FOREIGN KEY (session_id) REFERENCES sessions(id)
)
"#;

/// §3.6 初始化数据库表
pub fn init_tables(conn: &Connection) -> anyhow::Result<()> {
    // §3.6 WAL mode: 后台 persist_loop 每 10s 写入, 不能阻塞 RPC 线程的并发读 (spec §3.6)。
    let mut wal = Statement::prepare(conn, "PRAGMA journal_mode=WAL;")?;
    wal.exec()?;
    let mut stmt = Statement::prepare(conn, SCHEMA_SQL)?;
    stmt.exec()?;
    let mut stmt2 = Statement::prepare(conn, LAYOUT_NODES_SQL)?;
    stmt2.exec()?;
    Ok(())
}

/// §3.6 每 10s 快照所有 session layout metadata
pub async fn persist_loop(
    sessions: Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: Arc<parking_lot::Mutex<Connection>>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        if let Err(e) = snapshot_sessions(&sessions, &db) {
            tracing::error!(error = %e, "snapshot failed");
        }
    }
}

/// §3.6 快照所有 session
fn snapshot_sessions(
    sessions: &Arc<parking_lot::RwLock<Vec<crate::session::Session>>>,
    db: &Arc<parking_lot::Mutex<Connection>>,
) -> anyhow::Result<()> {
    let conn = db.lock();
    let sessions_r = sessions.read();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // §3.6 UPSERT session: INSERT OR REPLACE
    let upsert_sql = "INSERT OR REPLACE INTO sessions (id, name, cwd, layout_snapshot, last_snapshot_timestamp)
                      VALUES (?, ?, ?, ?, ?)";

    for session in &*sessions_r {
        // §3.7 序列化 layout tree (tmux 风格绝对 cell 计数)。
        // container 尺寸取自任一 constituent pane —— session 不单独记录窗口尺寸;
        // ratios 决定相对切分, 绝对值只影响 WxH 数字, 结构 round-trip 不受影响。
        let (cols, rows) = {
            let panes = session.panes.read();
            panes
                .values()
                .next()
                .map(|p| (p.get_cols(), p.get_rows()))
                .unwrap_or((80, 24))
        };
        let layout_snapshot = session.layout.serialize(cols, rows).unwrap_or_default();

        let mut stmt = Statement::prepare(&*conn, upsert_sql)?;
        stmt.bind(&session.id, 1)?;
        stmt.bind(&session.name, 2)?;
        stmt.bind(&session.cwd, 3)?;
        stmt.bind(&layout_snapshot, 4)?;
        stmt.bind(&now, 5)?;
        stmt.exec()?;
    }

    Ok(())
}

pub fn recover_sessions(conn: &Connection) -> anyhow::Result<Vec<crate::session::Session>> {
    let mut stmt = Statement::prepare(
        conn,
        "SELECT id, name, cwd, layout_snapshot FROM sessions ORDER BY last_snapshot_timestamp DESC",
    )?;

    stmt.map(|stmt| {
        let id: String = stmt.column_text(0)?.to_owned();
        let name: String = stmt.column_text(1)?.to_owned();
        let cwd: String = stmt.column_text(2)?.to_owned();
        Ok(crate::session::Session::new(id, name, cwd))
    })
}
