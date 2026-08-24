//! §3.1 The subsystems the browser server does not have.
//!
//! `extension_host`, `persistence` and `snapshot` all sit on things a tab does
//! not get: a QuickJS runtime on its own OS thread, a SQLite file, and a
//! filesystem watcher. Rather than gate every request handler that mentions
//! them — which would fork `connection.rs` into two versions of the same
//! dispatch — the wasm build gets these stand-ins at the same paths with the
//! same signatures. Every entry point reports that the feature is unavailable
//! instead of pretending to succeed, so a client sees a typed error rather
//! than a silent no-op.

/// §5 The extension host, absent.
pub mod extension_host {
    use anyhow::Result;
    use std::sync::Arc;

    type Sessions = Arc<parking_lot::RwLock<Vec<crate::session::Session>>>;

    pub struct ServerExtensionHost;

    impl ServerExtensionHost {
        pub fn new() -> Self {
            Self
        }

        pub fn bind_sessions(self: &Arc<Self>, _sessions: &Sessions) {}

        pub fn request_render(&self) {}

        pub async fn install_extension(
            &self,
            _request: &mux_protocol::InstallExtensionRequest,
        ) -> Result<()> {
            anyhow::bail!("installing extensions needs the QuickJS host, which the browser build does not run")
        }

        pub async fn execute_chrome_action(
            &self,
            _request: &mux_protocol::ExtensionChromeActionRequest,
        ) -> Result<()> {
            anyhow::bail!("extension chrome actions need the QuickJS host, which the browser build does not run")
        }
    }

    impl Default for ServerExtensionHost {
        fn default() -> Self {
            Self::new()
        }
    }
}

/// §3.7 Session persistence, absent.
pub mod persistence {
    use anyhow::Result;

    /// Mirrors the native scan result so `handle_list_recovery_candidates`
    /// compiles unchanged; the browser has nothing persisted to recover.
    pub struct RecoveryScan {
        pub candidates: Vec<RecoveryCandidate>,
        pub rejected: Vec<String>,
    }

    pub struct RecoveryCandidate {
        pub id: String,
        pub name: String,
        pub cwd: String,
        pub layout: crate::layout::LayoutTree,
        pub panes: Vec<PersistedPane>,
        pub tabs: Vec<(String, String, Vec<String>)>,
        pub focused_tab: Option<String>,
        pub focused_pane: Option<String>,
    }

    /// Mirrors the native persisted pane row. The list is always empty here.
    pub struct PersistedPane {
        pub id: String,
        pub cwd: String,
        pub title: String,
        pub cols: u32,
        pub rows: u32,
    }

    /// Stands in for `sqlez::connection::Connection` in handler signatures.
    /// Nothing can be stored, so nothing can be read back.
    pub struct Connection;

    pub fn init_tables() -> Result<()> {
        Ok(())
    }

    pub fn recovery_candidates<Db>(_db: &Db) -> Result<RecoveryScan> {
        Ok(RecoveryScan {
            candidates: Vec::new(),
            rejected: Vec::new(),
        })
    }

    pub fn snapshot_sessions<Db, Sessions>(_db: &Db, _sessions: &Sessions) -> Result<()> {
        Ok(())
    }
}

/// §4 Shadow snapshot, absent.
pub mod snapshot {
    use anyhow::Result;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SnapshotTrigger {
        Write,
    }

    pub struct FileVersion {
        pub version_id: u64,
        pub seq_no: u64,
        pub trigger: SnapshotTrigger,
    }

    pub struct FileChange {
        pub path: PathBuf,
        pub version_count: u64,
        pub latest_seq_no: u64,
    }

    /// Shadow snapshot needs a filesystem watcher and a WAL on disk. The
    /// browser has neither, so a watch is never started and every query
    /// reports that rather than returning an empty history, which would read
    /// as "this file never changed".
    pub struct SnapshotWatch;

    impl SnapshotWatch {
        pub fn stop(&self) {}

        pub fn list_changed_files(&self) -> Result<Vec<FileChange>> {
            anyhow::bail!("shadow snapshot is not available in the browser build")
        }

        pub fn list_versions(&self, _path: PathBuf) -> Result<Vec<FileVersion>> {
            anyhow::bail!("shadow snapshot is not available in the browser build")
        }

        pub fn get_version(&self, _path: PathBuf, _version_id: u64) -> Result<Option<Vec<u8>>> {
            anyhow::bail!("shadow snapshot is not available in the browser build")
        }

        pub fn decline(&self, _path: PathBuf, _version_id: u64) -> Result<()> {
            anyhow::bail!("shadow snapshot is not available in the browser build")
        }
    }

    /// `Ok(None)`, not an error: a session starting without shadow snapshot is
    /// the normal browser case, and the native path already treats `None` as
    /// "not watching".
    pub fn start(_session_id: &str, _cwd: &str) -> Result<Option<Arc<SnapshotWatch>>> {
        Ok(None)
    }
}
