//! WAL（Write-Ahead Log）：预写式日志，§4 Layer 0
//!
//! append-only 日志，支持 replay 和 checkpoint。
//! Group commit：防抖窗口内多变更合并一次 fsync。

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use parking_lot::Mutex;

use crate::version_tree::{ContentHash, DeltaRef, PathHash, SeqNo, SnapshotTrigger, VersionId};

/// WAL 条目
#[derive(Debug, Clone)]
pub struct WalEntry {
    /// 全局单调序列号
    pub seq_no: SeqNo,
    /// 文件路径 Blake3 哈希
    pub path_hash: PathHash,
    /// 父版本 ID
    pub parent_id: Option<VersionId>,
    /// 完整快照内容哈希（full snapshot）
    pub content_ref: Option<ContentHash>,
    /// 增量引用（delta snapshot）
    pub delta_ref: Option<DeltaRef>,
    /// 快照触发原因
    pub trigger: SnapshotTrigger,
}

/// WAL 二进制记录格式：
/// [magic:4][seq_no:8][path_hash:32][parent_id_flag:1][parent_id:8?][
///   content_ref_flag:1][content_hash:32?][delta_ref_flag:1][
///   delta_hash:32?][compressed_size:8?][trigger:1][checksum:8]
const MAGIC: u32 = 0x_666F_726D; // "form"

/// WAL 日志管理器
pub struct Wal {
    /// WAL 文件路径
    path: std::path::PathBuf,
    /// 写入文件（mutex 保护并发写入）
    file: Mutex<BufWriter<File>>,
}

