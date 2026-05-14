//! Integration test: AsyncMatchingEngine must produce engine state
//! bit-identical to running the same command stream through the synchronous
//! MatchingEngine. Also stress-tests the ring buffer's backpressure: the
//! ring is intentionally smaller than the command stream so the producer
//! has to wait for the consumer.

use std::collections::BTreeMap;

use me_core::{AsyncMatchingEngine, MatchingEngine};
use me_types::{
    AddUser, AdjustBalance, Amount, Bps, ClientOrderId, Command, CurrencyId, FeeSchedule,
    OrderType, PlaceOrder, Price, PriceBand, SelfTradePrevention, Side, Size, SymbolId,
    SymbolKindParams, SymbolSpec, TimeInForce, Timestamp, UserId,
};

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
        fee_schedule: FeeSchedule {
            maker_bps: Bps(10),
            taker_bps: Bps(20),
        },
        price_band: PriceBand::none(),
        kind_params: SymbolKindParams::Spot,
        is_suspended: false,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StateSummary {
    last_applied_seq: u64,
    accounts: BTreeMap<(u64, u32), (i64, i64)>,
    exchange_balances: BTreeMap<u32, i64>,
    total_internal: BTreeMap<u32, i128>,
    resting: BTreeMap<u32, usize>,
}

impl StateSummary {
    fn from(eng: &MatchingEngine, users: &[UserId]) -> Self {
        let mut accounts = BTreeMap::new();
        let mut exchange_balances = BTreeMap::new();
        let mut total_internal = BTreeMap::new();
        let mut resting = BTreeMap::new();

        for uid in users {
            for cur in [BTC, USDT] {
                if let Some(a) = eng.risk().account(*uid) {
                    accounts.insert((uid.0, cur.0), (a.free(cur).raw(), a.held(cur).raw()));
                }
            }
        }
        for cur in [BTC, USDT] {
            let acct = eng.risk().account(me_risk::EXCHANGE_ACCOUNT);
            exchange_balances.insert(cur.0, acct.map(|a| a.free(cur).raw()).unwrap_or(0));
            total_internal.insert(cur.0, eng.risk().total_internal(cur));
        }
        if let Some(book) = eng.book(BTC_USDT) {
            resting.insert(BTC_USDT.0, book.total_resting_orders());
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

fn build_commands() -> Vec<(Command, Timestamp)> {
    let mut v = Vec::new();
    let t = Timestamp(0);

    v.push((Command::RegisterSymbol(spec()), t));
    for uid in [1u64, 2, 3, 4, 5] {
        v.push((
            Command::AddUser(AddUser {
                user_id: UserId(uid),
            }),
            t,
        ));
    }
    v.push(adj(1, BTC, 500_000_000));
    v.push(adj(2, USDT, 10_000_000_000));
    v.push(adj(3, USDT, 5_000_000_000));
    v.push(adj(4, BTC, 200_000_000));
    v.push(adj(5, USDT, 2_000_000_000));

    let mut coid = 0u64;
    let mut next = || {
        coid += 1;
        coid
    };

    v.push(place(
        1,
        next(),
        Side::Ask,
        50_000_000,
        100_000_000,
        TimeInForce::Gtc,
    ));
    v.push(place(
        4,
        next(),
        Side::Ask,
        51_000_000,
        80_000_000,
        TimeInForce::Gtc,
    ));
    v.push(place(
        4,
        next(),
        Side::Ask,
        52_000_000,
        70_000_000,
        TimeInForce::Gtc,
    ));
    v.push(place(
        2,
        next(),
        Side::Bid,
        50_000_000,
        30_000_000,
        TimeInForce::Ioc,
    ));
    v.push(place(
        3,
        next(),
        Side::Bid,
        51_000_000,
        60_000_000,
        TimeInForce::Gtc,
    ));
    v.push(place(
        5,
        next(),
        Side::Bid,
        49_000_000,
        20_000_000,
        TimeInForce::PostOnly,
    ));
    v.push(place(
        2,
        next(),
        Side::Bid,
        52_000_000,
        50_000_000,
        TimeInForce::Fok,
    ));
    v.push(place(
        3,
        next(),
        Side::Ask,
        53_000_000,
        10_000_000,
        TimeInForce::Gtc,
    ));
    v.push(place(
        5,
        next(),
        Side::Bid,
        50_000_000,
        5_000_000,
        TimeInForce::Ioc,
    ));
    v
}

fn adj(uid: u64, cur: CurrencyId, delta: i64) -> (Command, Timestamp) {
    (
        Command::AdjustBalance(AdjustBalance {
            user_id: UserId(uid),
            currency_id: cur,
            delta: Amount(delta),
            transaction_id: 0,
        }),
        Timestamp(0),
    )
}

fn place(
    uid: u64,
    coid: u64,
    side: Side,
    price: i64,
    size: i64,
    tif: TimeInForce,
) -> (Command, Timestamp) {
    (
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
    )
}

#[test]
fn async_and_sync_produce_identical_state() {
    let cmds = build_commands();
    let users = [UserId(1), UserId(2), UserId(3), UserId(4), UserId(5)];

    let sync_summary = {
        let mut eng = MatchingEngine::new();
        for (cmd, ts) in cmds.clone() {
            eng.submit(cmd, ts);
        }
        StateSummary::from(&eng, &users)
    };

    let async_summary = {
        let mut eng = AsyncMatchingEngine::new(MatchingEngine::new(), 64);
        for (cmd, ts) in cmds {
            eng.submit(cmd, ts);
        }
        let inner = eng.shutdown();
        StateSummary::from(&inner, &users)
    };

    assert_eq!(sync_summary, async_summary);
}

#[test]
fn ring_backpressure_holds_correctness() {
    // Ring size 4 forces the producer to wait on the consumer repeatedly.
    let cmds = build_commands();
    let users = [UserId(1), UserId(2), UserId(3), UserId(4), UserId(5)];

    let mut eng = AsyncMatchingEngine::new(MatchingEngine::new(), 4);
    for (cmd, ts) in cmds.clone() {
        eng.submit(cmd, ts);
    }
    let inner = eng.shutdown();
    let summary = StateSummary::from(&inner, &users);

    // Compare against sync baseline.
    let mut sync = MatchingEngine::new();
    for (cmd, ts) in cmds {
        sync.submit(cmd, ts);
    }
    assert_eq!(summary, StateSummary::from(&sync, &users));
}

#[test]
fn many_submits_no_deadlock() {
    let mut eng = AsyncMatchingEngine::new(MatchingEngine::new(), 16);
    eng.submit(Command::RegisterSymbol(spec()), Timestamp(0));
    eng.submit(
        Command::AddUser(AddUser { user_id: UserId(1) }),
        Timestamp(0),
    );
    let (dep, _) = adj(1, USDT, 100_000_000_000);
    eng.submit(dep, Timestamp(0));
    for i in 0..500u64 {
        let (cmd, ts) = place(1, i + 1, Side::Bid, 49_999_999, 1_000, TimeInForce::Gtc);
        let _ = eng.submit(cmd, ts);
    }
    let inner = eng.shutdown();
    assert!(inner.book(BTC_USDT).unwrap().total_resting_orders() > 0);
}

#[test]
fn drop_without_shutdown_does_not_hang() {
    let cmds = build_commands();
    {
        let mut eng = AsyncMatchingEngine::new(MatchingEngine::new(), 16);
        for (cmd, ts) in cmds {
            eng.submit(cmd, ts);
        }
        // Drop here — the Drop impl must send a poison pill and join.
    }
    // If we reach here without hanging, the Drop path works.
}
