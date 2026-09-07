//! 生命周期约束与 WAL 持久性测试。
//!
//! 覆盖两类此前会静默出错的场景：
//! 1. `startup()` 之后调用只在同步模式下有效的 API —— 过去无声失败，现在必须报错
//! 2. WAL 尾部被掉电截断或数据损坏 —— 过去整个重放失败，现在必须能恢复到最后一条完整记录

use matching_core::api::*;
use matching_core::core::exchange::{ExchangeConfig, ExchangeCore};
use matching_core::core::journal::{Journaler, SyncPolicy};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn spec(symbol_id: SymbolId) -> CoreSymbolSpecification {
    CoreSymbolSpecification {
        stp: SelfTradePrevention::None,
        symbol_id,
        symbol_type: SymbolType::CurrencyExchangePair,
        base_currency: 0,
        quote_currency: 1,
        base_scale_k: 1,
        quote_scale_k: 1,
        taker_fee: 0,
        maker_fee: 0,
        margin_buy: 0,
        margin_sell: 0,
    }
}

fn new_core() -> ExchangeCore {
    let mut c = ExchangeCore::new(ExchangeConfig::default());
    c.add_symbol(spec(1)).expect("注册交易对失败");
    c
}

fn add_user(core: &mut ExchangeCore, uid: UserId) -> CommandResultCode {
    core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid,
        ..Default::default()
    })
    .result_code
}

/// 每个测试用独立目录，避免并行互相干扰
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("matching_core_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------- 生命周期约束

/// startup() 之后注册交易对必须报错，而不是静默丢弃
#[test]
fn add_symbol_after_startup_errors() {
    let mut core = new_core();
    core.startup();

    let err = core.add_symbol(spec(2)).unwrap_err();
    assert!(
        err.to_string().contains("startup"),
        "错误信息应指明必须在 startup 之前调用，实际：{err}"
    );
}

/// startup() 之后注册结果回调必须报错（过去回调会静默失效，一次都不触发）
#[test]
fn set_result_consumer_after_startup_errors() {
    let mut core = new_core();
    core.startup();

    let err = core
        .set_result_consumer(Arc::new(|_cmd: &OrderCommand| {}))
        .unwrap_err();
    assert!(err.to_string().contains("startup"));
}

/// startup() 之后做快照必须报错，而不是 panic
#[test]
fn snapshot_after_startup_errors() {
    let dir = temp_dir("snap_after_startup");
    let mut core = new_core();
    core.enable_snapshotting(&dir).unwrap();
    core.startup();

    assert!(core.take_snapshot().is_err(), "启动后应拒绝快照");
    assert!(core.serialize_state().is_err(), "启动后应拒绝序列化状态");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 异步模式下 submit_command 回报 Accepted，真实结果通过回调送达
#[test]
fn async_mode_reports_accepted_and_invokes_callback() {
    let mut core = new_core();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    core.set_result_consumer(Arc::new(move |_cmd: &OrderCommand| {
        hits2.fetch_add(1, Ordering::SeqCst);
    }))
    .expect("启动前注册回调应成功");
    core.startup();

    let returned = core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid: 1,
        ..Default::default()
    });

    assert_eq!(
        returned.result_code,
        CommandResultCode::Accepted,
        "异步提交应回报 Accepted，而不是让调用方以为命令没被处理"
    );

    // 等消费者线程处理完
    for _ in 0..100 {
        if hits.load(Ordering::SeqCst) > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "结果回调未触发");
}

/// 同步模式下这些 API 应当正常工作
#[test]
fn sync_mode_apis_work() {
    let mut core = new_core();
    assert!(core.set_result_consumer(Arc::new(|_| {})).is_ok());
    assert!(core.add_symbol(spec(3)).is_ok());
    assert!(core.serialize_state().is_ok());
    assert!(!core.is_started());
}

// ---------------------------------------------------------------- 重复 order_id

