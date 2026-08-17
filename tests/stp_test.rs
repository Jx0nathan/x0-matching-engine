//! 自成交防范（STP）测试：五种模式各自的行为，以及每种模式下的资金守恒。

use matching_core::api::*;
use matching_core::core::exchange::{ExchangeConfig, ExchangeCore};

const BASE: Currency = 0;
const QUOTE: Currency = 1;
const SYM: SymbolId = 1;

struct Harness {
    core: ExchangeCore,
    tx: i64,
    deposited_base: i64,
    deposited_quote: i64,
}

impl Harness {
    fn new(stp: SelfTradePrevention) -> Self {
        let mut core = ExchangeCore::new(ExchangeConfig::default());
        core.add_symbol(CoreSymbolSpecification {
            symbol_id: SYM,
            symbol_type: SymbolType::CurrencyExchangePair,
            base_currency: BASE,
            quote_currency: QUOTE,
            base_scale_k: 1,
            quote_scale_k: 1,
            taker_fee: 2,
            maker_fee: 1,
            margin_buy: 0,
            margin_sell: 0,
            stp,
        })
        .expect("注册交易对失败");

        let mut h = Self {
            core,
            tx: 0,
            deposited_base: 0,
            deposited_quote: 0,
        };
        for uid in [1, 2] {
            h.core.submit_command(OrderCommand {
                command: OrderCommandType::AddUser,
                uid,
                ..Default::default()
            });
            h.deposit(uid, BASE, 1_000);
            h.deposit(uid, QUOTE, 100_000);
        }
        h
    }

    fn deposit(&mut self, uid: UserId, currency: Currency, amount: i64) {
        self.tx += 1;
        let code = self
            .core
            .submit_command(OrderCommand {
                command: OrderCommandType::BalanceAdjustment,
                uid,
                symbol: currency,
                price: amount,
                order_id: self.tx as u64,
                ..Default::default()
            })
            .result_code;
        assert_eq!(code, CommandResultCode::Success);
        match currency {
            BASE => self.deposited_base += amount,
            QUOTE => self.deposited_quote += amount,
            _ => {}
        }
    }

    fn place(
        &mut self,
        uid: UserId,
        order_id: OrderId,
        action: OrderAction,
        price: Price,
        size: Size,
    ) -> OrderCommand {
        self.core.submit_command(OrderCommand {
            command: OrderCommandType::PlaceOrder,
            uid,
            order_id,
            symbol: SYM,
            price,
            reserve_price: price,
            size,
            action,
            order_type: OrderType::Gtc,
            ..Default::default()
        })
    }

    fn cancel(&mut self, uid: UserId, order_id: OrderId) -> CommandResultCode {
        self.core
            .submit_command(OrderCommand {
                command: OrderCommandType::CancelOrder,
                uid,
                order_id,
                symbol: SYM,
                ..Default::default()
            })
            .result_code
    }

    fn balance(&self, uid: UserId, currency: Currency) -> i64 {
        self.core.balance_of(uid, currency).unwrap()
    }

    /// 所有挂单撤净后校验守恒
    fn assert_conserved(&self) {
        let quote = self.balance(1, QUOTE) + self.balance(2, QUOTE) + self.core.fees_collected(QUOTE).unwrap();
        let base = self.balance(1, BASE) + self.balance(2, BASE) + self.core.fees_collected(BASE).unwrap();
        assert_eq!(quote, self.deposited_quote, "quote 不守恒");
        assert_eq!(base, self.deposited_base, "base 不守恒");
    }
}

fn count(cmd: &OrderCommand, kind: MatcherEventType) -> usize {
    cmd.matcher_events
        .iter()
        .filter(|e| e.event_type == kind)
        .count()
}

/// 同一用户先挂卖单，再下会与之成交的买单
fn self_cross(h: &mut Harness) -> OrderCommand {
    h.place(1, 1, OrderAction::Ask, 100, 10);
    h.place(1, 2, OrderAction::Bid, 100, 10)
}

/// None：不做防范，自成交照常发生（保留旧行为，仅供测试场景使用）
#[test]
fn none_allows_self_trade() {
    let mut h = Harness::new(SelfTradePrevention::None);
    let r = self_cross(&mut h);
    assert_eq!(count(&r, MatcherEventType::Trade), 1, "None 模式应允许自成交");
    h.assert_conserved();
}

/// CancelTaker：新单整单撤销，簿中旧单保留
#[test]
fn cancel_taker_keeps_maker() {
    let mut h = Harness::new(SelfTradePrevention::CancelTaker);
    let r = self_cross(&mut h);

    assert_eq!(count(&r, MatcherEventType::Trade), 0, "不应发生自成交");
    assert_eq!(count(&r, MatcherEventType::Reject), 1, "新单应被整单拒绝");
    assert_eq!(count(&r, MatcherEventType::RejectMaker), 0, "不应撤销挂单");

    // 旧卖单仍在簿中：让另一个用户来吃掉
    let taken = h.place(2, 3, OrderAction::Bid, 100, 10);
    assert_eq!(count(&taken, MatcherEventType::Trade), 1, "挂单应仍在簿中");

    h.assert_conserved();
}