impl Wal {
    /// 创建或打开 WAL 文件
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(BufWriter::new(file)),
        })
    }

    /// 追加一条 WAL 记录
    ///
    /// 注意：追加后不自动 fsync，由调用者决定何时 group commit。
    pub fn append(&self, entry: &WalEntry) -> io::Result<()> {
        let mut file = self.file.lock();
        Self::encode_entry(entry, &mut *file)?;
        Ok(())
    }

    /// Group commit：flush + fsync
    ///
    /// 防抖窗口内累积多个 append 后调用一次，减少磁盘同步次数。
    pub fn commit(&self) -> io::Result<()> {
        let mut file = self.file.lock();
        file.flush()?;
        file.get_ref().sync_all()?;
        Ok(())
    }

    /// Checkpoint：flush 后截断已持久化的 WAL 部分
    ///
    /// MemTable flush 到 SQLite 后调用，清除已处理的 WAL 条目。
    pub fn checkpoint(&self) -> io::Result<()> {
        let mut file = self.file.lock();
        file.flush()?;
        file.get_ref().sync_all()?;
        // 截断文件：已处理的 WAL 条目被清除
        file.get_ref().set_len(0)?;
        file.get_ref().sync_all()?;
        Ok(())
    }

    /// Replay：从头读取所有 WAL 条目
    ///
    /// 崩溃恢复时调用，重建 MemTable。
    pub fn replay(&self) -> io::Result<Vec<WalEntry>> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();

        loop {
            // 读 magic
            let mut magic_buf = [0u8; 4];
            match reader.read_exact(&mut magic_buf) {
                Ok(_) => {},
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }

            let magic = u32::from_le_bytes(magic_buf);
            if magic != MAGIC {
                // 无效记录，停止 replay
                break;
            }

            let entry = Self::decode_entry(&mut reader)?;
            entries.push(entry);
        }

        Ok(entries)
    }

    /// 编码 WAL 条目到 writer
    fn encode_entry(entry: &WalEntry, w: &mut impl Write) -> io::Result<()> {
        // magic
        w.write_all(&MAGIC.to_le_bytes())?;
        // seq_no
        w.write_all(&entry.seq_no.to_le_bytes())?;
        // path_hash
        w.write_all(&entry.path_hash)?;
        // parent_id
        let has_parent = entry.parent_id.is_some();
        w.write_all(&[has_parent as u8])?;
        if has_parent {
            w.write_all(&entry.parent_id.unwrap().to_le_bytes())?;
        }
        // content_ref
        let has_content = entry.content_ref.is_some();
        w.write_all(&[has_content as u8])?;
        if has_content {
            w.write_all(&entry.content_ref.unwrap())?;
        }
        // delta_ref
        let has_delta = entry.delta_ref.is_some();
        w.write_all(&[has_delta as u8])?;
        if has_delta {
            let delta = entry.delta_ref.as_ref().unwrap();
            w.write_all(&delta.hash)?;
            w.write_all(&delta.compressed_size.to_le_bytes())?;
        }
        // trigger (u8: 0=Write, 1=Close, 2=Debounce, 3=Decline, 4=Delete, 5=DeclineDone)
        w.write_all(&[match entry.trigger {
            SnapshotTrigger::Write => 0,
            SnapshotTrigger::Close => 1,
            SnapshotTrigger::Debounce => 2,
            SnapshotTrigger::Decline => 3,
            SnapshotTrigger::Delete => 4,
            SnapshotTrigger::DeclineDone => 5,
        }])?;

        Ok(())
    }

    /// 从 reader 解码 WAL 条目
    fn decode_entry(r: &mut impl Read) -> io::Result<WalEntry> {
        let mut buf = [0u8; 8];

        // seq_no
        r.read_exact(&mut buf)?;
        let seq_no = u64::from_le_bytes(buf);

        // path_hash
        let mut path_hash = [0u8; 32];
        r.read_exact(&mut path_hash)?;

        // parent_id
        let mut flag = [0u8; 1];
        r.read_exact(&mut flag)?;
        let parent_id = if flag[0] != 0 {
            r.read_exact(&mut buf)?;
            Some(u64::from_le_bytes(buf))
        } else {
            None
        };

        // content_ref
        r.read_exact(&mut flag)?;
        let content_ref = if flag[0] != 0 {
            let mut hash = [0u8; 32];
            r.read_exact(&mut hash)?;
            Some(hash)
        } else {
            None
        };

        // delta_ref
        r.read_exact(&mut flag)?;
        let delta_ref = if flag[0] != 0 {
            let mut hash = [0u8; 32];
            r.read_exact(&mut hash)?;
            r.read_exact(&mut buf)?;
            let compressed_size = u64::from_le_bytes(buf);
            Some(DeltaRef { hash, compressed_size })
        } else {
            None
        };

        // trigger
        r.read_exact(&mut flag)?;
        let trigger = match flag[0] {
            0 => SnapshotTrigger::Write,
            1 => SnapshotTrigger::Close,
            2 => SnapshotTrigger::Debounce,
            3 => SnapshotTrigger::Decline,
            4 => SnapshotTrigger::Delete,
            5 => SnapshotTrigger::DeclineDone,
            _ => SnapshotTrigger::Write,
        };

        Ok(WalEntry {
            seq_no,
            path_hash,
            parent_id,
            content_ref,
            delta_ref,
            trigger,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(seq_no: u64) -> WalEntry {
        WalEntry {
            seq_no,
            path_hash: [seq_no as u8; 32],
            parent_id: Some(seq_no - 1),
            content_ref: None,
            delta_ref: None,
            trigger: SnapshotTrigger::Write,
        }
    }

    #[test]
    fn test_wal_append_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let wal = Wal::open(&path).unwrap();

        for i in 1..=5 {
            wal.append(&make_entry(i)).unwrap();
        }
        wal.commit().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 5);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.seq_no, (i as u64 + 1));
        }
    }

    #[test]
    fn test_wal_checkpoint_clears_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let wal = Wal::open(&path).unwrap();

        for i in 1..=3 {
            wal.append(&make_entry(i)).unwrap();
        }
        wal.commit().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 3);

        // Checkpoint 截断
        wal.checkpoint().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_wal_all_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let wal = Wal::open(&path).unwrap();

        let triggers = [
            SnapshotTrigger::Write,
            SnapshotTrigger::Close,
            SnapshotTrigger::Debounce,
            SnapshotTrigger::Decline,
            SnapshotTrigger::DeclineDone,
            SnapshotTrigger::Delete,
        ];

        for (i, trigger) in triggers.iter().enumerate() {
            let entry = WalEntry {
                seq_no: (i + 1) as u64,
                path_hash: [0u8; 32],
                parent_id: None,
                content_ref: None,
                delta_ref: None,
                trigger: *trigger,
            };
            wal.append(&entry).unwrap();
        }
        wal.commit().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 6);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.trigger, triggers[i]);
        }
    }
}
