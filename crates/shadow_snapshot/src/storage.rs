//! Storage：Layer 2，SQLite 持久化 + content-addressed blob store
//!
//! SQLite 存储版本节点元数据。Blob store 按内容哈希分片存储，
//! 小 blob (< 4KB) 直接内联到 SQLite，大 blob 存磁盘。
//! Zstd level-1 压缩，refcounted 垃圾回收。

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{Connection, params, OptionalExtension};
use sha2::{Sha256, Digest};

use crate::version_tree::{ContentHash, PathHash, SeqNo, SnapshotTrigger, VersionId};

/// 小 blob 阈值：小于此值内联到 SQLite
const INLINE_THRESHOLD: u64 = 4096;

/// 内容哈希的前 2 字节作为分片目录名
fn shard_dir(hash: &ContentHash) -> String {
    format!("{:02x}", hash[0])
}

/// 将内容哈希格式化为 64 位十六进制字符串
fn hash_to_hex(hash: &ContentHash) -> String {
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

/// SQLite schema
const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;

CREATE TABLE IF NOT EXISTS version_nodes (
    version_id INTEGER PRIMARY KEY,
    path_hash BLOB NOT NULL,
    seq_no INTEGER NOT NULL,
    parent_id INTEGER,
    full_content_hash BLOB,
    delta_hash BLOB,
    delta_depth INTEGER NOT NULL DEFAULT 0,
    trigger TEXT NOT NULL,
    timestamp_ns INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_id) REFERENCES version_nodes(version_id)
);

CREATE INDEX IF NOT EXISTS idx_seq ON version_nodes(seq_no);
CREATE INDEX IF NOT EXISTS idx_path_seq ON version_nodes(path_hash, seq_no DESC);

CREATE TABLE IF NOT EXISTS blob_refs (
    content_hash BLOB PRIMARY KEY,
    ref_count INTEGER NOT NULL DEFAULT 1,
    size INTEGER NOT NULL,
    compressed INTEGER NOT NULL DEFAULT 0,
    inline_data BLOB
);

CREATE TABLE IF NOT EXISTS gc_queue (
    version_id INTEGER PRIMARY KEY,
    marked_at INTEGER NOT NULL DEFAULT 0
);
"#;

/// StorageEngine:SQLite 版本节点存储。
pub struct StorageEngine {
    conn: Connection,
}

impl StorageEngine {
    /// 打开或创建 SQLite 数据库
    pub fn open<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(SCHEMA)
            .context("Failed to initialize schema")?;
        Ok(Self { conn })
    }

    /// 写入版本节点到 SQLite
    pub fn write_node(
        &self,
        version_id: VersionId,
        path_hash: &PathHash,
        seq_no: SeqNo,
        parent_id: Option<VersionId>,
        full_content: Option<&ContentHash>,
        delta_hash: Option<&ContentHash>,
        delta_depth: u8,
        trigger: SnapshotTrigger,
        timestamp_ns: u128,
    ) -> Result<()> {
        let trigger_str = match trigger {
            SnapshotTrigger::Write => "Write",
            SnapshotTrigger::Close => "Close",
            SnapshotTrigger::Debounce => "Debounce",
            SnapshotTrigger::Decline => "Decline",
            SnapshotTrigger::Delete => "Delete",
        };

        self.conn.execute(
            "INSERT OR REPLACE INTO version_nodes
             (version_id, path_hash, seq_no, parent_id, full_content_hash,
              delta_hash, delta_depth, trigger, timestamp_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                version_id as i64,
                path_hash as &[u8],
                seq_no as i64,
                parent_id.map(|v| v as i64),
                full_content,
                delta_hash,
                delta_depth as i32,
                trigger_str,
                timestamp_ns as i64,
            ],
        )?;
        Ok(())
    }

    /// 查询版本节点存在性
    pub fn has_node(&self, version_id: VersionId) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM version_nodes WHERE version_id = ?1",
        )?;
        let count: i64 = stmt.query_row(params![version_id as i64], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// 查询指定路径的最新版本
    pub fn get_head_by_path(&self, path_hash: &PathHash) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT version_id FROM version_nodes
             WHERE path_hash = ?1 ORDER BY seq_no DESC LIMIT 1",
        )?;
        let result: Option<i64> = stmt.query_row(params![path_hash as &[u8]], |row| {
            row.get(0)
        })
        .optional()?;
        Ok(result)
    }

    /// 查询指定序列号范围内的所有节点
    pub fn query_by_seq_range(&self, from: SeqNo, to: SeqNo) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT version_id FROM version_nodes
             WHERE seq_no >= ?1 AND seq_no <= ?2 ORDER BY seq_no",
        )?;
        let mut rows = stmt.query(params![from as i64, to as i64])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get::<_, i64>(0)?);
        }
        Ok(ids)
    }

    /// 删除版本节点
    pub fn delete_node(&self, version_id: VersionId) -> Result<()> {
        self.conn
            .execute("DELETE FROM version_nodes WHERE version_id = ?1", params![version_id as i64])?;
        Ok(())
    }

    /// 获取连接引用（用于高级查询）
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// BlobStore：content-addressed 对象存储
///
/// 小对象 (< 4KB) 内联存 SQLite，大对象存磁盘分片目录。
/// Zstd 压缩 + 引用计数。
/// BlobStore:content-addressed 对象存储。
///
/// §4.5 持有 Arc<StorageEngine> 而非 StorageEngine by value,这样调用方
/// 可以同时持有 StorageEngine (调 write_node) 和 BlobStore (调 put/get)。
/// 旧 by-value 设计强迫二选一,严重限制 high-level engine 实现。
pub struct BlobStore {
    engine: Arc<StorageEngine>,
    blob_dir: std::path::PathBuf,
}

