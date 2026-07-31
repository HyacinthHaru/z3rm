// §3.6 Persistence 模块 — SQLite 布局元数据持久化。
// 每 10s 快照 session layout metadata。grid 内容不持久化 (§3.6)。

use sqlez::connection::Connection;
use sqlez::statement::Statement;
use std::sync::Arc;
use std::time::Duration;
use serde::{Deserialize, Serialize};

const RECOVERY_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionState {
    version: u32,
    layout: String,
    tabs: Vec<PersistedTab>,
    panes: Vec<PersistedPane>,
    focused_tab: Option<String>,
    focused_pane: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedTab {
    id: String,
    title: String,
    pane_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PersistedPane {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub cols: u32,
    pub rows: u32,
    /// Informational only. Recovery always starts a fresh default shell.
    pub prior_command: Option<String>,
}


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

fn persisted_session_state(
    session: &crate::session::Session,
    layout: String,
) -> anyhow::Result<PersistedSessionState> {
    let mut tabs = session
        .tabs
        .values()
        .map(|tab| PersistedTab {
            id: tab.id.clone(),
            title: tab.title.clone(),
            pane_ids: tab.pane_ids.clone(),
        })
        .collect::<Vec<_>>();
    tabs.sort_by(|left, right| left.id.cmp(&right.id));

    let panes = session.panes.read();
    let mut pane_metadata = panes
        .values()
        .map(|pane| PersistedPane {
            id: pane.id.clone(),
            cwd: pane.get_cwd(),
            title: pane.get_title(),
            cols: pane.get_cols(),
            rows: pane.get_rows(),
            prior_command: pane.command.clone(),
        })
        .collect::<Vec<_>>();
    pane_metadata.sort_by(|left, right| left.id.cmp(&right.id));

    let mut layout_panes = session.layout.pane_ids();
    layout_panes.sort();
    let registry_panes = pane_metadata
        .iter()
        .map(|pane| pane.id.clone())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        layout_panes == registry_panes,
        "session {} layout panes do not match its pane registry",
        session.id
    );
    anyhow::ensure!(
        tabs.iter()
            .flat_map(|tab| &tab.pane_ids)
            .all(|pane_id| { registry_panes.binary_search(pane_id).is_ok() }),
        "session {} tab metadata references an unknown pane",
        session.id
    );
    anyhow::ensure!(
        registry_panes
            .iter()
            .all(|pane_id| { tabs.iter().any(|tab| tab.pane_ids.contains(pane_id)) }),
        "session {} has a pane not assigned to a tab",
        session.id
    );
    if let Some(focused_tab) = &session.focused_tab {
        anyhow::ensure!(
            tabs.iter().any(|tab| &tab.id == focused_tab),
            "session {} has an invalid focused tab",
            session.id
        );
    }
    if let Some(focused_pane) = &session.focused_pane {
        anyhow::ensure!(
            registry_panes.binary_search(focused_pane).is_ok(),
            "session {} has an invalid focused pane",
            session.id
        );
    }

    Ok(PersistedSessionState {
        version: RECOVERY_FORMAT_VERSION,
        layout,
        tabs,
        panes: pane_metadata,
        focused_tab: session.focused_tab.clone(),
        focused_pane: session.focused_pane.clone(),
    })
}

pub(crate) fn snapshot_sessions(
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
        let layout = session.layout.serialize(cols, rows)?;
        let persisted_state = persisted_session_state(session, layout)?;
        let layout_snapshot = serde_json::to_string(&persisted_state)?;

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

#[derive(Clone, Debug)]
pub struct RecoveryCandidate {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub layout: crate::layout::LayoutTree,
    pub tabs: Vec<(String, String, Vec<String>)>,
    pub panes: Vec<PersistedPane>,
    pub focused_tab: Option<String>,
    pub focused_pane: Option<String>,
    pub metadata_complete: bool,
}

#[derive(Clone, Debug)]
pub struct RecoveryScan {
    pub candidates: Vec<RecoveryCandidate>,
    pub rejected: Vec<String>,
}

fn decode_persisted_state(
    session_id: &str,
    value: &str,
) -> anyhow::Result<(crate::layout::LayoutTree, Option<PersistedSessionState>)> {
    if let Ok(state) = serde_json::from_str::<PersistedSessionState>(value) {
        anyhow::ensure!(
            state.version == RECOVERY_FORMAT_VERSION,
            "unsupported recovery format {} for session {session_id}",
            state.version
        );
        let layout = crate::layout::LayoutTree::deserialize(&state.layout).map_err(|error| {
            anyhow::anyhow!("invalid persisted layout for session {session_id}: {error}")
        })?;
        return Ok((layout, Some(state)));
    }
    let layout = crate::layout::LayoutTree::deserialize(value).map_err(|error| {
        anyhow::anyhow!("invalid persisted layout for session {session_id}: {error}")
    })?;
    Ok((layout, None))
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
            anyhow::ensure!(
                !layout_snapshot.is_empty(),
                "session {id} has no persisted layout"
            );
            let (layout, state) = decode_persisted_state(&id, &layout_snapshot)?;
            let pane_ids = layout.pane_ids();
            anyhow::ensure!(
                !pane_ids.is_empty() && pane_ids.iter().all(|pane_id| !pane_id.is_empty()),
                "persisted layout for session {id} has no recoverable panes"
            );
            let (tabs, panes, focused_tab, focused_pane, metadata_complete) = match state {
                Some(state) => {
                    let mut persisted_panes = state
                        .panes
                        .iter()
                        .map(|pane| pane.id.clone())
                        .collect::<Vec<_>>();
                    persisted_panes.sort();
                    anyhow::ensure!(
                        persisted_panes.windows(2).all(|ids| ids[0] != ids[1]),
                        "persisted pane metadata for session {id} contains duplicate pane ids"
                    );
                    anyhow::ensure!(
                        state.panes.iter().all(|pane| {
                            !pane.id.is_empty()
                                && mux_protocol::checked_grid_cell_count(
                                    pane.cols as usize,
                                    pane.rows as usize,
                                )
                                .is_ok()
                        }),
                        "persisted pane metadata for session {id} contains an invalid pane size"
                    );
                    let mut layout_panes = pane_ids.clone();
                    layout_panes.sort();
                    anyhow::ensure!(
                        layout_panes == persisted_panes,
                        "persisted pane metadata for session {id} does not match its layout"
                    );
                    let mut tab_ids = state.tabs.iter().map(|tab| &tab.id).collect::<Vec<_>>();
                    tab_ids.sort();
                    anyhow::ensure!(
                        tab_ids.iter().all(|tab_id| !tab_id.is_empty())
                            && tab_ids.windows(2).all(|ids| ids[0] != ids[1]),
                        "persisted tab metadata for session {id} contains invalid tab ids"
                    );
                    anyhow::ensure!(
                        state
                            .tabs
                            .iter()
                            .flat_map(|tab| &tab.pane_ids)
                            .all(|pane_id| persisted_panes.binary_search(pane_id).is_ok()),
                        "persisted tab metadata for session {id} references an unknown pane"
                    );
                    anyhow::ensure!(
                        persisted_panes.iter().all(|pane_id| {
                            state.tabs.iter().any(|tab| tab.pane_ids.contains(pane_id))
                        }),
                        "persisted pane metadata for session {id} contains an unassigned pane"
                    );
                    if let Some(focused_tab) = &state.focused_tab {
                        anyhow::ensure!(
                            state.tabs.iter().any(|tab| &tab.id == focused_tab),
                            "persisted session {id} has an invalid focused tab"
                        );
                    }
                    if let Some(focused_pane) = &state.focused_pane {
                        anyhow::ensure!(
                            persisted_panes.binary_search(focused_pane).is_ok(),
                            "persisted session {id} has an invalid focused pane"
                        );
                    }
                    (
                        state
                            .tabs
                            .into_iter()
                            .map(|tab| (tab.id, tab.title, tab.pane_ids))
                            .collect(),
                        state.panes,
                        state.focused_tab,
                        state.focused_pane,
                        true,
                    )
                }
                None => (Vec::new(), Vec::new(), None, None, false),
            };
            anyhow::Ok(RecoveryCandidate {
                id,
                name,
                cwd,
                layout,
                tabs,
                panes,
                focused_tab,
                focused_pane,
                metadata_complete,
            })
        })();
        match validation {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => rejected.push(error.to_string()),
        }
    }
    Ok(RecoveryScan {
        candidates,
        rejected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_session_row(
        connection: &Connection,
        id: &str,
        name: &str,
        cwd: &str,
        layout_snapshot: &str,
    ) {
        let mut insert = Statement::prepare(
            connection,
            "INSERT INTO sessions (id, name, cwd, layout_snapshot, last_snapshot_timestamp) VALUES (?, ?, ?, ?, ?)",
        )
        .expect("prepare session row insert");
        insert.bind(&id, 1).expect("bind id");
        insert.bind(&name, 2).expect("bind name");
        insert.bind(&cwd, 3).expect("bind cwd");
        insert.bind(&layout_snapshot, 4).expect("bind layout");
        insert.bind(&0_i64, 5).expect("bind timestamp");
        insert.exec().expect("insert session row");
    }

    fn raw_layout(pane_id: &str) -> String {
        crate::layout::LayoutTree::with_pane(format!("{pane_id}-node"), pane_id.to_string())
            .serialize(80, 24)
            .expect("serialize test layout")
    }

    fn envelope_json(pane_id: &str) -> String {
        serde_json::to_string(&PersistedSessionState {
            version: RECOVERY_FORMAT_VERSION,
            layout: raw_layout(pane_id),
            tabs: vec![PersistedTab {
                id: "tab-1".to_string(),
                title: "shell".to_string(),
                pane_ids: vec![pane_id.to_string()],
            }],
            panes: vec![PersistedPane {
                id: pane_id.to_string(),
                cwd: "/tmp".to_string(),
                title: "cat".to_string(),
                cols: 80,
                rows: 24,
                prior_command: Some("/bin/cat".to_string()),
            }],
            focused_tab: Some("tab-1".to_string()),
            focused_pane: Some(pane_id.to_string()),
        })
        .expect("serialize recovery envelope")
    }

    #[test]
    fn deleted_session_is_not_recovered() {
        let connection = Connection::open_memory(Some("deleted_session_is_not_recovered"));
        init_tables(&connection).expect("initialize persistence tables");
        insert_session_row(
            &connection,
            "keep",
            "keep",
            "/tmp",
            &envelope_json("keep-pane"),
        );
        insert_session_row(
            &connection,
            "kill",
            "kill",
            "/tmp",
            &envelope_json("kill-pane"),
        );

        delete_session(&connection, "kill").expect("delete killed session");
        let scan = recovery_candidates(&connection).expect("load remaining recovery candidates");

        assert!(scan.rejected.is_empty());
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].id, "keep");
        assert_eq!(scan.candidates[0].layout.pane_ids(), vec!["keep-pane"]);
        assert!(scan.candidates[0].metadata_complete);
        assert_eq!(scan.candidates[0].panes[0].id, "keep-pane");
        assert_eq!(scan.candidates[0].tabs[0].0, "tab-1");
    }

    #[test]
    fn legacy_layout_without_metadata_is_rejected_as_incomplete() {
        let connection = Connection::open_memory(Some("legacy_incomplete_recovery"));
        init_tables(&connection).expect("initialize persistence tables");
        insert_session_row(
            &connection,
            "legacy",
            "legacy",
            "/tmp",
            &raw_layout("legacy-pane"),
        );
        let scan = recovery_candidates(&connection).expect("scan recovery candidates");

        assert!(scan.rejected.is_empty());
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].id, "legacy");
        assert!(!scan.candidates[0].metadata_complete);
        assert!(scan.candidates[0].panes.is_empty());
    }

    #[test]
    fn invalid_recovery_focus_is_rejected_before_shell_spawn() {
        let connection = Connection::open_memory(Some("invalid_recovery_focus"));
        init_tables(&connection).expect("initialize persistence tables");
        let mut envelope = serde_json::from_str::<serde_json::Value>(&envelope_json("pane-1"))
            .expect("decode test recovery envelope");
        envelope["focused_pane"] = serde_json::Value::String("missing-pane".to_string());
        insert_session_row(
            &connection,
            "bad-focus",
            "bad-focus",
            "/tmp",
            &serde_json::to_string(&envelope).expect("encode invalid focus envelope"),
        );

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("invalid focused pane"));
    }

    #[test]
    fn corrupt_persisted_layout_is_not_published_as_a_candidate() {
        let connection = Connection::open_memory(Some("corrupt_persisted_layout"));
        init_tables(&connection).expect("initialize persistence tables");
        insert_session_row(&connection, "broken", "broken", "/tmp", "not-a-layout");

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("invalid persisted layout"));
    }

    #[test]
    fn empty_session_is_not_published_as_a_recovery_candidate() {
        let connection = Connection::open_memory(Some("empty_recovery_candidate"));
        init_tables(&connection).expect("initialize persistence tables");
        insert_session_row(&connection, "empty", "empty", "/tmp", &raw_layout(""));

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("no recoverable panes"));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_persists_complete_recovery_metadata() {
        let connection = Connection::open_memory(Some("snapshot_complete_recovery_metadata"));
        init_tables(&connection).expect("initialize persistence tables");
        let pane = crate::pane::Pane::spawn(
            "pane-1".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
            80,
            24,
            Some(crate::pane::ShellCommand {
                program: "/bin/cat".to_string(),
                ..Default::default()
            }),
        )
        .expect("spawn recovery metadata pane");
        let mut session = crate::session::Session::new(
            "meta".to_string(),
            "meta".to_string(),
            "/tmp".to_string(),
        );
        session.panes.write().insert(pane.id.clone(), pane.clone());
        session.add_tab("tab-1".to_string(), "shell".to_string());
        session
            .tabs
            .get_mut("tab-1")
            .unwrap()
            .pane_ids
            .push(pane.id.clone());
        session.layout =
            crate::layout::LayoutTree::with_pane("node-1".to_string(), pane.id.clone());
        session.set_focused_pane(pane.id.clone());
        session.focused_tab = Some("tab-1".to_string());
        let sessions = Arc::new(parking_lot::RwLock::new(vec![session]));
        let database = Arc::new(parking_lot::Mutex::new(connection));
        snapshot_sessions(&sessions, &database).expect("snapshot complete session");

        let scan = {
            let connection = database.lock();
            recovery_candidates(&connection).expect("scan complete recovery candidate")
        };

        assert!(scan.rejected.is_empty());
        assert_eq!(scan.candidates.len(), 1);
        let candidate = &scan.candidates[0];
        assert!(candidate.metadata_complete);
        assert_eq!(candidate.panes.len(), 1);
        assert_eq!(candidate.panes[0].id, "pane-1");
        assert!(
            candidate.panes[0]
                .prior_command
                .as_deref()
                .unwrap()
                .starts_with("/bin/cat")
        );
        assert_eq!(candidate.focused_pane.as_deref(), Some("pane-1"));
        assert_eq!(candidate.tabs[0].2, vec!["pane-1"]);
    }
}
