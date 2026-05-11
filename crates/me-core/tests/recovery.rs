//! Crash recovery integration test.
//!
//! For each scenario:
//! - Engine A is opened with persistence, runs a sequence of commands, then
//!   is dropped (simulating a crash since `Drop` doesn't run any special
//!   teardown — the WAL was synced after every submit).
//! - Engine B is opened against the same WAL+snapshot directory and
//!   `restore` runs implicitly.
//! - We compare a structured `StateSummary` of both engines: balances,
//!   holds, totals, and resting order counts must match exactly.

use std::collections::BTreeMap;

use me_core::MatchingEngine;
use me_types::{
    AddUser, AdjustBalance, Amount, Bps, CancelOrder, ClientOrderId, Command, CurrencyId,
    FeeSchedule, OrderType, PlaceOrder, Price, PriceBand, SelfTradePrevention, Side, Size, SymbolId,
    SymbolKindParams, SymbolSpec, TimeInForce, Timestamp, UserId,
};
use tempfile::tempdir;

const BTC: CurrencyId = CurrencyId(1);
const USDT: CurrencyId = CurrencyId(2);
const BTC_USDT: SymbolId = SymbolId(1);

fn spec() -> SymbolSpec {
    SymbolSpec {
        symbol_id: BTC_USDT,
        base_currency: BTC,
        quote_currency: USDT,
        base_minor_per_major: 100_000_000,
        quote_minor_per_major: 1_000_000,
        tick_size: Price(1),
        lot_size: Size(1),
        min_order_size: Size(1),
        max_order_size: Size(i64::MAX),
        fee_schedule: FeeSchedule { maker_bps: Bps(10), taker_bps: Bps(20) },
        price_band: PriceBand::none(),
        kind_params: SymbolKindParams::Spot,
        is_suspended: false,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StateSummary {
    last_applied_seq: u64,
    accounts: BTreeMap<(u64, u32), (i64, i64)>, // (uid,cur) -> (free, held)
    exchange_balances: BTreeMap<u32, i64>,
    total_internal: BTreeMap<u32, i128>,
    resting: BTreeMap<u32, usize>, // symbol_id.0 -> count
}

impl StateSummary {
    fn from_engine(eng: &MatchingEngine, users: &[UserId], symbols: &[SymbolId]) -> Self {
        let mut accounts = BTreeMap::new();
        let mut exchange_balances = BTreeMap::new();
        let mut total_internal = BTreeMap::new();
        let mut resting = BTreeMap::new();

        for uid in users {
            for cur in [BTC, USDT] {
                let acct = eng.risk().account(*uid);
                if let Some(a) = acct {
                    accounts.insert((uid.0, cur.0), (a.free(cur).raw(), a.held(cur).raw()));
                }
            }
        }
        for cur in [BTC, USDT] {
            let acct = eng.risk().account(me_risk::EXCHANGE_ACCOUNT);
            exchange_balances.insert(cur.0, acct.map(|a| a.free(cur).raw()).unwrap_or(0));
            total_internal.insert(cur.0, eng.risk().total_internal(cur));
        }
        for sym in symbols {
            if let Some(book) = eng.book(*sym) {
                resting.insert(sym.0, book.total_resting_orders());
            }
        }
        Self {
            last_applied_seq: eng.last_applied_seq().0,
            accounts,
            exchange_balances,
            total_internal,
            resting,
        }
    }
}

fn add_user(eng: &mut MatchingEngine, uid: u64) {
    eng.submit(Command::AddUser(AddUser { user_id: UserId(uid) }), Timestamp(0));
}

fn deposit(eng: &mut MatchingEngine, uid: u64, cur: CurrencyId, amt: i64) {
    eng.submit(
        Command::AdjustBalance(AdjustBalance {
            user_id: UserId(uid),
            currency_id: cur,
            delta: Amount(amt),
            transaction_id: 0,
        }),
        Timestamp(0),
    );
}

fn place(
    eng: &mut MatchingEngine,
    uid: u64,
    coid: u64,
    side: Side,
    price: i64,
    size: i64,
    tif: TimeInForce,
) {
    eng.submit(
        Command::PlaceOrder(PlaceOrder {
            user_id: UserId(uid),
            symbol_id: BTC_USDT,
            client_order_id: ClientOrderId(coid),
            side,
            order_type: OrderType::Limit,
            time_in_force: tif,
            price: Some(Price(price)),
            size: Size(size),
            reserve_price: None,
            stop_price: None,
            visible_size: None,
            self_trade_prevention: SelfTradePrevention::None,
        }),
        Timestamp(0),
    );
}

fn cancel(eng: &mut MatchingEngine, uid: u64, oid: u64) {
    eng.submit(
        Command::CancelOrder(CancelOrder {
            user_id: UserId(uid),
            symbol_id: BTC_USDT,
            order_id: me_types::OrderId(oid),
        }),
        Timestamp(0),
    );
}

fn seed_scenario(eng: &mut MatchingEngine) {
    eng.register_symbol(spec()).unwrap();
    add_user(eng, 1);
    add_user(eng, 2);
    add_user(eng, 3);
    deposit(eng, 1, BTC, 200_000_000);
    deposit(eng, 2, USDT, 5_000_000_000);
    deposit(eng, 3, USDT, 1_000_000_000);
    place(eng, 1, 1, Side::Ask, 50_000_000, 80_000_000, TimeInForce::Gtc);
    place(eng, 2, 2, Side::Bid, 50_000_000, 30_000_000, TimeInForce::Ioc);
    place(eng, 3, 3, Side::Bid, 49_000_000, 10_000_000, TimeInForce::Gtc);
    place(eng, 1, 4, Side::Ask, 51_000_000, 50_000_000, TimeInForce::Gtc);
}

fn extra_post_snapshot(eng: &mut MatchingEngine) {
    place(eng, 2, 5, Side::Bid, 51_000_000, 25_000_000, TimeInForce::Ioc);
    cancel(eng, 3, 7); // 3's resting bid — order_id depends on internal counter
    place(eng, 3, 6, Side::Bid, 50_500_000, 5_000_000, TimeInForce::Gtc);
}

#[test]
fn replay_from_wal_only_reproduces_state() {
    let dir = tempdir().unwrap();
    let wal = dir.path().join("wal.bin");
    let snap = dir.path().join("snapshots");

    let summary_a = {
        let mut eng = MatchingEngine::with_persistence(&wal, &snap).unwrap();
        seed_scenario(&mut eng);
        extra_post_snapshot(&mut eng);
        StateSummary::from_engine(
            &eng,
            &[UserId(1), UserId(2), UserId(3)],
            &[BTC_USDT],
        )
    };

    // Engine B opens cold; no snapshot exists yet, so it must reconstruct
    // every byte of state from the WAL alone.
    let eng_b = MatchingEngine::with_persistence(&wal, &snap).unwrap();
    let summary_b = StateSummary::from_engine(
        &eng_b,
        &[UserId(1), UserId(2), UserId(3)],
        &[BTC_USDT],
    );

    assert_eq!(summary_a, summary_b);
}

#[test]
fn snapshot_plus_wal_delta_reproduces_state() {
    let dir = tempdir().unwrap();
    let wal = dir.path().join("wal.bin");
    let snap = dir.path().join("snapshots");

    let summary_a = {
        let mut eng = MatchingEngine::with_persistence(&wal, &snap).unwrap();
        seed_scenario(&mut eng);
        eng.take_snapshot().unwrap();
        extra_post_snapshot(&mut eng);
        StateSummary::from_engine(
            &eng,
            &[UserId(1), UserId(2), UserId(3)],
            &[BTC_USDT],
        )
    };

    let eng_b = MatchingEngine::with_persistence(&wal, &snap).unwrap();
    let summary_b = StateSummary::from_engine(
        &eng_b,
        &[UserId(1), UserId(2), UserId(3)],
        &[BTC_USDT],
    );

    assert_eq!(summary_a, summary_b);
}

#[test]
fn snapshot_only_no_wal_delta_reproduces_state() {
    let dir = tempdir().unwrap();
    let wal = dir.path().join("wal.bin");
    let snap = dir.path().join("snapshots");

    let summary_a = {
        let mut eng = MatchingEngine::with_persistence(&wal, &snap).unwrap();
        seed_scenario(&mut eng);
        eng.take_snapshot().unwrap();
        StateSummary::from_engine(
            &eng,
            &[UserId(1), UserId(2), UserId(3)],
            &[BTC_USDT],
        )
    };

    let eng_b = MatchingEngine::with_persistence(&wal, &snap).unwrap();
    let summary_b = StateSummary::from_engine(
        &eng_b,
        &[UserId(1), UserId(2), UserId(3)],
        &[BTC_USDT],
    );

    assert_eq!(summary_a, summary_b);
}

#[test]
fn restore_preserves_conservation_invariant() {
    let dir = tempdir().unwrap();
    let wal = dir.path().join("wal.bin");
    let snap = dir.path().join("snapshots");

    let (deposits_btc, deposits_usdt) = {
        let mut eng = MatchingEngine::with_persistence(&wal, &snap).unwrap();
        seed_scenario(&mut eng);
        eng.take_snapshot().unwrap();
        extra_post_snapshot(&mut eng);
        (200_000_000i128, 5_000_000_000i128 + 1_000_000_000i128)
    };

    let eng_b = MatchingEngine::with_persistence(&wal, &snap).unwrap();
    assert_eq!(eng_b.risk().total_internal(BTC), deposits_btc);
    assert_eq!(eng_b.risk().total_internal(USDT), deposits_usdt);
}
