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

    assert!(core.take_snapshot(1).is_err(), "启动后应拒绝快照");
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

    // 翻转第一条记录 payload 的首字节（记录头占 12 字节）
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[12] ^= 0xFF;
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
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
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
