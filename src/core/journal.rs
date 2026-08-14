use crate::api::OrderCommand;
use anyhow::Result;
use rkyv::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 记录头魔数，用于识别记录边界并在遇到垃圾数据时立即停下
const RECORD_MAGIC: u32 = 0x4D43_5741; // "MCWA"

/// 记录头：magic(4) + len(4) + crc(4)
const HEADER_LEN: usize = 12;

/// 单条记录 payload 上限。防止损坏的长度字段导致按天文数字分配内存。
const MAX_RECORD_LEN: usize = 1 << 20; // 1 MiB

/// CRC-32 (IEEE) 查表，编译期生成
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    !crc
}

/// 落盘策略。决定"写入返回"与"数据真正安全"之间的距离。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// 每条命令都 fsync。最安全，也最慢。
    EveryCommand,
    /// 每积攒 N 条 fsync 一次（组提交）。吞吐与持久性的折中，
    /// 崩溃最多丢失最后 N-1 条。
    EveryN(usize),
    /// 只写进 OS 页缓存，不主动 fsync。进程崩溃不丢，**掉电会丢**。
    Never,
}

/// 重放结果
#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub commands: Vec<OrderCommand>,
    /// 该字节偏移之后的数据无法解析（尾部截断或损坏）。
    /// `None` 表示整个文件都是完整记录。
    pub truncated_at: Option<u64>,
}

/// 预写日志 (WAL)。
///
/// 记录格式：`magic(u32) | len(u32) | crc32(u32) | payload(rkyv, len 字节)`
/// magic 用于定位记录边界，CRC 用于识别写了一半或被损坏的记录——
/// 二者缺一，掉电产生的半条记录就会让整个重放失败。
pub struct Journaler {
    writer: BufWriter<File>,
    policy: SyncPolicy,
    since_sync: usize,
}

impl Journaler {
    /// 以最安全的策略（每条 fsync）打开日志
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::with_sync_policy(path, SyncPolicy::EveryCommand)
    }

    pub fn with_sync_policy<P: AsRef<Path>>(path: P, policy: SyncPolicy) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: BufWriter::with_capacity(64 * 1024, file),
            policy,
            since_sync: 0,
        })
    }

    /// 追加一条命令
    pub fn write_command(&mut self, cmd: &OrderCommand) -> Result<()> {
        let bytes = rkyv::to_bytes::<_, 256>(cmd)
            .map_err(|e| anyhow::anyhow!("rkyv 序列化失败: {}", e))?;

        if bytes.len() > MAX_RECORD_LEN {
            anyhow::bail!("单条记录 {} 字节，超过上限 {}", bytes.len(), MAX_RECORD_LEN);
        }

        self.writer.write_all(&RECORD_MAGIC.to_le_bytes())?;
        self.writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.writer.write_all(&crc32(&bytes).to_le_bytes())?;
        self.writer.write_all(&bytes)?;

        self.maybe_sync()
    }

    fn maybe_sync(&mut self) -> Result<()> {
        self.since_sync += 1;
        let should_sync = match self.policy {
            SyncPolicy::EveryCommand => true,
            SyncPolicy::EveryN(n) => self.since_sync >= n.max(1),
            SyncPolicy::Never => false,
        };

        if should_sync {
            // flush 只是把 BufWriter 交给 OS；真正落盘必须 sync_data。
            self.writer.flush()?;
            self.writer.get_ref().sync_data()?;
            self.since_sync = 0;
        }
        Ok(())
    }

    /// 强制落盘（优雅关闭 / 快照点前调用）
    pub fn sync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        self.since_sync = 0;
        Ok(())
    }

    /// 读取并重放日志。遇到写了一半或损坏的记录时，**在该处停下并返回此前的全部命令**，
    /// 而不是让整个重放失败——掉电正是 WAL 存在的理由，不能反过来被它击垮。
    pub fn read_commands<P: AsRef<Path>>(path: P) -> Result<ReplayOutcome> {
        if !path.as_ref().exists() {
            return Ok(ReplayOutcome {
                commands: Vec::new(),
                truncated_at: None,
            });
        }

        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut commands = Vec::new();
        let mut offset: u64 = 0;

        loop {
            let mut header = [0u8; HEADER_LEN];
            match read_full(&mut reader, &mut header)? {
                // 干净的文件末尾
                0 => {
                    return Ok(ReplayOutcome {
                        commands,
                        truncated_at: None,
                    });
                }
                // 头都没读全 => 尾部被截断
                n if n < HEADER_LEN => {
                    return Ok(ReplayOutcome {
                        commands,
                        truncated_at: Some(offset),
                    });
                }
                _ => {}
            }

            let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
            let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let expected_crc = u32::from_le_bytes(header[8..12].try_into().unwrap());

            if magic != RECORD_MAGIC || len > MAX_RECORD_LEN {
                return Ok(ReplayOutcome {
                    commands,
                    truncated_at: Some(offset),
                });
            }

            // rkyv 的 check_archived_root 要求缓冲区对齐，Vec<u8> 的对齐是 1，
            // 这里必须用 AlignedVec。
            let mut data = rkyv::AlignedVec::with_capacity(len);
            data.resize(len, 0);
            if read_full(&mut reader, &mut data)? < len {
                return Ok(ReplayOutcome {
                    commands,
                    truncated_at: Some(offset),
                });
            }

            if crc32(&data) != expected_crc {
                return Ok(ReplayOutcome {
                    commands,
                    truncated_at: Some(offset),
                });
            }

            let Ok(archived) = rkyv::check_archived_root::<OrderCommand>(&data) else {
                return Ok(ReplayOutcome {
                    commands,
                    truncated_at: Some(offset),
                });
            };
            // 已通过 check_archived_root 校验，反序列化的错误类型是 Infallible
            let cmd: OrderCommand = archived
                .deserialize(&mut rkyv::Infallible)
                .expect("Infallible 不可能失败");

            commands.push(cmd);
            offset += (HEADER_LEN + len) as u64;
        }
    }

    /// 把日志裁剪到最后一条完整记录处，返回裁掉的字节数。
    ///
    /// 重放发现尾部损坏后必须先做这件事再继续追加，否则新记录会接在垃圾数据后面，
    /// 下次重放依然会在同一个位置停住。
    pub fn truncate_to_last_valid<P: AsRef<Path>>(path: P) -> Result<u64> {
        let outcome = Self::read_commands(&path)?;
        let Some(valid_end) = outcome.truncated_at else {
            return Ok(0);
        };

        let mut file = OpenOptions::new().write(true).open(&path)?;
        let total = file.seek(SeekFrom::End(0))?;
        file.set_len(valid_end)?;
        file.sync_all()?;
        Ok(total - valid_end)
    }
}

/// 尽力读满 buf，返回实际读到的字节数（0 表示恰好在边界处到达 EOF）
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}