impl BlobStore {
    /// 创建 BlobStore,与调用方共享 StorageEngine。
    pub fn new(engine: impl Into<Arc<StorageEngine>>, blob_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            engine: engine.into(),
            blob_dir: blob_dir.into(),
        }
    }

    /// 计算内容的 SHA-256 哈希
    pub fn compute_hash(data: &[u8]) -> ContentHash {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher.finalize().into()
    }

    /// 存储 blob：内容 → 哈希
    ///
    /// 如果已存在则递增 refcount，否则写入并初始化 refcount=1。
    /// 小 blob 内联到 SQLite，大 blob 写磁盘。
    pub fn put(&self, data: &[u8]) -> Result<ContentHash> {
        let hash = Self::compute_hash(data);
        let size = data.len() as u64;

        // Zstd 压缩
        let compressed = zstd::encode_all(data, 1)
            .context("Zstd compression failed")?;
        let is_compressed = compressed.len() < data.len();
        let store_data = if is_compressed { &compressed } else { data };
        let compressed_flag = if is_compressed { 1 } else { 0 };

        let inline = size < INLINE_THRESHOLD;

        // 检查是否已存在
        let existing: Option<i64> = self.engine.connection().query_row(
            "SELECT ref_count FROM blob_refs WHERE content_hash = ?1",
            params![&hash[..] as &[u8]],
            |row| row.get(0),
        ).optional()?;

        if existing.is_some() {
            // 已存在 → refcount++
            self.engine.connection().execute(
                "UPDATE blob_refs SET ref_count = ref_count + 1 WHERE content_hash = ?1",
                params![&hash[..] as &[u8]],
            )?;
        } else {
            if inline {
                // 小 blob：内联到 SQLite
                self.engine.connection().execute(
                    "INSERT INTO blob_refs (content_hash, ref_count, size, compressed, inline_data)
                     VALUES (?1, 1, ?2, ?3, ?4)",
                    params![&hash[..] as &[u8], size as i64, compressed_flag, store_data as &[u8]],
                )?;
            } else {
                // 大 blob：写磁盘
                let shard = shard_dir(&hash);
                let shard_path = self.blob_dir.join(&shard);
                fs::create_dir_all(&shard_path)?;
                let blob_path = shard_path.join(hash_to_hex(&hash));
                let mut file = File::create(&blob_path)?;
                file.write_all(store_data)?;
                file.sync_all()?;

                self.engine.connection().execute(
                    "INSERT INTO blob_refs (content_hash, ref_count, size, compressed, inline_data)
                     VALUES (?1, 1, ?2, ?3, NULL)",
                    params![&hash[..] as &[u8], size as i64, compressed_flag],
                )?;
            }
        }

        Ok(hash)
    }

    /// 读取 blob：哈希 → 内容
    pub fn get(&self, hash: &ContentHash) -> Result<Vec<u8>> {
        let row: (i64, i32, Option<Vec<u8>>) = self.engine.connection().query_row(
            "SELECT size, compressed, inline_data FROM blob_refs WHERE content_hash = ?1",
            params![&hash[..] as &[u8]],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        if let Some(data) = row.2 {
            // 内联 blob
            if row.1 != 0 {
                // 已压缩，需要解压缩
                zstd::decode_all(&data[..])
                    .context("Zstd decompression failed")
            } else {
                Ok(data)
            }
        } else {
            // 磁盘 blob
            let shard = shard_dir(hash);
            let blob_path = self.blob_dir.join(&shard).join(hash_to_hex(hash));
            let data = fs::read(&blob_path)?;
            if row.1 != 0 {
                zstd::decode_all(data.as_slice())
                    .context("Zstd decompression failed")
            } else {
                Ok(data)
            }
        }
    }

    /// 释放引用：refcount--，为 0 时删除
    pub fn unref(&self, hash: &ContentHash) -> Result<()> {
        let new_count: i64 = self.engine.connection().query_row(
            "SELECT ref_count FROM blob_refs WHERE content_hash = ?1",
            params![&hash[..] as &[u8]],
            |row| row.get::<_, i64>(0),
        ).optional()?
        .map(|c| c - 1)
        .unwrap_or(0);

        if new_count <= 0 {
            self.delete_blob(hash)?;
        } else {
            self.engine.connection().execute(
                "UPDATE blob_refs SET ref_count = ?1 WHERE content_hash = ?2",
                params![new_count, &hash[..] as &[u8]],
            )?;
        }

        Ok(())
    }

    /// 删除 blob 数据
    fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        // 检查是否磁盘存储
        let inline: Option<Vec<u8>> = self.engine.connection().query_row(
            "SELECT inline_data FROM blob_refs WHERE content_hash = ?1",
            params![&hash[..] as &[u8]],
            |row| row.get::<_, Vec<u8>>(0),
        ).optional()?;

        if inline.is_none() {
            // 磁盘 blob：删除文件
            let shard = shard_dir(hash);
            let blob_path = self.blob_dir.join(&shard).join(hash_to_hex(hash));
            if blob_path.exists() {
                fs::remove_file(&blob_path)?;
            }
        }

        // 删除 SQLite 记录
        self.engine.connection().execute(
            "DELETE FROM blob_refs WHERE content_hash = ?1",
            params![&hash[..] as &[u8]],
        )?;

        Ok(())
    }

    /// 获取 blob 引用计数
    pub fn refcount(&self, hash: &ContentHash) -> Result<Option<i64>> {
        let count: Option<i64> = self.engine.connection().query_row(
            "SELECT ref_count FROM blob_refs WHERE content_hash = ?1",
            params![&hash[..] as &[u8]],
            |row| row.get::<_, i64>(0),
        ).optional()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_write_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let engine = StorageEngine::open(dir.path().join("test.db")).unwrap();

        let path_hash: PathHash = [0x42; 32];
        engine
            .write_node(
                1,
                &path_hash,
                100,
                None,
                None,
                None,
                0,
                SnapshotTrigger::Write,
                0,
            )
            .unwrap();

        let head = engine.get_head_by_path(&path_hash).unwrap();
        assert_eq!(head, Some(1));

        let ids = engine.query_by_seq_range(1, 200).unwrap();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn test_blob_inline() {
        let dir = tempfile::tempdir().unwrap();
        let engine = StorageEngine::open(dir.path().join("test.db")).unwrap();
        let store = BlobStore::new(engine, dir.path().join("blobs"));

        let data = b"hello, small blob"; // < 4KB
        let hash = store.put(data).unwrap();

        let got = store.get(&hash).unwrap();
        assert_eq!(got, data.as_slice());

        let rc = store.refcount(&hash).unwrap();
        assert_eq!(rc, Some(1));
    }

    #[test]
    fn test_blob_large() {
        let dir = tempfile::tempdir().unwrap();
        let engine = StorageEngine::open(dir.path().join("test.db")).unwrap();
        let store = BlobStore::new(engine, dir.path().join("blobs"));

        // > 4KB
        let data = vec![0xABu8; 5000];
        let hash = store.put(&data).unwrap();

        let got = store.get(&hash).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn test_blob_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let engine = StorageEngine::open(dir.path().join("test.db")).unwrap();
        let store = BlobStore::new(engine, dir.path().join("blobs"));

        let data = b"dedup test";
        store.put(data).unwrap();
        store.put(data).unwrap(); // 重复写入

        let rc = store.refcount(&BlobStore::compute_hash(data)).unwrap();
        assert_eq!(rc, Some(2));

        // 释放一次
        let hash = BlobStore::compute_hash(data);
        store.unref(&hash).unwrap();
        let rc = store.refcount(&hash).unwrap();
        assert_eq!(rc, Some(1));

        // 释放第二次 → blob 删除
        store.unref(&hash).unwrap();
        let rc = store.refcount(&hash).unwrap();
        assert_eq!(rc, None);
    }
}