/// CancelMaker：撤掉簿中自己的挂单，新单继续
#[test]
fn cancel_maker_removes_resting_order() {
    let mut h = Harness::new(SelfTradePrevention::CancelMaker);
    let r = self_cross(&mut h);

    assert_eq!(count(&r, MatcherEventType::Trade), 0, "不应发生自成交");
    assert_eq!(count(&r, MatcherEventType::RejectMaker), 1, "应撤销簿中挂单");

    // 旧卖单已被撤：另一用户的买单吃不到东西
    let taken = h.place(2, 3, OrderAction::Bid, 100, 10);
    assert_eq!(count(&taken, MatcherEventType::Trade), 0, "挂单应已被撤销");

    // 新买单挂进了簿子，撤掉它和 uid2 的买单后校验守恒
    assert_eq!(h.cancel(1, 2), CommandResultCode::Success);
    assert_eq!(h.cancel(2, 3), CommandResultCode::Success);
    h.assert_conserved();
}

/// CancelBoth：双方都撤，簿子清空
#[test]
fn cancel_both_clears_both_sides() {
    let mut h = Harness::new(SelfTradePrevention::CancelBoth);
    let r = self_cross(&mut h);

    assert_eq!(count(&r, MatcherEventType::Trade), 0);
    assert_eq!(count(&r, MatcherEventType::RejectMaker), 1, "应撤销簿中挂单");
    assert_eq!(count(&r, MatcherEventType::Reject), 1, "应撤销新单");

    // 簿子已空
    let probe = h.place(2, 3, OrderAction::Bid, 100, 10);
    assert_eq!(count(&probe, MatcherEventType::Trade), 0);

    assert_eq!(h.cancel(2, 3), CommandResultCode::Success);
    h.assert_conserved();
}

/// Skip：跳过自己的挂单，双方订单都保留
#[test]
fn skip_keeps_both_orders() {
    let mut h = Harness::new(SelfTradePrevention::Skip);
    let r = self_cross(&mut h);

    assert_eq!(count(&r, MatcherEventType::Trade), 0, "不应发生自成交");
    assert_eq!(count(&r, MatcherEventType::Reject), 0, "新单不应被拒");
    assert_eq!(count(&r, MatcherEventType::RejectMaker), 0, "挂单不应被撤");

    // 两张单都还在：uid2 都能吃到
    let hit_ask = h.place(2, 3, OrderAction::Bid, 100, 10);
    assert_eq!(count(&hit_ask, MatcherEventType::Trade), 1, "卖单应仍在簿中");
    let hit_bid = h.place(2, 4, OrderAction::Ask, 100, 10);
    assert_eq!(count(&hit_bid, MatcherEventType::Trade), 1, "买单应仍在簿中");

    h.assert_conserved();
}

/// Skip 的关键行为：跳过自己的挂单后，继续与更差价位的他人订单撮合
#[test]
fn skip_continues_matching_other_makers() {
    let mut h = Harness::new(SelfTradePrevention::Skip);

    h.place(1, 1, OrderAction::Ask, 100, 10); // 自己的卖一
    h.place(2, 2, OrderAction::Ask, 101, 10); // 他人的卖二

    // 买到 101：应跳过自己 100 的挂单，成交在 101
    let r = h.place(1, 3, OrderAction::Bid, 101, 10);

    let trades: Vec<_> = r
        .matcher_events
        .iter()
        .filter(|e| e.event_type == MatcherEventType::Trade)
        .collect();
    assert_eq!(trades.len(), 1, "应与他人订单成交一次");
    assert_eq!(trades[0].price, 101, "应成交在更差的价位，而不是自己的 100");
    assert_eq!(trades[0].matched_order_uid, 2, "对手方应是他人");

    // 自己的卖单仍在簿中
    let taken = h.place(2, 4, OrderAction::Bid, 100, 10);
    assert_eq!(count(&taken, MatcherEventType::Trade), 1);

    h.assert_conserved();
}

/// CancelTaker 下，部分成交后遇到自己的挂单：已成交部分保留，剩余部分撤销
#[test]
fn cancel_taker_after_partial_fill_keeps_fills() {
    let mut h = Harness::new(SelfTradePrevention::CancelTaker);

    h.place(2, 1, OrderAction::Ask, 100, 4); // 他人卖 4
    h.place(1, 2, OrderAction::Ask, 100, 10); // 自己卖 10（同价，排在后面）

    // 买 10：先吃掉他人的 4，再碰到自己的挂单 => 剩余 6 撤销
    let r = h.place(1, 3, OrderAction::Bid, 100, 10);

    let filled: i64 = r
        .matcher_events
        .iter()
        .filter(|e| e.event_type == MatcherEventType::Trade)
        .map(|e| e.size)
        .sum();
    assert_eq!(filled, 4, "应只成交他人那 4 手");

    let rejected: i64 = r
        .matcher_events
        .iter()
        .filter(|e| e.event_type == MatcherEventType::Reject)
        .map(|e| e.size)
        .sum();
    assert_eq!(rejected, 6, "剩余 6 手应被撤销并退款");

    assert_eq!(h.cancel(1, 2), CommandResultCode::Success);
    h.assert_conserved();
}

/// 每种模式跑完同一套指令后都必须资金守恒
#[test]
fn all_modes_conserve_funds() {
    for stp in [
        SelfTradePrevention::None,
        SelfTradePrevention::CancelTaker,
        SelfTradePrevention::CancelMaker,
        SelfTradePrevention::CancelBoth,
        SelfTradePrevention::Skip,
    ] {
        let mut h = Harness::new(stp);

        h.place(1, 1, OrderAction::Ask, 100, 10);
        h.place(2, 2, OrderAction::Ask, 101, 10);
        h.place(1, 3, OrderAction::Bid, 101, 15);
        h.place(2, 4, OrderAction::Bid, 99, 5);

        // 尽力撤净所有可能残留的挂单
        for (uid, oid) in [(1, 1), (2, 2), (1, 3), (2, 4)] {
            let _ = h.cancel(uid, oid);
        }

        h.assert_conserved();
    }
}