/// 重复 order_id 必须直接拒单：不撮合、不入簿、资金退还、回报专用错误码
#[test]
fn duplicate_order_id_rejected_and_refunded() {
    let mut core = new_core();
    add_user(&mut core, 1);
    core.submit_command(OrderCommand {
        command: OrderCommandType::BalanceAdjustment,
        uid: 1,
        symbol: 1,
        price: 10_000,
        order_id: 1,
        ..Default::default()
    });

    fn place(core: &mut ExchangeCore, order_id: u64) -> CommandResultCode {
        core.submit_command(OrderCommand {
            command: OrderCommandType::PlaceOrder,
            uid: 1,
            order_id,
            symbol: 1,
            price: 100,
            reserve_price: 100,
            size: 10,
            action: OrderAction::Bid,
            order_type: OrderType::Gtc,
            ..Default::default()
        })
        .result_code
    }

    assert_eq!(place(&mut core, 7), CommandResultCode::Success);
    let after_first = core.balance_of(1, 1).unwrap();

    assert_eq!(
        place(&mut core, 7),
        CommandResultCode::MatchingDuplicateOrderId,
        "重复 order_id 应被明确拒绝，而不是回报成功"
    );
    assert_eq!(
        core.balance_of(1, 1).unwrap(),
        after_first,
        "重复下单被拒后资金应原样退还"
    );
}

// ---------------------------------------------------------------- WAL

/// 记录头布局：magic(4) + seq(8) + len(4) + crc(4)。与 journal.rs 的 HEADER_LEN 同步。
const HEADER_LEN: usize = 20;
/// len 字段在记录头中的位置
const LEN_FIELD: std::ops::Range<usize> = 12..16;

fn write_journal(path: &std::path::Path, count: u64) {
    let mut core = new_core();
    core.enable_journaling(path).expect("启用 WAL 失败");
    for uid in 1..=count {
        core.submit_command(OrderCommand {
            command: OrderCommandType::AddUser,
            uid,
            ..Default::default()
        });
    }
    core.sync_journal().unwrap();
}

