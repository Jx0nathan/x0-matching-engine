use crate::core::exchange::ExchangeState;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};

/// 快照管理器（使用 bincode，兼容性好）
pub struct SnapshotStore {
    base_path: PathBuf,
}

impl SnapshotStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let base_path = path.as_ref().to_path_buf();
        if !base_path.exists() {
            fs::create_dir_all(&base_path).context("无法创建快照目录")?;
        }
        Ok(Self { base_path })
    }

    /// 保存核心状态到快照文件。
    ///
    /// 走 临时文件 -> fsync -> rename -> fsync 父目录：
    /// 快照是 WAL 前缀压缩的唯一依据，一旦它只停在页缓存里（或写了一半），
    /// 压缩掉的那段日志就再也补不回来了。原子替换保证任何时刻崩溃，
    /// 看到的要么是上一个完整快照，要么是这一个完整快照。
    pub fn save_snapshot(&self, state: &ExchangeState, seq_id: u64) -> Result<PathBuf> {
        let filename = format!("snapshot_{}.bin", seq_id);
        let path = self.base_path.join(&filename);
        let tmp_path = self.base_path.join(format!("{}.tmp", filename));

        {
            let file = File::create(&tmp_path).context("无法创建快照临时文件")?;
            let mut writer = BufWriter::new(file);
            bincode::serialize_into(&mut writer, state).context("序列化快照失败")?;
            writer.flush().context("快照 flush 失败")?;
            writer.get_ref().sync_all().context("快照 fsync 失败")?;
        }

        fs::rename(&tmp_path, &path).context("快照原子替换失败")?;
        if let Ok(dir) = File::open(&self.base_path) {
            let _ = dir.sync_all();
        }

        Ok(path)
    }

    /// 加载指定索引的快照
    pub fn load_snapshot(&self, seq_id: u64) -> Result<ExchangeState> {
        let filename = format!("snapshot_{}.bin", seq_id);
        let path = self.base_path.join(filename);
        
        let file = File::open(&path).context("无法打开快照文件")?;
        let reader = BufReader::new(file);
        
        let state: ExchangeState = bincode::deserialize_from(reader).context("反序列化快照失败")?;
        
        Ok(state)
    }

    /// 获取最新的快照索引
    pub fn get_latest_seq_id(&self) -> Result<Option<u64>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.base_path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("snapshot_") && name.ends_with(".bin") {
                if let Ok(id) = name["snapshot_".len()..name.len() - 4].parse::<u64>() {
                    ids.push(id);
                }
            }
        }
        
        ids.sort_unstable();
        Ok(ids.last().copied())
    }
}
