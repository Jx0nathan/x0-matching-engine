use crate::api::OrderCommand;
use anyhow::Result;
use rkyv::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// 记录头魔数，用于识别记录边界并在遇到垃圾数据时立即停下。
///
/// 注意：v2 起记录头新增 seq 字段，布局与 v1 不兼容，因此魔数一并更换。
/// 旧日志会在偏移 0 处被判为不可解析（`truncated_at: Some(0)`、重放 0 条），
/// 不会被误读成垃圾命令。
const RECORD_MAGIC: u32 = 0x3257_434D; // "MCW2"

/// 记录头：magic(4) + seq(8) + len(4) + crc(4)
const HEADER_LEN: usize = 20;

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
    /// 日志中最后一条完整记录的序号；空日志为 None。
    /// 注意这是**日志里**的最大序号，与本次实际返回了多少条命令无关
    /// （`read_commands_after` 会过滤掉前缀）。
    pub last_seq: Option<u64>,
    /// 该字节偏移之后的数据无法解析（尾部截断或损坏）。
    /// `None` 表示整个文件都是完整记录。
    pub truncated_at: Option<u64>,
}

/// 扫描日志结构得到的结果（只读记录头，不解码 payload）
#[derive(Debug, Clone, Copy)]
struct ScanEnd {
    /// 最后一条完整记录的结束偏移，也即可安全追加的位置
    valid_end: u64,
    /// 最后一条完整记录的序号
    last_seq: Option<u64>,
    /// 从该偏移起不可解析
    truncated_at: Option<u64>,
}

/// 解析记录头，返回 (seq, payload 长度, crc)
fn parse_header(header: &[u8; HEADER_LEN]) -> Option<(u64, usize, u32)> {
    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != RECORD_MAGIC {
        return None;
    }
    let seq = u64::from_le_bytes(header[4..12].try_into().unwrap());
    let len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if len > MAX_RECORD_LEN {
        return None;
    }
    let crc = u32::from_le_bytes(header[16..20].try_into().unwrap());
    Some((seq, len, crc))
}

/// 预写日志 (WAL)。
///
/// 记录格式：`magic(u32) | seq(u64) | len(u32) | crc32(u32) | payload(rkyv, len 字节)`
///
/// - magic 用于定位记录边界，CRC 用于识别写了一半或被损坏的记录——
///   二者缺一，掉电产生的半条记录就会让整个重放失败。
/// - seq 是贯穿命令流的全局单调序号，让"快照对应到日志的哪个位置"这件事可表达，
///   从而支持增量重放（只放快照之后的命令）与前缀压缩（丢弃快照之前的日志）。
pub struct Journaler {
    path: PathBuf,
    writer: BufWriter<File>,
    policy: SyncPolicy,
    since_sync: usize,
    /// 下一条记录将要使用的序号。打开已有日志时从文件尾部续上。
    next_seq: u64,
}

