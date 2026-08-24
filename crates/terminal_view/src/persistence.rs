use anyhow::Result;
use std::path::PathBuf;

#[cfg(not(target_family = "wasm"))]
use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{ItemId, WorkspaceDb, WorkspaceId};

// The TerminalPanel pane-grid serialization that lived here went away with
// the panel itself (§3.1); only the terminal database remains.

#[cfg(not(target_family = "wasm"))]
pub struct TerminalDb(ThreadSafeConnection);

#[cfg(not(target_family = "wasm"))]
impl Domain for TerminalDb {
    const NAME: &str = stringify!(TerminalDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE terminals (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        ),
        // Remove the unique constraint on the item_id table
        // SQLite doesn't have a way of doing this automatically, so
        // we have to do this silly copying.
        sql!(
            CREATE TABLE terminals2 (
                workspace_id INTEGER,
                item_id INTEGER,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;

            INSERT INTO terminals2 (workspace_id, item_id, working_directory)
            SELECT workspace_id, item_id, working_directory FROM terminals;

            DROP TABLE terminals;

            ALTER TABLE terminals2 RENAME TO terminals;
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN working_directory_path TEXT;
            UPDATE terminals SET working_directory_path = CAST(working_directory AS TEXT);
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN custom_title TEXT;
        ),
    ];
}

#[cfg(not(target_family = "wasm"))]
db::static_connection!(TerminalDb, [WorkspaceDb]);

#[cfg(not(target_family = "wasm"))]
impl TerminalDb {
    query! {
       pub async fn update_workspace_id(
            new_id: WorkspaceId,
            old_id: WorkspaceId,
            item_id: ItemId
        ) -> Result<()> {
            UPDATE terminals
            SET workspace_id = ?
            WHERE workspace_id = ? AND item_id = ?
        }
    }

    pub async fn save_working_directory(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        working_directory: PathBuf,
    ) -> Result<()> {
        log::debug!(
            "Saving working directory {working_directory:?} for item {item_id} in workspace {workspace_id:?}"
        );
        let query =
            "INSERT INTO terminals(item_id, workspace_id, working_directory, working_directory_path)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT DO UPDATE SET
                item_id = ?1,
                workspace_id = ?2,
                working_directory = ?3,
                working_directory_path = ?4"
        ;
        self.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            next_index = statement.bind(&working_directory, next_index)?;
            statement.bind(
                &working_directory.to_string_lossy().into_owned(),
                next_index,
            )?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_working_directory(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
            SELECT working_directory
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub async fn save_custom_title(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        custom_title: Option<String>,
    ) -> Result<()> {
        log::debug!(
            "Saving custom title {:?} for item {} in workspace {:?}",
            custom_title,
            item_id,
            workspace_id
        );
        self.write(move |conn| {
            let query = "INSERT INTO terminals (item_id, workspace_id, custom_title)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    custom_title = excluded.custom_title";
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            statement.bind(&custom_title, next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_custom_title(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT custom_title
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}

// =============================================================================
// WASM IN-MEMORY PERSISTENCE
// =============================================================================
//
// On wasm32-unknown-unknown there is no SQLite. This module provides an
// in-memory `TerminalDb` whose public signatures are byte-identical to the
// native SQLite-backed version. Semantics:
//
// * Writes are accepted and stored in process memory for the lifetime of the
//   browser session (a `LazyLock<Mutex<...>>` static; the web client owns a
//   single App, so process-local storage has the same lifetime as the per-App
//   SQLite database used on native).
// * Reads return what was written earlier in the session, or `None` if
//   nothing has been persisted for that key.
// * Records are keyed by `(item_id, workspace_id)` and hold the working
//   directory plus the custom title, matching the native `terminals` table.

#[cfg(target_family = "wasm")]
mod wasm_terminal_db {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{LazyLock, Mutex};

    use anyhow::Result;
    use gpui::App;
    use workspace::{ItemId, WorkspaceId};

    /// Per terminal item: working directory + optional custom title.
    #[derive(Default)]
    struct TerminalRecord {
        working_directory: Option<PathBuf>,
        custom_title: Option<String>,
    }

    static TERMINAL_TABLE: LazyLock<Mutex<HashMap<(ItemId, WorkspaceId), TerminalRecord>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn table() -> &'static Mutex<HashMap<(ItemId, WorkspaceId), TerminalRecord>> {
        &TERMINAL_TABLE
    }

    /// A poisoned lock means another thread panicked mid-write. There is only
    /// one thread in the browser build, so this cannot happen; recovering the
    /// guard rather than propagating keeps the store from becoming permanently
    /// unreadable if that ever changes.
    fn with_table<R>(body: impl FnOnce(&mut HashMap<(ItemId, WorkspaceId), TerminalRecord>) -> R) -> R {
        let mut guard = match table().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        body(&mut guard)
    }

    /// In-memory `TerminalDb` for wasm.
    #[derive(Clone, Default)]
    pub struct TerminalDb;

    impl std::ops::Deref for TerminalDb {
        type Target = ();
        fn deref(&self) -> &Self::Target {
            &()
        }
    }

    impl TerminalDb {
        /// Same signature as `db::static_connection!(TerminalDb, [WorkspaceDb])`
        /// on native.
        pub fn global(_cx: &App) -> Self {
            TerminalDb
        }

        /// wasm shim for `query! { pub async fn update_workspace_id(...) }`.
        /// Re-keys every record for `item_id` from `old_id` to `new_id`.
        pub async fn update_workspace_id(
            &self,
            new_id: WorkspaceId,
            old_id: WorkspaceId,
            item_id: ItemId,
        ) -> Result<()> {
            with_table(|table| {
                if let Some(record) = table.remove(&(item_id, old_id)) {
                    table.insert((item_id, new_id), record);
                }
            });
            Ok(())
        }

        /// wasm shim for `save_working_directory`.
        pub async fn save_working_directory(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
            working_directory: PathBuf,
        ) -> Result<()> {
            log::debug!(
                "Saving working directory {working_directory:?} for item {item_id} in workspace {workspace_id:?}"
            );
            with_table(|table| {
                let record = table
                    .entry((item_id, workspace_id))
                    .or_insert_with(TerminalRecord::default);
                record.working_directory = Some(working_directory);
            });
            Ok(())
        }

        /// wasm shim for `query! { pub fn get_working_directory(...) }`.
        pub fn get_working_directory(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
        ) -> Result<Option<PathBuf>> {
            let guard = match table().lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            Ok(guard
                .get(&(item_id, workspace_id))
                .and_then(|record| record.working_directory.clone()))
        }

        /// wasm shim for `save_custom_title`.
        pub async fn save_custom_title(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
            custom_title: Option<String>,
        ) -> Result<()> {
            log::debug!(
                "Saving custom title {:?} for item {} in workspace {:?}",
                custom_title,
                item_id,
                workspace_id
            );
            with_table(|table| {
                let record = table
                    .entry((item_id, workspace_id))
                    .or_insert_with(TerminalRecord::default);
                record.custom_title = custom_title;
            });
            Ok(())
        }

        /// wasm shim for `query! { pub fn get_custom_title(...) }`.
        pub fn get_custom_title(
            &self,
            item_id: ItemId,
            workspace_id: WorkspaceId,
        ) -> Result<Option<String>> {
            let guard = match table().lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            Ok(guard
                .get(&(item_id, workspace_id))
                .and_then(|record| record.custom_title.clone()))
        }
    }
}

#[cfg(target_family = "wasm")]
pub use wasm_terminal_db::TerminalDb;
