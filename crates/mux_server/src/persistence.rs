// §3.6 Persistence 模块 — SQLite 布局元数据持久化。
// 每 10s 快照 session layout metadata。grid 内容不持久化 (§3.6)。

use serde::{Deserialize, Serialize};
use sqlez::connection::Connection;
use sqlez::statement::Statement;
use std::sync::Arc;
use std::time::Duration;

const RECOVERY_FORMAT_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionState {
    version: u32,
    /// §3.7 类型化布局树 (前序节点表), 重建时校验结构不变量。
    layout: crate::layout::LayoutTree,
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
}

// §3.6 SQLite schema: session 元数据表
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    cwd TEXT NOT NULL,
    layout_snapshot TEXT,  -- §3.7 类型化 layout tree JSON 信封
    last_snapshot_timestamp INTEGER NOT NULL  -- Unix 毫秒
)
"#;

/// §3.6 初始化数据库表
pub fn init_tables(conn: &Connection) -> anyhow::Result<()> {
    // §3.6 WAL mode: 后台 persist_loop 每 10s 写入, 不能阻塞 RPC 线程的并发读 (spec §3.6)。
    let mut wal = Statement::prepare(conn, "PRAGMA journal_mode=WAL;")?;
    wal.exec()?;
    let mut stmt = Statement::prepare(conn, SCHEMA_SQL)?;
    stmt.exec()?;
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
    layout: crate::layout::LayoutTree,
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

    let now = web_time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // §3.6 UPSERT session: INSERT OR REPLACE
    let upsert_sql =
        "INSERT OR REPLACE INTO sessions (id, name, cwd, layout_snapshot, last_snapshot_timestamp)
                      VALUES (?, ?, ?, ?, ?)";

    for session in &*sessions_r {
        // §3.7 类型化 layout tree 直接入 JSON 信封: 保留节点 ID 与精确比例,
        // 恢复时重建出与保存时完全一致的树。
        let persisted_state = persisted_session_state(session, session.layout.clone())?;
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
}

#[derive(Clone, Debug)]
pub struct RecoveryScan {
    pub candidates: Vec<RecoveryCandidate>,
    pub rejected: Vec<String>,
}

/// §3.7 解码持久化行: 先核对信封版本, 再解析类型化 layout (反序列化本身会
/// 执行完整结构校验)。旧格式行 (tmux 风格字符串 / 旧 version) 以明确错误
/// 拒绝, 绝不静默降级。
fn decode_persisted_state(session_id: &str, value: &str) -> anyhow::Result<PersistedSessionState> {
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|error| {
        anyhow::anyhow!("invalid persisted layout for session {session_id}: {error}")
    })?;
    let version = parsed
        .get("version")
        .and_then(|version| version.as_u64())
        .unwrap_or(0) as u32;
    anyhow::ensure!(
        version == RECOVERY_FORMAT_VERSION,
        "unsupported recovery format {version} for session {session_id}"
    );
    serde_json::from_value(parsed).map_err(|error| {
        anyhow::anyhow!("invalid persisted layout for session {session_id}: {error}")
    })
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
            let state = decode_persisted_state(&id, &layout_snapshot)?;
            let layout = state.layout.clone();
            let pane_ids = layout.pane_ids();
            anyhow::ensure!(
                !pane_ids.is_empty() && pane_ids.iter().all(|pane_id| !pane_id.is_empty()),
                "persisted layout for session {id} has no recoverable panes"
            );
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
            anyhow::Ok(RecoveryCandidate {
                id,
                name,
                cwd,
                layout,
                tabs: state
                    .tabs
                    .into_iter()
                    .map(|tab| (tab.id, tab.title, tab.pane_ids))
                    .collect(),
                panes: state.panes,
                focused_tab: state.focused_tab,
                focused_pane: state.focused_pane,
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
        let layout = crate::layout::LayoutTree::with_pane(
            format!("{pane_id}-node"),
            pane_id.to_string(),
        );
        serde_json::to_string(&layout).expect("serialize test layout")
    }

    fn persisted_pane(id: &str) -> PersistedPane {
        PersistedPane {
            id: id.to_string(),
            cwd: "/tmp".to_string(),
            title: "cat".to_string(),
            cols: 80,
            rows: 24,
        }
    }

    fn envelope_json(pane_id: &str) -> String {
        serde_json::to_string(&PersistedSessionState {
            version: RECOVERY_FORMAT_VERSION,
            layout: crate::layout::LayoutTree::with_pane(
                format!("{pane_id}-node"),
                pane_id.to_string(),
            ),
            tabs: vec![PersistedTab {
                id: "tab-1".to_string(),
                title: "shell".to_string(),
                pane_ids: vec![pane_id.to_string()],
            }],
            panes: vec![persisted_pane(pane_id)],
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
        assert_eq!(scan.candidates[0].panes[0].id, "keep-pane");
        assert_eq!(scan.candidates[0].tabs[0].0, "tab-1");
    }

    /// 旧格式裸 layout 行 (tmux 风格字符串, 无 JSON 信封) 在类型化 cutover
    /// 后不再被当作 "incomplete candidate", 而是带错误信息拒绝 —— 绝不静默
    /// 降级成单 pane 布局。
    #[test]
    fn legacy_raw_layout_row_is_rejected_after_typed_cutover() {
        let connection = Connection::open_memory(Some("legacy_rejected_after_cutover"));
        init_tables(&connection).expect("initialize persistence tables");
        insert_session_row(
            &connection,
            "legacy",
            "legacy",
            "/tmp",
            &raw_layout("legacy-pane"),
        );
        let scan = recovery_candidates(&connection).expect("scan recovery candidates");

        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(
            scan.rejected[0].contains("unsupported recovery format"),
            "unexpected rejection: {}",
            scan.rejected[0]
        );
    }

    #[test]
    fn unsupported_recovery_format_version_is_rejected() {
        let connection = Connection::open_memory(Some("unsupported_recovery_version"));
        init_tables(&connection).expect("initialize persistence tables");
        let mut envelope = serde_json::from_str::<serde_json::Value>(&envelope_json("pane-1"))
            .expect("decode test recovery envelope");
        envelope["version"] = serde_json::Value::from(1u32);
        insert_session_row(
            &connection,
            "old-format",
            "old-format",
            "/tmp",
            &serde_json::to_string(&envelope).expect("encode stale version envelope"),
        );

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("unsupported recovery format"));
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
        let empty_envelope = serde_json::to_string(&PersistedSessionState {
            version: RECOVERY_FORMAT_VERSION,
            layout: crate::layout::LayoutTree::empty(),
            tabs: Vec::new(),
            panes: Vec::new(),
            focused_tab: None,
            focused_pane: None,
        })
        .expect("encode empty session envelope");
        insert_session_row(
            &connection,
            "empty",
            "empty",
            "/tmp",
            &empty_envelope,
        );

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
        assert_eq!(candidate.panes.len(), 1);
        assert_eq!(candidate.panes[0].id, "pane-1");
        assert_eq!(candidate.focused_pane.as_deref(), Some("pane-1"));
        assert_eq!(candidate.tabs[0].2, vec!["pane-1"]);
        let live = sessions.read();
        assert_eq!(
            candidate.layout.root, live[0].layout.root,
            "snapshotted layout must round-trip the exact tree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn multi_level_layout_and_focus_round_trip_through_recovery_scan() {
        let connection = Connection::open_memory(Some("multi_level_recovery_round_trip"));
        init_tables(&connection).expect("initialize persistence tables");

        let mut layout = crate::layout::LayoutTree::with_pane(
            "node-1".to_string(),
            "pane-1".to_string(),
        );
        layout
            .split(
                "pane-1",
                "pane-2".to_string(),
                crate::layout::SplitDirection::LeftRight,
            )
            .expect("split left-right");
        layout
            .resize_pane("pane-1", crate::layout::SplitDirection::LeftRight, 0.2)
            .expect("resize outer split");
        layout
            .split(
                "pane-2",
                "pane-3".to_string(),
                crate::layout::SplitDirection::TopBottom,
            )
            .expect("split top-bottom");
        layout
            .resize_pane("pane-2", crate::layout::SplitDirection::TopBottom, 0.1)
            .expect("resize inner split");

        let envelope = serde_json::to_string(&PersistedSessionState {
            version: RECOVERY_FORMAT_VERSION,
            layout: layout.clone(),
            tabs: vec![PersistedTab {
                id: "tab-1".to_string(),
                title: "shell".to_string(),
                pane_ids: vec![
                    "pane-1".to_string(),
                    "pane-2".to_string(),
                    "pane-3".to_string(),
                ],
            }],
            panes: vec![
                persisted_pane("pane-1"),
                persisted_pane("pane-2"),
                persisted_pane("pane-3"),
            ],
            focused_tab: Some("tab-1".to_string()),
            focused_pane: Some("pane-3".to_string()),
        })
        .expect("encode multi-level recovery envelope");
        insert_session_row(
            &connection,
            "multi",
            "multi",
            "/tmp",
            &envelope,
        );

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.rejected.is_empty(), "rejections: {:?}", scan.rejected);
        assert_eq!(scan.candidates.len(), 1);
        let candidate = &scan.candidates[0];
        assert_eq!(
            candidate.layout.root, layout.root,
            "multi-level mixed-axis tree must round-trip exactly"
        );
        assert_eq!(candidate.focused_tab.as_deref(), Some("tab-1"));
        assert_eq!(candidate.focused_pane.as_deref(), Some("pane-3"));
        assert_eq!(
            candidate.layout.pane_ids(),
            vec!["pane-1", "pane-2", "pane-3"]
        );
    }

    /// 过期布局引用不在 pane 元数据里的 pane → 候选被拒绝, 不会带病发布。
    #[test]
    fn stale_persisted_layout_referencing_unknown_pane_is_rejected() {
        let connection = Connection::open_memory(Some("stale_persisted_layout"));
        init_tables(&connection).expect("initialize persistence tables");
        let mut envelope = serde_json::from_str::<serde_json::Value>(&envelope_json("pane-1"))
            .expect("decode test recovery envelope");
        envelope["layout"] = serde_json::from_str::<serde_json::Value>(
            &raw_layout("ghost-pane"),
        )
        .expect("encode stale layout");
        insert_session_row(
            &connection,
            "stale",
            "stale",
            "/tmp",
            &serde_json::to_string(&envelope).expect("encode stale envelope"),
        );

        let scan = recovery_candidates(&connection).expect("scan recovery candidates");
        assert!(scan.candidates.is_empty());
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].contains("does not match its layout"));
    }
}