impl Journaler {
    /// 以最安全的策略（每条 fsync）打开日志
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::with_sync_policy(path, SyncPolicy::EveryCommand)
    }

    pub fn with_sync_policy<P: AsRef<Path>>(path: P, policy: SyncPolicy) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // 追加打开前先扫一遍，把序号续上。否则重启后序号从头开始，
        // 快照与日志的对应关系立刻失效。
        let scan = Self::scan_headers(&path, |_, _, _| {})?;
        let next_seq = scan.last_seq.map_or(1, |s| s + 1);

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            writer: BufWriter::with_capacity(64 * 1024, file),
            policy,
            since_sync: 0,
            next_seq,
        })
    }

    /// 已写入的最后一条记录的序号（尚未写过任何记录时为 0）
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// 追加一条命令，返回分配给它的全局序号
    pub fn write_command(&mut self, cmd: &OrderCommand) -> Result<u64> {
        let bytes = rkyv::to_bytes::<_, 256>(cmd)
            .map_err(|e| anyhow::anyhow!("rkyv 序列化失败: {}", e))?;

        if bytes.len() > MAX_RECORD_LEN {
            anyhow::bail!("单条记录 {} 字节，超过上限 {}", bytes.len(), MAX_RECORD_LEN);
        }

        let seq = self.next_seq;
        self.writer.write_all(&RECORD_MAGIC.to_le_bytes())?;
        self.writer.write_all(&seq.to_le_bytes())?;
        self.writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.writer.write_all(&crc32(&bytes).to_le_bytes())?;
        self.writer.write_all(&bytes)?;
        self.next_seq += 1;

        self.maybe_sync()?;
        Ok(seq)
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

    /// 丢弃 `keep_after_seq` 及之前的所有记录，返回回收的字节数。
    ///
    /// 快照做完之后调用：那些命令的效果已经固化在快照里，再留着只会让日志无限膨胀、
    /// 让恢复时间线性增长。
    ///
    /// 由于序号随追加单调递增，"要保留的记录"必然是文件的一个**连续后缀**，
    /// 因此只需定位起始偏移再整段拷贝，无需解码任何 payload。
    /// 尾部若有损坏，顺带一并裁掉。
    ///
    /// 实现上走 临时文件 → fsync → rename → fsync 父目录，保证压缩本身是崩溃安全的：
    /// 任何时刻崩溃，看到的要么是压缩前的完整日志，要么是压缩后的完整日志。
    pub fn compact_before(&mut self, keep_after_seq: u64) -> Result<u64> {
        // 缓冲区里的内容必须先进文件，否则会被 rename 覆盖掉
        self.sync()?;

        let mut start: Option<u64> = None;
        let scan = Self::scan_headers(&self.path, |seq, offset, _len| {
            if start.is_none() && seq > keep_after_seq {
                start = Some(offset);
            }
        })?;

        // 没有任何记录需要保留时，起点就是有效区末尾（结果是清空日志）
        let start = start.unwrap_or(scan.valid_end);
        let old_len = std::fs::metadata(&self.path)?.len();
        if start == 0 && scan.truncated_at.is_none() {
            return Ok(0); // 无需压缩
        }

        let tmp_path = self.path.with_extension("compact.tmp");
        {
            let mut src = File::open(&self.path)?;
            src.seek(SeekFrom::Start(start))?;
            let mut src = src.take(scan.valid_end.saturating_sub(start));

            let tmp = File::create(&tmp_path)?;
            let mut dst = BufWriter::with_capacity(64 * 1024, tmp);
            std::io::copy(&mut src, &mut dst)?;
            dst.flush()?;
            dst.get_ref().sync_all()?;
        }

        std::fs::rename(&tmp_path, &self.path)?;
        Self::sync_parent_dir(&self.path);

        // 旧句柄指向的已是被 rename 顶掉的 inode，必须重开
        let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.writer = BufWriter::with_capacity(64 * 1024, file);
        self.since_sync = 0;

        let new_len = std::fs::metadata(&self.path)?.len();
        Ok(old_len.saturating_sub(new_len))
    }

    /// rename 之后必须 fsync 父目录，否则目录项本身可能还没落盘
    fn sync_parent_dir(path: &Path) {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }

    /// 只读记录头、跳过 payload 地扫描整个日志，对每条完整记录回调
    /// `(seq, 记录起始偏移, 记录总长)`。
    fn scan_headers<P: AsRef<Path>, F: FnMut(u64, u64, u64)>(
        path: P,
        mut on_record: F,
    ) -> Result<ScanEnd> {
        if !path.as_ref().exists() {
            return Ok(ScanEnd {
                valid_end: 0,
                last_seq: None,
                truncated_at: None,
            });
        }

        let file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut offset: u64 = 0;
        let mut last_seq = None;

        macro_rules! stop {
            ($at:expr) => {
                return Ok(ScanEnd {
                    valid_end: offset,
                    last_seq,
                    truncated_at: $at,
                })
            };
        }

        loop {
            let mut header = [0u8; HEADER_LEN];
            match read_full(&mut reader, &mut header)? {
                0 => stop!(None),
                n if n < HEADER_LEN => stop!(Some(offset)),
                _ => {}
            }

            let Some((seq, len, _crc)) = parse_header(&header) else {
                stop!(Some(offset))
            };

            // seek 越过文件尾并不报错，所以先用文件长度判定 payload 是否真的存在
            let end = offset + (HEADER_LEN + len) as u64;
            if end > file_len {
                stop!(Some(offset))
            }

            // 跳过 payload。seek_relative 在目标仍落在缓冲区内时不会丢弃缓冲，
            // 顺序扫描因此不会退化成每条一次 syscall。
            reader.seek_relative(len as i64)?;

            on_record(seq, offset, (HEADER_LEN + len) as u64);
            last_seq = Some(seq);
            offset = end;
        }
    }

    /// 读取并重放日志的全部命令
    pub fn read_commands<P: AsRef<Path>>(path: P) -> Result<ReplayOutcome> {
        Self::read_commands_after(path, 0)
    }

    /// 只读取序号大于 `after_seq` 的命令。
    ///
    /// 从快照恢复后应当用这个：快照已经包含了 `after_seq` 及之前所有命令的效果，
    /// 再放一遍就是重复下单、重复扣款。
    ///
    /// 遇到写了一半或损坏的记录时，**在该处停下并返回此前的全部命令**，
    /// 而不是让整个重放失败——掉电正是 WAL 存在的理由，不能反过来被它击垮。
    pub fn read_commands_after<P: AsRef<Path>>(path: P, after_seq: u64) -> Result<ReplayOutcome> {
        if !path.as_ref().exists() {
            return Ok(ReplayOutcome {
                commands: Vec::new(),
                last_seq: None,
                truncated_at: None,
            });
        }

        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut commands = Vec::new();
        let mut offset: u64 = 0;
        let mut last_seq = None;

        macro_rules! stop {
            ($at:expr) => {
                return Ok(ReplayOutcome {
                    commands,
                    last_seq,
                    truncated_at: $at,
                })
            };
        }

        loop {
            let mut header = [0u8; HEADER_LEN];
            match read_full(&mut reader, &mut header)? {
                0 => stop!(None),                       // 干净的文件末尾
                n if n < HEADER_LEN => stop!(Some(offset)), // 头都没读全 => 尾部被截断
                _ => {}
            }

            let Some((seq, len, expected_crc)) = parse_header(&header) else {
                stop!(Some(offset))
            };

            // rkyv 的 check_archived_root 要求缓冲区对齐，Vec<u8> 的对齐是 1，
            // 这里必须用 AlignedVec。
            let mut data = rkyv::AlignedVec::with_capacity(len);
            data.resize(len, 0);
            if read_full(&mut reader, &mut data)? < len {
                stop!(Some(offset))
            }

            if crc32(&data) != expected_crc {
                stop!(Some(offset))
            }

            let Ok(archived) = rkyv::check_archived_root::<OrderCommand>(&data) else {
                stop!(Some(offset))
            };

            if seq > after_seq {
                // 已通过 check_archived_root 校验，反序列化的错误类型是 Infallible
                let mut cmd: OrderCommand = archived
                    .deserialize(&mut rkyv::Infallible)
                    .expect("Infallible 不可能失败");
                // seq 被 rkyv 的 Skip 排除在 payload 之外，权威来源是记录头，这里回填
                cmd.seq = seq;
                commands.push(cmd);
            }

            last_seq = Some(seq);
            offset += (HEADER_LEN + len) as u64;
        }
    }

    /// 把日志裁剪到最后一条完整记录处，返回裁掉的字节数。
    ///
    /// 重放发现尾部损坏后必须先做这件事再继续追加，否则新记录会接在垃圾数据后面，
    /// 下次重放依然会在同一个位置停住。
    pub fn truncate_to_last_valid<P: AsRef<Path>>(path: P) -> Result<u64> {
        let scan = Self::scan_headers(&path, |_, _, _| {})?;
        if scan.truncated_at.is_none() {
            return Ok(0);
        }

        let mut file = OpenOptions::new().write(true).open(&path)?;
        let total = file.seek(SeekFrom::End(0))?;
        file.set_len(scan.valid_end)?;
        file.sync_all()?;
        Self::sync_parent_dir(path.as_ref());
        Ok(total - scan.valid_end)
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
