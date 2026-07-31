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
    let upsert_sql =
        "INSERT OR REPLACE INTO sessions (id, name, cwd, layout_snapshot, last_snapshot_timestamp)
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
        let layout_snapshot = session.layout.serialize(cols, rows)?;

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

pub fn delete_session(conn: &Connection, session_id: &str) -> anyhow::Result<()> {
    conn.exec("BEGIN IMMEDIATE")?()?;
    let result = (|| {
        conn.exec_bound::<&str>("DELETE FROM layout_nodes WHERE session_id = ?")?(session_id)?;
        conn.exec_bound::<&str>("DELETE FROM sessions WHERE id = ?")?(session_id)?;
        conn.exec("COMMIT")?()?;
        anyhow::Ok(())
    })();
    if result.is_err()
        && let Ok(mut rollback) = conn.exec("ROLLBACK")
    {
        if let Err(error) = rollback() {
            tracing::error!(%error, "failed to roll back session deletion");
        }
    }
    result
}

#[derive(Debug)]
pub struct RecoveryCandidate {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub layout: crate::layout::LayoutTree,
}

#[derive(Debug)]
pub struct RecoveryScan {
    pub candidates: Vec<RecoveryCandidate>,
    pub rejected: Vec<String>,
}

pub fn recovery_candidates(conn: &Connection) -> anyhow::Result<RecoveryScan> {
    let mut stmt = Statement::prepare(
        conn,
        "SELECT id, name, cwd, layout_snapshot FROM sessions ORDER BY last_snapshot_timestamp DESC",
    )?;
    let rows = stmt.map(|stmt| {
        Ok((
            stmt.column_text(0)?.to_owned(),
            stmt.column_text(1)?.to_owned(),
            stmt.column_text(2)?.to_owned(),
            stmt.column_text(3)?.to_owned(),
        ))
    })?;

    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    for (id, name, cwd, layout_snapshot) in rows {
        let validation = (|| {
            anyhow::ensure!(!layout_snapshot.is_empty(), "session {id} has no persisted layout");
            let layout = crate::layout::LayoutTree::deserialize(&layout_snapshot)
                .map_err(|error| anyhow::anyhow!("invalid persisted layout for session {id}: {error}"))?;
            let pane_ids = layout.pane_ids();
            anyhow::ensure!(
                !pane_ids.is_empty() && pane_ids.iter().all(|pane_id| !pane_id.is_empty()),
                "persisted layout for session {id} has no recoverable panes"
            );
            anyhow::Ok(RecoveryCandidate { id, name, cwd, layout })
        })();
        match validation {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => rejected.push(error.to_string()),
        }
    }
    Ok(RecoveryScan { candidates, rejected })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_session_is_not_recovered() {
        let connection = Connection::open_memory(Some("deleted_session_is_not_recovered"));
        init_tables(&connection).expect("initialize persistence tables");
        let mut keep = crate::session::Session::new(
            "keep".to_string(),
            "keep".to_string(),
            "/tmp".to_string(),
        );
        keep.layout = crate::layout::LayoutTree::with_pane(
            "keep-node".to_string(),
            "keep-pane".to_string(),
        );
        let mut kill = crate::session::Session::new(
            "kill".to_string(),
            "kill".to_string(),
            "/tmp".to_string(),
        );
        kill.layout = crate::layout::LayoutTree::with_pane(
            "kill-node".to_string(),
            "kill-pane".to_string(),
        );
        let sessions = Arc::new(parking_lot::RwLock::new(vec![keep, kill]));
        let database = Arc::new(parking_lot::Mutex::new(connection));
        snapshot_sessions(&sessions, &database).expect("snapshot sessions");

        {
            let connection = database.lock();
            delete_session(&connection, "kill").expect("delete killed session");
        }
        let scan = {
            let connection = database.lock();
            recovery_candidates(&connection).expect("load remaining recovery candidates")
        };

        assert!(scan.rejected.is_empty());
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].id, "keep");
        assert_eq!(scan.candidates[0].layout.pane_ids(), vec!["keep-pane"]);
    }

    #[test]
    fn corrupt_persisted_layout_is_not_published_as_a_candidate() {
        let connection = Connection::open_memory(Some("corrupt_persisted_layout"));
        init_tables(&connection).expect("initialize persistence tables");
        let mut insert = Statement::prepare(
            &connection,
            "INSERT INTO sessions (id, name, cwd, layout_snapshot, last_snapshot_timestamp) VALUES (?, ?, ?, ?, ?)",
        )
        .expect("prepare corrupt candidate insert");
        insert.bind(&"broken", 1).expect("bind id");
        insert.bind(&"broken", 2).expect("bind name");
        insert.bind(&"/tmp", 3).expect("bind cwd");
        insert.bind(&"not-a-layout", 4).expect("bind layout");
        insert.bind(&0_i64, 5).expect("bind timestamp");
        insert.exec().expect("insert corrupt candidate");

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("invalid persisted layout"));
    }

    #[test]
    fn empty_session_is_not_published_as_a_recovery_candidate() {
        let connection = Connection::open_memory(Some("empty_recovery_candidate"));
        init_tables(&connection).expect("initialize persistence tables");
        let sessions = Arc::new(parking_lot::RwLock::new(vec![
            crate::session::Session::new(
                "empty".to_string(),
                "empty".to_string(),
                "/tmp".to_string(),
            ),
        ]));
        let database = Arc::new(parking_lot::Mutex::new(connection));
        snapshot_sessions(&sessions, &database).expect("snapshot empty session");

        let scan = {
            let connection = database.lock();
            recovery_candidates(&connection).expect("scan recovery candidates")
        };
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("no recoverable panes"));
    }
}
