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
    /// 创建或打开 WAL 文件
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
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
