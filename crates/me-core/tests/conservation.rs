//! Property test: across any sequence of randomly generated commands,
//! `risk.total_internal(currency)` must equal the net of all accepted external
//! transfers in that currency. This is the M2 quality gate that proves the
//! "no fund leak / no fund minting" guarantee.

use std::collections::HashMap;

use me_core::MatchingEngine;
use me_types::{
    AddUser, AdjustBalance, Amount, Bps, CancelOrder, ClientOrderId, Command, CommandStatus,
    CurrencyId, FeeSchedule, OrderId, OrderType, PlaceOrder, Price, PriceBand, SelfTradePrevention,
    Side, Size, SymbolId, SymbolKindParams, SymbolSpec, TimeInForce, Timestamp, UserId,
};

use proptest::prelude::*;

fn build_engine() -> MatchingEngine {
    let mut eng = MatchingEngine::new();
    eng.register_symbol(SymbolSpec {
        symbol_id: SymbolId(1),
        base_currency: CurrencyId(1),
        quote_currency: CurrencyId(2),
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
    })
    .unwrap();
    eng
}

#[derive(Debug, Clone)]
enum Op {
    AddUser(u64),
    AdjustBalance { uid: u64, cur: u32, delta: i64 },
    Place { uid: u64, side: Side, price: i64, size: i64, tif: Tif },
    Cancel { uid: u64, order_id_hint: u64 },
}

#[derive(Debug, Clone, Copy)]
enum Tif { Gtc, Ioc, Fok, PostOnly }
impl Tif {
    fn into(self) -> TimeInForce {
        match self {
            Tif::Gtc => TimeInForce::Gtc,
            Tif::Ioc => TimeInForce::Ioc,
            Tif::Fok => TimeInForce::Fok,
            Tif::PostOnly => TimeInForce::PostOnly,
        }
    }
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let side = prop_oneof![Just(Side::Bid), Just(Side::Ask)];
    let tif = prop_oneof![Just(Tif::Gtc), Just(Tif::Ioc), Just(Tif::Fok), Just(Tif::PostOnly)];

    prop_oneof![
        // weight new-user lower than other ops so the sequence has more interesting state
        1 => (1u64..=5).prop_map(Op::AddUser),
        2 => (1u64..=5, 1u32..=2, -1_000_000i64..=1_000_000_000i64)
            .prop_map(|(uid, cur, delta)| Op::AdjustBalance { uid, cur, delta }),
        6 => (1u64..=5, side, 40_000_000i64..=60_000_000i64, 1_000i64..=20_000_000i64, tif)
            .prop_map(|(uid, side, price, size, tif)| Op::Place { uid, side, price, size, tif }),
        3 => (1u64..=5, 1u64..=80u64)
            .prop_map(|(uid, hint)| Op::Cancel { uid, order_id_hint: hint }),
    ]
}

fn op_sequence() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op_strategy(), 1..120)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn conservation_holds_under_random_command_stream(ops in op_sequence()) {
        let mut eng = build_engine();
        let mut external_net: HashMap<u32, i128> = HashMap::new();
        let mut next_id = 1u64;

        for op in ops {
            let (cmd, external_delta) = build_command(&op, &mut next_id);
            let receipt = eng.submit(cmd, Timestamp(0));

            // Only AdjustBalance moves the external boundary. It changes the boundary
            // iff it was Accepted (deposit always; withdrawal only if enough free).
            if matches!(receipt.status, CommandStatus::Accepted) {
                if let Some((cur, delta)) = external_delta {
                    *external_net.entry(cur).or_insert(0) += delta;
                }
            }

            for cur in [1u32, 2u32] {
                let internal = eng.risk().total_internal(CurrencyId(cur));
                let external = *external_net.get(&cur).unwrap_or(&0);
                prop_assert_eq!(
                    internal, external,
                    "currency {} broken after op {:?}: internal={}, external={}",
                    cur, op, internal, external
                );
            }
        }
    }
}

fn build_command(op: &Op, next_id: &mut u64) -> (Command, Option<(u32, i128)>) {
    let id = *next_id;
    *next_id += 1;
    match *op {
        Op::AddUser(uid) => (
            Command::AddUser(AddUser { user_id: UserId(uid) }),
            None,
        ),
        Op::AdjustBalance { uid, cur, delta } => (
            Command::AdjustBalance(AdjustBalance {
                user_id: UserId(uid),
                currency_id: CurrencyId(cur),
                delta: Amount(delta),
                transaction_id: id,
            }),
            Some((cur, delta as i128)),
        ),
        Op::Place { uid, side, price, size, tif } => (
            Command::PlaceOrder(PlaceOrder {
                user_id: UserId(uid),
                symbol_id: SymbolId(1),
                client_order_id: ClientOrderId(id),
                side,
                order_type: OrderType::Limit,
                time_in_force: tif.into(),
                price: Some(Price(price)),
                size: Size(size),
                reserve_price: None,
                stop_price: None,
                visible_size: None,
                self_trade_prevention: SelfTradePrevention::None,
            }),
            None,
        ),
        Op::Cancel { uid, order_id_hint } => (
            Command::CancelOrder(CancelOrder {
                user_id: UserId(uid),
                symbol_id: SymbolId(1),
                order_id: OrderId(order_id_hint),
            }),
            None,
        ),
    }
}