/// 正常往返：写多少条就应重放出多少条
#[test]
fn wal_roundtrip() {
    let dir = temp_dir("wal_roundtrip");
    let path = dir.join("journal.bin");
    write_journal(&path, 5);

    let mut core = new_core();
    let summary = core.replay_journal(&path).expect("重放失败");
    assert_eq!(summary.replayed, 5);
    assert_eq!(summary.truncated_at, None, "完整日志不应报告截断");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 掉电场景：尾部半条记录，必须重放出此前的全部命令而不是整体失败
#[test]
fn wal_truncated_tail_replays_prefix() {
    let dir = temp_dir("wal_truncated");
    let path = dir.join("journal.bin");
    write_journal(&path, 5);

    // 砍掉最后 3 个字节，模拟掉电时写了一半的记录
    let full = std::fs::read(&path).unwrap();
    std::fs::write(&path, &full[..full.len() - 3]).unwrap();

    let mut core = new_core();
    let summary = core
        .replay_journal(&path)
        .expect("尾部截断不应导致整个重放失败");

    assert_eq!(summary.replayed, 4, "应重放出前 4 条完整记录");
    assert!(summary.truncated_at.is_some(), "应报告尾部截断位置");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 记录内容被损坏：CRC 必须发现，并停在最后一条完整记录处
#[test]
fn wal_corrupted_payload_detected_by_crc() {
    let dir = temp_dir("wal_corrupted");
    let path = dir.join("journal.bin");
    write_journal(&path, 3);

    // 翻转第一条记录 payload 的首字节
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[HEADER_LEN] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let mut core = new_core();
    let summary = core.replay_journal(&path).expect("损坏不应导致 Err");
    assert_eq!(summary.replayed, 0, "首条记录损坏，不应重放任何命令");
    assert_eq!(summary.truncated_at, Some(0), "应指出损坏起点");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 损坏的长度字段不得导致按天文数字分配内存
#[test]
fn wal_absurd_length_is_rejected() {
    let dir = temp_dir("wal_absurd_len");
    let path = dir.join("journal.bin");
    write_journal(&path, 2);

    // 把第一条记录的 len 字段改成 4GB
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[LEN_FIELD].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let mut core = new_core();
    let summary = core.replay_journal(&path).expect("不应崩溃");
    assert_eq!(summary.replayed, 0);
    assert_eq!(summary.truncated_at, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 修复截断的日志后，应能继续追加并完整重放
#[test]
fn truncate_to_last_valid_allows_clean_append() {
    let dir = temp_dir("wal_repair");
    let path = dir.join("journal.bin");
    write_journal(&path, 5);

    let full = std::fs::read(&path).unwrap();
    std::fs::write(&path, &full[..full.len() - 3]).unwrap();

    // 裁掉坏尾巴
    let dropped = Journaler::truncate_to_last_valid(&path).expect("修复失败");
    assert!(dropped > 0, "应裁掉损坏的尾部字节");

    // 继续追加
    {
        let mut j = Journaler::with_sync_policy(&path, SyncPolicy::EveryCommand).unwrap();
        j.write_command(&OrderCommand {
            command: OrderCommandType::AddUser,
            uid: 99,
            ..Default::default()
        })
        .unwrap();
        j.sync().unwrap();
    }

    let mut core = new_core();
    let summary = core.replay_journal(&path).expect("重放失败");
    assert_eq!(summary.replayed, 5, "应为 4 条完整记录 + 1 条新追加");
    assert_eq!(summary.truncated_at, None, "修复后不应再有截断");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 重放必须在 startup() 之前进行，否则会绕过同步管线并把命令重新写回 WAL
#[test]
fn replay_after_startup_errors() {
    let dir = temp_dir("wal_replay_after_startup");
    let path = dir.join("journal.bin");
    write_journal(&path, 2);

    let mut core = new_core();
    core.startup();
    assert!(core.replay_journal(&path).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 重放后的状态应与原始执行一致
#[test]
fn replay_restores_balances() {
    let dir = temp_dir("wal_state");
    let path = dir.join("journal.bin");

    {
        let mut core = new_core();
        core.enable_journaling(&path).unwrap();
        add_user(&mut core, 1);
        core.submit_command(OrderCommand {
            command: OrderCommandType::BalanceAdjustment,
            uid: 1,
            symbol: 1,
            price: 5_000,
            order_id: 1,
            ..Default::default()
        });
        core.sync_journal().unwrap();
        assert_eq!(core.balance_of(1, 1).unwrap(), 5_000);
    }

    let mut restored = new_core();
    restored.replay_journal(&path).expect("重放失败");
    assert_eq!(
        restored.balance_of(1, 1).unwrap(),
        5_000,
        "重放后余额未恢复"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- 全局序号 · 检查点 · 前缀压缩

/// 给账户加钱的命令，金额直接决定余额，便于断言"有没有被重复执行"
fn credit(core: &mut ExchangeCore, uid: UserId, order_id: u64, amount: i64) {
    core.submit_command(OrderCommand {
        command: OrderCommandType::BalanceAdjustment,
        uid,
        symbol: 1,
        price: amount,
        order_id,
        ..Default::default()
    });
}

/// 序号必须单调递增，且重开日志时从文件尾部续上而不是从头开始
#[test]
fn journal_seq_is_monotonic_and_survives_reopen() {
    let dir = temp_dir("wal_seq_resume");
    let path = dir.join("journal.bin");

    {
        let mut core = new_core();
        core.enable_journaling(&path).unwrap();
        assert_eq!(core.last_seq(), 0, "尚未写入时序号应为 0");
        add_user(&mut core, 1);
        add_user(&mut core, 2);
        core.sync_journal().unwrap();
        assert_eq!(core.last_seq(), 2, "两条命令应得到序号 1、2");
    }

    // 重新打开同一个日志继续写：序号必须接着 2 往下，而不是重新从 1 开始
    {
        let mut core = new_core();
        core.enable_journaling(&path).unwrap();
        add_user(&mut core, 3);
        core.sync_journal().unwrap();
        assert_eq!(core.last_seq(), 3, "重开日志后序号未续上，快照与日志将无法对齐");
    }

    let mut core = new_core();
    let summary = core.replay_journal(&path).expect("重放失败");
    assert_eq!(summary.replayed, 3);
    assert_eq!(summary.last_seq, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 检查点之后再恢复：快照覆盖的那段命令绝不能被重放第二遍
#[test]
fn checkpoint_then_replay_does_not_double_apply() {
    let dir = temp_dir("wal_checkpoint");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    {
        let mut core = new_core();
        core.enable_journaling(&journal).unwrap();
        core.enable_snapshotting(&snaps).unwrap();

        add_user(&mut core, 1);
        credit(&mut core, 1, 1, 5_000); // 快照前
        assert_eq!(core.balance_of(1, 1).unwrap(), 5_000);

        let reclaimed = core.checkpoint().expect("检查点失败");
        assert!(reclaimed > 0, "快照已覆盖全部命令，日志前缀应被回收");

        credit(&mut core, 1, 2, 300); // 快照后
        core.sync_journal().unwrap();
        assert_eq!(core.balance_of(1, 1).unwrap(), 5_300);
    }

    // 恢复：快照给出 5000，日志只应补上快照之后的那 300
    let mut restored = new_core();
    restored.enable_snapshotting(&snaps).unwrap();
    assert!(restored.load_latest_snapshot().unwrap(), "应加载到快照");
    assert_eq!(restored.balance_of(1, 1).unwrap(), 5_000, "快照本身应含 5000");

    let summary = restored.replay_journal(&journal).expect("重放失败");
    assert_eq!(summary.replayed, 1, "只应重放快照之后的那 1 条命令");
    assert_eq!(
        restored.balance_of(1, 1).unwrap(),
        5_300,
        "余额被重复累加说明快照覆盖的命令又放了一遍"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 压缩只丢弃快照覆盖的前缀，后缀记录必须原样保留（序号也不变）
#[test]
fn compaction_keeps_suffix_intact() {
    let dir = temp_dir("wal_compact_suffix");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    core.enable_snapshotting(&snaps).unwrap();

    for uid in 1..=5 {
        add_user(&mut core, uid);
    }
    core.take_snapshot().expect("快照失败"); // 覆盖到 seq 5
    for uid in 6..=8 {
        add_user(&mut core, uid);
    }
    core.sync_journal().unwrap();

    let before = std::fs::metadata(&journal).unwrap().len();
    let reclaimed = core.compact_journal().expect("压缩失败");
    let after = std::fs::metadata(&journal).unwrap().len();
    assert_eq!(before - after, reclaimed, "回收字节数与文件缩减量不符");
    assert!(after > 0, "后缀记录不应被一起丢掉");

    // 压缩后的日志里应只剩 seq 6..=8，且序号原样保留
    let outcome = matching_core::core::journal::Journaler::read_commands(&journal).unwrap();
    assert_eq!(outcome.commands.len(), 3, "应只剩快照之后的 3 条");
    assert_eq!(outcome.last_seq, Some(8), "压缩不得重排序号");
    assert_eq!(outcome.truncated_at, None, "压缩后的日志应当是完整的");

    // 压缩后继续追加，序号仍要接着走
    add_user(&mut core, 9);
    core.sync_journal().unwrap();
    assert_eq!(core.last_seq(), 9);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 没有快照兜底时，压缩必须被拒绝 —— 否则那段命令再也回不来
#[test]
fn compaction_refused_without_snapshotting() {
    let dir = temp_dir("wal_compact_guard");
    let journal = dir.join("journal.bin");

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    add_user(&mut core, 1);
    core.sync_journal().unwrap();

    let err = core.compact_journal().unwrap_err();
    assert!(
        err.to_string().contains("未启用快照"),
        "未启用快照时压缩应报错，实际: {err}"
    );

    // 启用了快照但一次都没做过，也不能丢任何东西
    core.enable_snapshotting(dir.join("snapshots")).unwrap();
    assert_eq!(core.compact_journal().unwrap(), 0, "没有快照时不得回收任何字节");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------ 带内快照（异步态）

/// 异步态下必须能做快照 —— 这是 take_snapshot() 做不到的：
/// startup() 之后 pipeline 已移交撮合线程，ExchangeCore 读不到状态。
#[test]
fn in_band_snapshot_works_after_startup() {
    let dir = temp_dir("inband_snapshot");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    let done = Arc::new(AtomicUsize::new(0));
    let done2 = done.clone();
    let persist_ok = Arc::new(AtomicUsize::new(0));
    let persist_ok2 = persist_ok.clone();

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    core.enable_snapshotting(&snaps).unwrap();
    core.set_result_consumer(Arc::new(move |cmd: &OrderCommand| {
        if cmd.command == OrderCommandType::PersistStateMatching
            && cmd.result_code == CommandResultCode::Success
        {
            persist_ok2.fetch_add(1, Ordering::SeqCst);
        }
        done2.fetch_add(1, Ordering::SeqCst);
    }))
    .unwrap();

    core.startup();
    assert!(core.take_snapshot().is_err(), "启动后直连快照仍应被拒绝");

    core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid: 1,
        ..Default::default()
    });
    core.submit_command(OrderCommand {
        command: OrderCommandType::BalanceAdjustment,
        uid: 1,
        symbol: 1,
        price: 7_000,
        order_id: 1,
        ..Default::default()
    });
    let accepted = core.request_snapshot();
    assert_eq!(
        accepted.result_code,
        CommandResultCode::Accepted,
        "异步态下快照请求应先回 Accepted"
    );

    // 等消费者把三条命令都处理完
    for _ in 0..200 {
        if done.load(Ordering::SeqCst) >= 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(done.load(Ordering::SeqCst), 3, "消费者未处理完全部命令");
    assert_eq!(persist_ok.load(Ordering::SeqCst), 1, "带内快照未成功落盘");

    // 快照应以"它之前那条命令的序号"为档案号，即 2
    let store = matching_core::core::snapshot::SnapshotStore::new(&snaps).unwrap();
    assert_eq!(
        store.get_latest_seq_id().unwrap(),
        Some(2),
        "快照档案号应等于已覆盖的最后一条命令序号"
    );

    // 从这份快照恢复，状态必须完整
    let mut restored = new_core();
    restored.enable_snapshotting(&snaps).unwrap();
    assert!(restored.load_latest_snapshot().unwrap());
    assert_eq!(
        restored.balance_of(1, 1).unwrap(),
        7_000,
        "撮合线程写出的快照未包含完整状态"
    );
    assert_eq!(restored.last_seq(), 2);

    // 再补重放：日志里没有比快照更新的命令，应当一条都不放
    let summary = restored.replay_journal(&journal).expect("重放失败");
    assert_eq!(summary.replayed, 0, "快照已覆盖全部命令，不应再重放");
    assert_eq!(restored.balance_of(1, 1).unwrap(), 7_000);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 控制命令不进 WAL：否则每次重放都会凭空多做一次快照，而且会挤占序号
#[test]
fn control_commands_are_not_journaled() {
    let dir = temp_dir("inband_not_journaled");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    core.enable_snapshotting(&snaps).unwrap();

    add_user(&mut core, 1);
    core.request_snapshot(); // 同步态下就地执行
    add_user(&mut core, 2);
    core.sync_journal().unwrap();

    // 日志里只应有两条 AddUser，序号 1、2 连续，没有被控制命令挤占
    let outcome = matching_core::core::journal::Journaler::read_commands(&journal).unwrap();
    assert_eq!(outcome.commands.len(), 2, "控制命令不应出现在 WAL 里");
    assert!(
        outcome
            .commands
            .iter()
            .all(|c| c.command == OrderCommandType::AddUser),
        "WAL 中混入了控制命令"
    );
    assert_eq!(
        outcome.commands.iter().map(|c| c.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "重放时 seq 应从记录头回填，且不被控制命令挤占"
    );

    // 快照应停在请求时刻的序号 1，而不是 2
    let store = matching_core::core::snapshot::SnapshotStore::new(&snaps).unwrap();
    assert_eq!(store.get_latest_seq_id().unwrap(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 未启用快照时收到快照命令，必须明确报错而不是静默当作成功
#[test]
fn in_band_snapshot_without_store_reports_failure() {
    let mut core = new_core();
    let result = core.request_snapshot();
    assert_eq!(
        result.result_code,
        CommandResultCode::StatePersistMatchingEngineFailed,
        "没有快照存储时不得静默成功"
    );
}

/// 生产态的完整检查点循环：异步跑单 -> 带内快照 -> 压缩日志 -> 崩溃恢复。
/// 这是"日志无限增长"那个问题真正被解决的证明。
#[test]
fn async_checkpoint_then_compact_then_recover() {
    let dir = temp_dir("async_full_loop");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    let done = Arc::new(AtomicUsize::new(0));
    let done2 = done.clone();

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    core.enable_snapshotting(&snaps).unwrap();
    core.set_result_consumer(Arc::new(move |_: &OrderCommand| {
        done2.fetch_add(1, Ordering::SeqCst);
    }))
    .unwrap();
    core.startup();

    // 快照前：开户 + 充值 1000，共 2 条命令
    core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid: 1,
        ..Default::default()
    });
    core.submit_command(OrderCommand {
        command: OrderCommandType::BalanceAdjustment,
        uid: 1,
        symbol: 1,
        price: 1_000,
        order_id: 1,
        ..Default::default()
    });
    core.request_snapshot();

    for _ in 0..200 {
        if done.load(Ordering::SeqCst) >= 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(done.load(Ordering::SeqCst), 3, "快照命令未被消费");

    // 快照落盘后压缩：前两条命令的日志应被回收
    core.sync_journal().unwrap();
    let before = std::fs::metadata(&journal).unwrap().len();
    let reclaimed = core.compact_journal().expect("压缩失败");
    assert!(reclaimed > 0, "快照已覆盖前两条命令，日志前缀应被回收");
    assert!(std::fs::metadata(&journal).unwrap().len() < before);

    // 快照后再充值 250，这条只存在于压缩后的日志里
    core.submit_command(OrderCommand {
        command: OrderCommandType::BalanceAdjustment,
        uid: 1,
        symbol: 1,
        price: 250,
        order_id: 2,
        ..Default::default()
    });
    for _ in 0..200 {
        if done.load(Ordering::SeqCst) >= 4 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    core.sync_journal().unwrap();

    // 模拟崩溃重启：快照给 1000，压缩后的日志补上 250
    let mut restored = new_core();
    restored.enable_snapshotting(&snaps).unwrap();
    assert!(restored.load_latest_snapshot().unwrap());
    assert_eq!(restored.balance_of(1, 1).unwrap(), 1_000, "快照应含 1000");

    let summary = restored.replay_journal(&journal).expect("重放失败");
    assert_eq!(summary.replayed, 1, "压缩后日志里只应剩快照之后的那 1 条");
    assert_eq!(
        restored.balance_of(1, 1).unwrap(),
        1_250,
        "压缩 + 增量重放后状态不一致"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------ 撮合线程 panic 兜底

/// 撮合线程 panic 必须被兜住。
///
/// 未兜住时的实测行为：panic 逃出 handler 后在 disruptor 的析构路径上引发二次
/// panic，整个进程被 SIGABRT（"panic in a destructor during cleanup"）。最终
/// 打出来的是析构期二次 panic 的信息，**原始 panic 是哪条命令触发的完全看不出来**，
/// WAL 也没有机会收尾。
///
/// 这里用一个会 panic 的结果回调触发（下游回调有 bug 是真实场景），校验引擎转入
/// 中毒状态并对后续命令快速失败，而不是把进程带走。
/// 想复现未兜住时的行为：把 exchange.rs 里的 catch_unwind 去掉再跑本用例。
#[test]
fn panicking_consumer_poisons_engine_instead_of_aborting() {
    let seen = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();

    let mut core = new_core();
    core.set_result_consumer(Arc::new(move |_: &OrderCommand| {
        // 第一条正常放行，之后每条都炸
        if seen2.fetch_add(1, Ordering::SeqCst) >= 1 {
            panic!("下游回调故意 panic");
        }
    }))
    .unwrap();
    core.startup();

    assert!(!core.is_poisoned(), "初始不应中毒");

    let ok = core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid: 1,
        ..Default::default()
    });
    assert_eq!(ok.result_code, CommandResultCode::Accepted);

    // 这一条会让回调 panic
    core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid: 2,
        ..Default::default()
    });

    for _ in 0..300 {
        if core.is_poisoned() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(core.is_poisoned(), "撮合线程 panic 后引擎应转入中毒状态");

    // 关键：后续提交必须立刻返回，而不是自旋等一个死掉的消费者。
    // 环形缓冲默认 64Ki，多灌几条足以证明没有被阻塞。
    for uid in 3..200 {
        let r = core.submit_command(OrderCommand {
            command: OrderCommandType::AddUser,
            uid,
            ..Default::default()
        });
        assert_eq!(
            r.result_code,
            CommandResultCode::EnginePoisoned,
            "中毒后应快速失败，而不是继续受理"
        );
    }
}

/// 中毒后不得再写 WAL —— 那些命令根本没被执行，写进去会在重放时凭空多做一遍
#[test]
fn poisoned_engine_stops_journaling() {
    let dir = temp_dir("poisoned_no_journal");
    let journal = dir.join("journal.bin");

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    core.set_result_consumer(Arc::new(|cmd: &OrderCommand| {
        if cmd.uid == 2 {
            panic!("下游回调故意 panic");
        }
    }))
    .unwrap();
    core.startup();

    add_user(&mut core, 1);
    add_user(&mut core, 2); // 触发 panic

    for _ in 0..300 {
        if core.is_poisoned() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(core.is_poisoned());

    let before = std::fs::metadata(&journal).unwrap().len();
    for uid in 3..20 {
        add_user(&mut core, uid);
    }
    core.sync_journal().unwrap();
    let after = std::fs::metadata(&journal).unwrap().len();
    assert_eq!(before, after, "中毒后仍在写 WAL：这些命令并未执行，重放会多做一遍");

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ 优雅停机

/// 停机必须排空在途命令，不能把环形缓冲里没消费完的丢掉。
///
/// 这是异步态最容易出事的地方：submit_command 只负责投递就返回，进程直接退出的话，
/// 已受理但未消费的命令连同整个状态机一起没了。
#[test]
fn shutdown_drains_inflight_commands() {
    let handled = Arc::new(AtomicUsize::new(0));
    let handled2 = handled.clone();

    let mut core = new_core();
    // 刻意放慢消费者，保证 submit 循环跑完时缓冲里一定还有积压 ——
    // 否则消费者天然跟得上，这个用例就证明不了"排空"这件事。
    core.set_result_consumer(Arc::new(move |_: &OrderCommand| {
        std::thread::sleep(std::time::Duration::from_micros(50));
        handled2.fetch_add(1, Ordering::SeqCst);
    }))
    .unwrap();
    core.startup();

    const N: u64 = 500;
    for uid in 1..=N {
        core.submit_command(OrderCommand {
            command: OrderCommandType::AddUser,
            uid,
            ..Default::default()
        });
    }

    let before_shutdown = handled.load(Ordering::SeqCst) as u64;
    assert!(
        before_shutdown < N,
        "停机前消费者已全部处理完（{before_shutdown}/{N}），本用例无法验证排空；\
         请调慢消费者或加大 N"
    );

    // 不等待、直接停机：排空由 shutdown 负责
    core.shutdown().expect("停机失败");

    assert_eq!(
        handled.load(Ordering::SeqCst) as u64,
        N + 1,
        "在途命令被丢弃（停机前才处理了 {before_shutdown} 条；\
         应为 N 条业务命令 + 1 条 ShutdownSignal 全部处理完）"
    );
    assert!(core.is_stopped());
}

/// 停机时落最终快照：状态活在撮合线程的闭包里，线程一走就没了
#[test]
fn shutdown_takes_final_snapshot_and_state_survives() {
    let dir = temp_dir("shutdown_snapshot");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    {
        let mut core = new_core();
        core.enable_journaling(&journal).unwrap();
        core.enable_snapshotting(&snaps).unwrap();
        core.startup();

        core.submit_command(OrderCommand {
            command: OrderCommandType::AddUser,
            uid: 1,
            ..Default::default()
        });
        core.submit_command(OrderCommand {
            command: OrderCommandType::BalanceAdjustment,
            uid: 1,
            symbol: 1,
            price: 4_200,
            order_id: 1,
            ..Default::default()
        });

        core.shutdown().expect("停机失败");

        // 停机后一律拒绝，且必须是"已停机"而不是"中毒"
        let rejected = core.submit_command(OrderCommand {
            command: OrderCommandType::AddUser,
            uid: 99,
            ..Default::default()
        });
        assert_eq!(rejected.result_code, CommandResultCode::EngineStopped);
        assert!(core.shutdown().is_ok(), "重复停机应当幂等");
    }

    // 快照档案号应停在最后一条业务命令上（ShutdownSignal 不入 WAL、不占序号）
    let store = matching_core::core::snapshot::SnapshotStore::new(&snaps).unwrap();
    assert_eq!(store.get_latest_seq_id().unwrap(), Some(2));

    // 只靠快照即可完整恢复，日志无需再重放
    let mut restored = new_core();
    restored.enable_snapshotting(&snaps).unwrap();
    assert!(restored.load_latest_snapshot().unwrap());
    assert_eq!(
        restored.balance_of(1, 1).unwrap(),
        4_200,
        "停机快照未包含完整状态"
    );
    let summary = restored.replay_journal(&journal).expect("重放失败");
    assert_eq!(summary.replayed, 0, "最终快照已覆盖全部命令，不该再重放");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 中毒后停机不得落快照：状态可能停在半更新的位置，存下去等于把损坏固化
#[test]
fn shutdown_after_poison_does_not_snapshot() {
    let dir = temp_dir("shutdown_poisoned");
    let snaps = dir.join("snapshots");

    let mut core = new_core();
    core.enable_snapshotting(&snaps).unwrap();
    core.set_result_consumer(Arc::new(|cmd: &OrderCommand| {
        if cmd.uid == 2 {
            panic!("下游回调故意 panic");
        }
    }))
    .unwrap();
    core.startup();

    add_user(&mut core, 1);
    add_user(&mut core, 2); // 触发 panic

    for _ in 0..300 {
        if core.is_poisoned() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(core.is_poisoned());

    core.shutdown().expect("中毒后停机仍应成功返回");

    let store = matching_core::core::snapshot::SnapshotStore::new(&snaps).unwrap();
    assert_eq!(
        store.get_latest_seq_id().unwrap(),
        None,
        "中毒后不应写出快照 —— 那会把可能已损坏的状态固化下来"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 同步态也要能停机：没有消费者线程，但快照与 WAL 收尾照做
#[test]
fn shutdown_works_in_sync_mode() {
    let dir = temp_dir("shutdown_sync");
    let journal = dir.join("journal.bin");
    let snaps = dir.join("snapshots");

    let mut core = new_core();
    core.enable_journaling(&journal).unwrap();
    core.enable_snapshotting(&snaps).unwrap();
    add_user(&mut core, 1);

    core.shutdown().expect("同步态停机失败");
    assert!(core.is_stopped());

    let store = matching_core::core::snapshot::SnapshotStore::new(&snaps).unwrap();
    assert_eq!(store.get_latest_seq_id().unwrap(), Some(1));

    let rejected = core.submit_command(OrderCommand {
        command: OrderCommandType::AddUser,
        uid: 2,
        ..Default::default()
    });
    assert_eq!(rejected.result_code, CommandResultCode::EngineStopped);
    let _ = std::fs::remove_dir_all(&dir);
}
