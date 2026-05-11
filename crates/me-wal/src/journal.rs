use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use me_types::{CommandEnvelope, SeqNo};

#[derive(Debug, Error)]
pub enum WalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serialization(#[from] Box<bincode::ErrorKind>),
    #[error("corrupt record at offset {offset}: {message}")]
    Corrupt { offset: u64, message: String },
}

pub type WalResult<T> = Result<T, WalError>;

/// Append-only journal writer. `append` queues a record in the internal
/// buffer; `sync` flushes the buffer to disk and fsyncs.
///
/// `append + sync` per call gives synchronous durability (slow but
/// straightforward). Group-commit batching is added in M3.2 once the
/// Disruptor pipeline marks natural batch boundaries.
#[derive(Debug)]
pub struct WalWriter {
    path: PathBuf,
    file: BufWriter<File>,
    last_written_seq: SeqNo,
    last_synced_seq: SeqNo,
}

impl WalWriter {
    pub fn open<P: AsRef<Path>>(path: P) -> WalResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: BufWriter::with_capacity(64 * 1024, file),
            last_written_seq: SeqNo(0),
            last_synced_seq: SeqNo(0),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_written_seq(&self) -> SeqNo {
        self.last_written_seq
    }

    pub fn last_synced_seq(&self) -> SeqNo {
        self.last_synced_seq
    }

    pub fn append(&mut self, env: &CommandEnvelope) -> WalResult<()> {
        let bytes = bincode::serialize(env)?;
        let len = bytes.len() as u32;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(&bytes)?;
        self.last_written_seq = env.seq_no;
        Ok(())
    }

    /// Flush the buffer to the OS, then call `sync_data` to durably persist.
    /// After this returns Ok, all previously-appended records are recoverable
    /// after a crash.
    pub fn sync(&mut self) -> WalResult<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        self.last_synced_seq = self.last_written_seq;
        Ok(())
    }
}

/// Streaming WAL reader. Yields `CommandEnvelope`s in order. Stops at EOF
/// or returns `Corrupt` if a record cannot be decoded.
pub struct WalReader {
    file: BufReader<File>,
    offset: u64,
}

impl WalReader {
    pub fn open<P: AsRef<Path>>(path: P) -> WalResult<Self> {
        let file = File::open(path)?;
        Ok(Self {
            file: BufReader::with_capacity(64 * 1024, file),
            offset: 0,
        })
    }

    /// Returns Ok(None) at EOF, Ok(Some(env)) for each successfully decoded
    /// record. A torn write at the tail (incomplete final frame) is treated
    /// as EOF — this is the typical post-crash state, and the engine is
    /// supposed to truncate or retry afterwards. A *corrupt* (non-torn) frame
    /// returns an error.
    ///
    /// Not named `next` to avoid shadowing `Iterator::next` — the signature
    /// here is `Result<Option<_>>`, not the iterator's `Option<_>`.
    pub fn read_next(&mut self) -> WalResult<Option<CommandEnvelope>> {
        let mut len_buf = [0u8; 4];
        match self.file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(WalError::Io(e)),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        // Sanity bound — a single command should never serialize beyond a few KB.
        // 1 MiB is generous; anything larger is almost certainly garbage from a
        // torn write or corruption.
        if len > 1024 * 1024 {
            return Err(WalError::Corrupt {
                offset: self.offset,
                message: format!("record length {} exceeds 1 MiB cap", len),
            });
        }
        let mut data = vec![0u8; len];
        match self.file.read_exact(&mut data) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Torn write at tail. Treat as clean EOF.
                return Ok(None);
            }
            Err(e) => return Err(WalError::Io(e)),
        }
        self.offset += 4 + len as u64;
        let env: CommandEnvelope = bincode::deserialize(&data).map_err(|e| WalError::Corrupt {
            offset: self.offset,
            message: format!("bincode: {}", e),
        })?;
        Ok(Some(env))
    }
}

/// Convenience: drain an entire WAL file into a Vec. Use only for testing or
/// small replay sets; for large WALs prefer streaming via `WalReader::next`.
pub fn read_all<P: AsRef<Path>>(path: P) -> WalResult<Vec<CommandEnvelope>> {
    if !path.as_ref().exists() {
        return Ok(Vec::new());
    }
    let mut reader = WalReader::open(path)?;
    let mut out = Vec::new();
    while let Some(env) = reader.read_next()? {
        out.push(env);
    }
    Ok(out)
}

/// Length-on-disk in bytes (informational; not load-bearing).
pub fn wal_length<P: AsRef<Path>>(path: P) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::End(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_types::{AddUser, Command, Timestamp, UserId};
    use tempfile::tempdir;

    fn env(seq: u64, uid: u64) -> CommandEnvelope {
        CommandEnvelope {
            seq_no: SeqNo(seq),
            received_at: Timestamp(seq as i64),
            command: Command::AddUser(AddUser { user_id: UserId(uid) }),
        }
    }

    #[test]
    fn append_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.bin");
        {
            let mut w = WalWriter::open(&path).unwrap();
            w.append(&env(1, 100)).unwrap();
            w.append(&env(2, 200)).unwrap();
            w.append(&env(3, 300)).unwrap();
            w.sync().unwrap();
            assert_eq!(w.last_synced_seq(), SeqNo(3));
        }
        let all = read_all(&path).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].seq_no, SeqNo(1));
        assert_eq!(all[2].seq_no, SeqNo(3));
    }

    #[test]
    fn empty_file_yields_no_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.bin");
        {
            let _ = WalWriter::open(&path).unwrap();
        }
        let all = read_all(&path).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn missing_file_yields_no_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.bin");
        let all = read_all(&path).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn torn_tail_treated_as_eof() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.bin");
        {
            let mut w = WalWriter::open(&path).unwrap();
            w.append(&env(1, 100)).unwrap();
            w.append(&env(2, 200)).unwrap();
            w.append(&env(3, 300)).unwrap();
            w.sync().unwrap();
        }
        // Lop a byte off the final record's payload.
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        let full_len = f.metadata().unwrap().len();
        f.set_len(full_len - 1).unwrap();
        drop(f);

        let all = read_all(&path).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].seq_no, SeqNo(2));
    }

    #[test]
    fn corrupt_inner_record_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.bin");
        {
            let mut w = WalWriter::open(&path).unwrap();
            w.append(&env(1, 100)).unwrap();
            w.sync().unwrap();
        }
        // Corrupt the body by zeroing it (length header survives, payload garbage).
        let mut f = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let total = f.metadata().unwrap().len();
        f.seek(SeekFrom::Start(4)).unwrap();
        let zeros = vec![0xFFu8; (total - 4) as usize];
        f.write_all(&zeros).unwrap();
        drop(f);

        let result = read_all(&path);
        assert!(matches!(result, Err(WalError::Corrupt { .. })));
    }

    #[test]
    fn reopen_appends_after_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.bin");
        {
            let mut w = WalWriter::open(&path).unwrap();
            w.append(&env(1, 1)).unwrap();
            w.sync().unwrap();
        }
        {
            let mut w = WalWriter::open(&path).unwrap();
            w.append(&env(2, 2)).unwrap();
            w.sync().unwrap();
        }
        let all = read_all(&path).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].seq_no, SeqNo(2));
    }
}
