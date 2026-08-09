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

/// Legacy records used an unframed payload after `LEGACY_MAGIC`. New records
/// are self-delimiting and checksummed:
/// `[magic:4][version:1][payload_len:4][payload][blake3:32]`.
const LEGACY_MAGIC: u32 = 0x_666F_726D; // "form"
const FRAMED_MAGIC: u32 = 0x_7A33_574C; // "z3WL"
const FRAME_VERSION: u8 = 1;
const CHECKSUM_LEN: usize = 32;
const MAX_PAYLOAD_LEN: usize = 1024;

/// WAL 日志管理器
pub struct Wal {
    /// WAL 文件路径
    path: std::path::PathBuf,
    /// 写入文件（mutex 保护并发写入）
    file: Mutex<BufWriter<File>>,
}

impl Wal {
    /// 创建或打开 WAL 文件。
    ///
    /// 打开前先做旋转崩溃恢复:`checkpoint` 把日志原子 rename 到
    /// `<wal>.old` 后才会创建新的空日志,崩溃可能留下"规范路径缺失或为
    /// 空、归档仍持有完整旧日志"的状态。此时必须把归档恢复到规范路径,
    /// 让 replay 看到全部条目,而不是静默以空日志启动。规范路径与归档
    /// 同时有数据是意外状态,打开失败关闭(不丢弃任何一份)。
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        Self::recover_rotated_log(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(BufWriter::new(file)),
        })
    }

    /// 旋转崩溃恢复(见 [`Wal::open`] 文档)。
    ///
    /// 只处理两种崩溃窗口:规范路径缺失 + 归档有数据,规范路径为空 +
    /// 归档有数据。两种情况都通过原子 rename 把归档恢复到规范路径;
    /// 恢复幂等,崩溃后重开会再次恢复,无需 fsync 父目录。两边都有数据
    /// 是意外状态,返回 `InvalidData` 失败关闭。归档路径被目录等非文件
    /// 占住时不动它——那不是可恢复的旋转状态,下次 checkpoint 的 rename
    /// 会失败并报错。
    fn recover_rotated_log(path: &Path) -> io::Result<()> {
        let archive_path = Self::archive_path(path);
        let archive_len = match std::fs::metadata(&archive_path) {
            Ok(meta) if meta.is_file() => Some(meta.len()),
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let canonical_len = match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => Some(meta.len()),
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        match (
            canonical_len.is_some_and(|len| len > 0),
            archive_len.is_some_and(|len| len > 0),
        ) {
            (false, true) => std::fs::rename(&archive_path, path)?,
            (true, true) => {
                return Err(invalid_data(format!(
                    "WAL rotation state is ambiguous: both {} and {} contain records",
                    path.display(),
                    archive_path.display()
                )));
            }
            _ => {}
        }
        Ok(())
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

    /// Checkpoint：把 WAL 原子旋转为空,前提是每条既有条目都已在上游
    /// (SQLite / blob store) 持久化。
    ///
    /// 先 flush + fsync 既有条目,再把整份日志原子 rename 到 `<wal>.old`,
    /// 最后在规范路径创建新的空日志并 fsync 父目录。旧文件从不原地截断,
    /// 因此崩溃在任意一点都只会留下"规范路径仍是旧的有效日志"或"新的
    /// 有效空日志"(旧日志整份在归档中)——绝不会出现部分截断的日志。
    /// 旋转持久化后归档被删除;删除失败只留下一个陈旧文件(下次旋转的
    /// rename 会原子覆盖),记录日志而不让 checkpoint 失败。
    ///
    /// 必须与 `append` 串行执行(内部锁保证):旋转期间并发 append 会写进
    /// 已归档的旧 inode,导致条目从规范路径丢失。
    pub fn checkpoint(&self) -> io::Result<()> {
        let mut file = self.file.lock();

        // 1. 先让既有条目全部落盘,再动路径。
        file.flush()?;
        file.get_ref().sync_all()?;

        // 2. 原子旋转:把整份日志 rename 到固定归档名。rename 是原子元数据
        // 操作,崩溃只会留下"规范路径仍是旧日志"或"旧日志整份在归档"两种
        // 状态。归档路径被目录占住(或其它 I/O 错误)时旋转失败,旧日志
        // 原封不动。
        let archive_path = Self::archive_path(&self.path);
        match std::fs::rename(&self.path, &archive_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // 上次旋转崩溃后规范路径尚未重建;归档仍持旧日志,这里只需
                // 创建新的空日志——旧条目早已全部持久化。
            }
            Err(error) => return Err(error),
        }

        // 3. 在规范路径创建新的空日志,并把写句柄换到新 inode——否则后续
        // append 会写进已归档的旧 inode。
        let new_file = OpenOptions::new()
            .create(true)
            .append(true)
            .truncate(true)
            .open(&self.path)?;
        *file = BufWriter::new(new_file);
        file.flush()?;
        file.get_ref().sync_all()?;

        // 4. fsync 父目录,让 rename 与新日志的创建持久化。
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            File::open(parent)?.sync_all()?;
        }
        drop(file);

        // 5. 新空日志已持久,归档不再承担恢复职责。
        match std::fs::remove_file(&archive_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %archive_path.display(),
                error = %error,
                "shadow WAL: stale archive cleanup failed"
            ),
        }
        Ok(())
    }

    /// 归档路径:`<wal>.old`。固定名称,下次旋转的 rename 会原子覆盖旧归档。
    fn archive_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".old");
        std::path::PathBuf::from(name)
    }

    /// Replay：从头读取所有 WAL 条目
    ///
    /// 崩溃恢复时调用，重建 MemTable。
    pub fn replay(&self) -> io::Result<Vec<WalEntry>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            // 打开句柄存活期间规范路径被移除(外部干扰或异常状态)时,
            // WAL 等价于空日志,而不是损坏状态。
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();

        loop {
            let Some(magic_buf) = read_or_torn_tail::<4>(&mut reader)? else {
                break;
            };
            let magic = u32::from_le_bytes(magic_buf);
            match magic {
                FRAMED_MAGIC => {
                    let Some(header) = read_or_torn_tail::<5>(&mut reader)? else {
                        break;
                    };
                    let version = header[0];
                    if version != FRAME_VERSION {
                        return Err(invalid_data(format!(
                            "unsupported WAL frame version {version}"
                        )));
                    }
                    let payload_len_bytes = [header[1], header[2], header[3], header[4]];
                    let payload_len = u32::from_le_bytes(payload_len_bytes) as usize;
                    if payload_len > MAX_PAYLOAD_LEN {
                        return Err(invalid_data(format!(
                            "WAL payload length {payload_len} exceeds {MAX_PAYLOAD_LEN}"
                        )));
                    }
                    let Some(payload) = read_or_torn_tail_vec(&mut reader, payload_len)? else {
                        break;
                    };
                    let Some(checksum) = read_or_torn_tail::<CHECKSUM_LEN>(&mut reader)? else {
                        break;
                    };
                    let expected = frame_checksum(version, &header[1..5], &payload);
                    if checksum != expected {
                        return Err(invalid_data("WAL frame checksum mismatch"));
                    }
                    let mut payload_reader = std::io::Cursor::new(payload);
                    let entry = Self::decode_entry(&mut payload_reader)?;
                    if payload_reader.position() != payload_len as u64 {
                        return Err(invalid_data("WAL frame contains trailing payload bytes"));
                    }
                    entries.push(entry);
                }
                LEGACY_MAGIC => match Self::decode_entry(&mut reader) {
                    Ok(entry) => entries.push(entry),
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error),
                },
                _ => return Err(invalid_data(format!("invalid WAL magic {magic:#010x}"))),
            }
        }

        Ok(entries)
    }

    fn encode_entry(entry: &WalEntry, w: &mut impl Write) -> io::Result<()> {
        let mut payload = Vec::with_capacity(128);
        Self::encode_payload(entry, &mut payload)?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| invalid_data("WAL payload length exceeds u32"))?;
        let payload_len_bytes = payload_len.to_le_bytes();
        let checksum = frame_checksum(FRAME_VERSION, &payload_len_bytes, &payload);

        w.write_all(&FRAMED_MAGIC.to_le_bytes())?;
        w.write_all(&[FRAME_VERSION])?;
        w.write_all(&payload_len_bytes)?;
        w.write_all(&payload)?;
        w.write_all(&checksum)?;
        Ok(())
    }

    fn encode_payload(entry: &WalEntry, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&entry.seq_no.to_le_bytes())?;
        w.write_all(&entry.path_hash)?;
        let has_parent = entry.parent_id.is_some();
        w.write_all(&[has_parent as u8])?;
        if let Some(parent_id) = entry.parent_id {
            w.write_all(&parent_id.to_le_bytes())?;
        }
        let has_content = entry.content_ref.is_some();
        w.write_all(&[has_content as u8])?;
        if let Some(content_ref) = entry.content_ref {
            w.write_all(&content_ref)?;
        }
        let has_delta = entry.delta_ref.is_some();
        w.write_all(&[has_delta as u8])?;
        if let Some(delta) = &entry.delta_ref {
            w.write_all(&delta.hash)?;
            w.write_all(&delta.compressed_size.to_le_bytes())?;
        }
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
        let parent_id = match flag[0] {
            0 => None,
            1 => {
                r.read_exact(&mut buf)?;
                Some(u64::from_le_bytes(buf))
            }
            value => return Err(invalid_data(format!("invalid WAL parent flag {value}"))),
        };

        // content_ref
        r.read_exact(&mut flag)?;
        let content_ref = match flag[0] {
            0 => None,
            1 => {
                let mut hash = [0u8; 32];
                r.read_exact(&mut hash)?;
                Some(hash)
            }
            value => return Err(invalid_data(format!("invalid WAL content flag {value}"))),
        };

        // delta_ref
        r.read_exact(&mut flag)?;
        let delta_ref = match flag[0] {
            0 => None,
            1 => {
                let mut hash = [0u8; 32];
                r.read_exact(&mut hash)?;
                r.read_exact(&mut buf)?;
                let compressed_size = u64::from_le_bytes(buf);
                Some(DeltaRef {
                    hash,
                    compressed_size,
                })
            }
            value => return Err(invalid_data(format!("invalid WAL delta flag {value}"))),
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
            value => return Err(invalid_data(format!("invalid WAL trigger {value}"))),
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

fn frame_checksum(version: u8, payload_len: &[u8], payload: &[u8]) -> [u8; CHECKSUM_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[version]);
    hasher.update(payload_len);
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn read_or_torn_tail<const N: usize>(reader: &mut impl Read) -> io::Result<Option<[u8; N]>> {
    let mut bytes = [0; N];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_or_torn_tail_vec(reader: &mut impl Read, len: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = vec![0; len];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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

        // Checkpoint 旋转清空
        wal.checkpoint().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 0);
    }

    /// 崩溃点 1:checkpoint 的 rename 已生效、fresh file 尚未创建(规范路径
    /// 缺失)。正常的 `Wal::open` 必须恢复归档,replay 看到全部旧条目——
    /// 而不是像旧行为那样创建一个空日志,静默丢掉归档里的条目。
    #[test]
    fn open_recovers_archived_log_after_crash_before_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash-missing.wal");
        let archive = Wal::archive_path(&path);

        let wal = Wal::open(&path).unwrap();
        for i in 1..=3 {
            wal.append(&make_entry(i)).unwrap();
        }
        wal.commit().unwrap();
        drop(wal);

        // 崩溃窗口:rename 已生效,fresh file 尚未创建。
        std::fs::rename(&path, &archive).unwrap();
        assert!(!path.exists(), "canonical path missing after crashed rotation");

        // 通过普通 open 恢复:归档移回规范路径,replay 看到全部条目。
        let reopened = Wal::open(&path).unwrap();
        let entries = reopened.replay().unwrap();
        assert_eq!(entries.len(), 3, "recovery must restore the archived log");
        assert_eq!(entries[0].seq_no, 1);
        assert_eq!(entries[2].seq_no, 3);
        assert!(
            !archive.exists(),
            "archive must be restored into the canonical path"
        );
    }

    /// 崩溃点 2:fresh 空文件已创建、归档尚未删除(规范路径存在但为空)。
    /// 恢复必须把归档移回规范路径,不能静默偏好空文件。
    #[test]
    fn open_recovers_archived_log_when_canonical_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash-empty.wal");
        let archive = Wal::archive_path(&path);

        let wal = Wal::open(&path).unwrap();
        for i in 1..=3 {
            wal.append(&make_entry(i)).unwrap();
        }
        wal.commit().unwrap();
        drop(wal);

        // 崩溃窗口:rename 已生效,fresh 空文件已创建,归档尚未删除。
        std::fs::rename(&path, &archive).unwrap();
        std::fs::write(&path, b"").unwrap();

        let reopened = Wal::open(&path).unwrap();
        let entries = reopened.replay().unwrap();
        assert_eq!(
            entries.len(),
            3,
            "recovery must not prefer the empty canonical file"
        );
        assert_eq!(entries[0].seq_no, 1);
        assert!(!archive.exists());
    }

    /// 规范路径与归档同时有数据是意外状态:打开失败关闭,绝不丢弃任何
    /// 一份日志。
    #[test]
    fn open_fails_closed_when_canonical_and_archive_both_have_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous.wal");

        let wal = Wal::open(&path).unwrap();
        wal.append(&make_entry(1)).unwrap();
        wal.commit().unwrap();
        drop(wal);

        let archive = Wal::archive_path(&path);
        let archived = Wal::open(&archive).unwrap();
        archived.append(&make_entry(2)).unwrap();
        archived.commit().unwrap();
        drop(archived);

        let error = Wal::open(&path).expect_err("ambiguous rotation state must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// A blocked rotation target must fail the checkpoint and leave the old
    /// WAL untouched and fully replayable; appends must keep working on the
    /// same handle.
    #[test]
    fn checkpoint_failure_keeps_old_wal_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.wal");
        let wal = Wal::open(&path).unwrap();
        for i in 1..=3 {
            wal.append(&make_entry(i)).unwrap();
        }
        wal.commit().unwrap();

        // 归档路径被目录占住:旋转 rename 必须失败,旧日志原封不动。
        let archive = Wal::archive_path(&path);
        std::fs::create_dir(&archive).unwrap();
        wal.checkpoint()
            .expect_err("blocked rotation must fail the checkpoint");

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 3, "failed checkpoint must keep the old log");

        // 同一句柄继续追加,落到规范路径的新内容。
        wal.append(&make_entry(4)).unwrap();
        wal.commit().unwrap();
        assert_eq!(wal.replay().unwrap().len(), 4);
    }

    /// A successful rotation leaves a fresh empty canonical log, drops the
    /// archive, and keeps appending on the same handle.
    #[test]
    fn rotation_continues_appending_and_drops_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotate.wal");
        let wal = Wal::open(&path).unwrap();
        for i in 1..=3 {
            wal.append(&make_entry(i)).unwrap();
        }
        wal.commit().unwrap();

        wal.checkpoint().unwrap();
        assert!(wal.replay().unwrap().is_empty());
        assert!(
            !Wal::archive_path(&path).exists(),
            "rotation must drop the archive"
        );

        wal.append(&make_entry(4)).unwrap();
        wal.commit().unwrap();
        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].seq_no, 4);
    }

    /// A missing canonical log at replay time is an empty WAL, not an error
    /// (live-handle file removal or rotation edge cases).
    #[test]
    fn replay_missing_log_is_an_empty_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.wal");
        let wal = Wal::open(&path).unwrap();
        wal.append(&make_entry(1)).unwrap();
        wal.commit().unwrap();

        std::fs::remove_file(&path).unwrap();
        assert!(wal.replay().unwrap().is_empty());
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

    #[test]
    fn replay_ignores_only_a_torn_final_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn-tail.wal");
        let wal = Wal::open(&path).unwrap();
        wal.append(&make_entry(1)).unwrap();
        wal.append(&make_entry(2)).unwrap();
        wal.commit().unwrap();

        let original_len = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(original_len - 5)
            .unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].seq_no, 1);
    }

    #[test]
    fn replay_rejects_corrupted_complete_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupted.wal");
        let wal = Wal::open(&path).unwrap();
        wal.append(&make_entry(1)).unwrap();
        wal.commit().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let payload_byte = bytes.len() / 2;
        bytes[payload_byte] ^= 0x80;
        std::fs::write(&path, bytes).unwrap();

        let error = wal.replay().expect_err("corrupted frame must fail replay");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn replay_accepts_legacy_records_followed_by_framed_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed-format.wal");
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&LEGACY_MAGIC.to_le_bytes());
        Wal::encode_payload(&make_entry(1), &mut legacy).unwrap();
        std::fs::write(&path, legacy).unwrap();

        let wal = Wal::open(&path).unwrap();
        wal.append(&make_entry(2)).unwrap();
        wal.commit().unwrap();

        let entries = wal.replay().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq_no, 1);
        assert_eq!(entries[1].seq_no, 2);
    }

    #[test]
    fn payload_decode_rejects_unknown_trigger() {
        let mut payload = Vec::new();
        Wal::encode_payload(&make_entry(1), &mut payload).unwrap();
        *payload.last_mut().unwrap() = u8::MAX;

        let error = Wal::decode_entry(&mut payload.as_slice())
            .expect_err("unknown trigger must not silently become Write");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn payload_decode_rejects_unknown_option_flag() {
        let mut payload = Vec::new();
        Wal::encode_payload(&make_entry(1), &mut payload).unwrap();
        let parent_flag_offset = 8 + 32;
        payload[parent_flag_offset] = 2;

        let error = Wal::decode_entry(&mut payload.as_slice())
            .expect_err("unknown option flag must not change payload shape");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
