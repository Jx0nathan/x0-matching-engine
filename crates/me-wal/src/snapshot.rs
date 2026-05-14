use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Serialize};

use me_types::SeqNo;

use crate::journal::WalResult;

/// Directory-backed snapshot store. Files are named `snapshot_{seq}.bin`
/// where `seq` is the engine's `last_applied_seq` at write time.
#[derive(Debug)]
pub struct SnapshotStore {
    dir: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct SnapshotEntry {
    pub seq_no: SeqNo,
}

impl SnapshotStore {
    pub fn open<P: AsRef<Path>>(dir: P) -> WalResult<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save<S: Serialize>(&self, state: &S, seq_no: SeqNo) -> WalResult<PathBuf> {
        let path = self.dir.join(format!("snapshot_{}.bin", seq_no.0));
        let file = File::create(&path)?;
        let mut w = BufWriter::with_capacity(64 * 1024, file);
        bincode::serialize_into(&mut w, state)?;
        use std::io::Write;
        w.flush()?;
        w.get_ref().sync_data()?;
        Ok(path)
    }

    pub fn load<S: DeserializeOwned>(&self, seq_no: SeqNo) -> WalResult<S> {
        let path = self.dir.join(format!("snapshot_{}.bin", seq_no.0));
        let file = File::open(&path)?;
        let reader = BufReader::with_capacity(64 * 1024, file);
        let state: S = bincode::deserialize_from(reader)?;
        Ok(state)
    }

    pub fn list(&self) -> WalResult<Vec<SnapshotEntry>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(rest) = name.strip_prefix("snapshot_") else {
                continue;
            };
            let Some(num) = rest.strip_suffix(".bin") else {
                continue;
            };
            let Ok(seq) = num.parse::<u64>() else {
                continue;
            };
            out.push(SnapshotEntry { seq_no: SeqNo(seq) });
        }
        out.sort_by_key(|e| e.seq_no.0);
        Ok(out)
    }

    pub fn latest(&self) -> WalResult<Option<SnapshotEntry>> {
        Ok(self.list()?.into_iter().next_back())
    }

    pub fn load_latest<S: DeserializeOwned>(&self) -> WalResult<Option<(S, SeqNo)>> {
        let Some(entry) = self.latest()? else {
            return Ok(None);
        };
        let state = self.load::<S>(entry.seq_no)?;
        Ok(Some((state, entry.seq_no)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use tempfile::tempdir;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct DummyState {
        x: i64,
        y: String,
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::open(dir.path()).unwrap();
        let s = DummyState {
            x: 42,
            y: "hello".into(),
        };
        store.save(&s, SeqNo(7)).unwrap();
        let loaded: DummyState = store.load(SeqNo(7)).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn list_sorted_ascending() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::open(dir.path()).unwrap();
        for seq in [3u64, 10, 1, 5] {
            store
                .save(
                    &DummyState {
                        x: seq as i64,
                        y: "".into(),
                    },
                    SeqNo(seq),
                )
                .unwrap();
        }
        let list = store.list().unwrap();
        assert_eq!(
            list.iter().map(|e| e.seq_no.0).collect::<Vec<_>>(),
            [1, 3, 5, 10]
        );
    }

    #[test]
    fn latest_returns_max_seq() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::open(dir.path()).unwrap();
        store
            .save(&DummyState { x: 1, y: "".into() }, SeqNo(1))
            .unwrap();
        store
            .save(
                &DummyState {
                    x: 99,
                    y: "".into(),
                },
                SeqNo(99),
            )
            .unwrap();
        store
            .save(&DummyState { x: 5, y: "".into() }, SeqNo(5))
            .unwrap();
        let latest = store.latest().unwrap().unwrap();
        assert_eq!(latest.seq_no, SeqNo(99));
    }

    #[test]
    fn load_latest_returns_state_and_seq() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::open(dir.path()).unwrap();
        store
            .save(
                &DummyState {
                    x: 1,
                    y: "a".into(),
                },
                SeqNo(1),
            )
            .unwrap();
        store
            .save(
                &DummyState {
                    x: 2,
                    y: "b".into(),
                },
                SeqNo(2),
            )
            .unwrap();
        let (state, seq): (DummyState, SeqNo) = store.load_latest().unwrap().unwrap();
        assert_eq!(seq, SeqNo(2));
        assert_eq!(state.y, "b");
    }

    #[test]
    fn empty_store_returns_none() {
        let dir = tempdir().unwrap();
        let store = SnapshotStore::open(dir.path()).unwrap();
        assert!(store.latest().unwrap().is_none());
        let r: Option<(DummyState, SeqNo)> = store.load_latest().unwrap();
        assert!(r.is_none());
    }
}
