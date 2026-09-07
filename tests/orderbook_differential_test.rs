//! 差分测试：拿最朴素的 NaiveOrderBook 当 oracle，对拍生产在用的 DirectOrderBook。
//!
//! 定点用例只能覆盖想得到的场景，而订单簿的 bug 恰恰藏在"挂单、部分成交、撤单、
//! 减量交织之后的某个状态"里。随机命令流 + 双实现对拍能自动逼出这类分歧：
//! 两个实现对同一串命令必须给出相同的成交量、相同的结果码、相同的盘口。

use matching_core::api::*;
use matching_core::core::orderbook::{DirectOrderBook, NaiveOrderBook, OrderBook};

/// xorshift64*，固定种子可复现。刻意不引第三方 rng 依赖。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn spec() -> CoreSymbolSpecification {
    CoreSymbolSpecification {
        symbol_id: 1,
        symbol_type: SymbolType::CurrencyExchangePair,
        base_currency: 0,
        quote_currency: 1,
        base_scale_k: 1,
        quote_scale_k: 1,
        taker_fee: 0,
        maker_fee: 0,
        margin_buy: 0,
        margin_sell: 0,
        stp: SelfTradePrevention::None,
    }
}

/// 成交事件的可比较摘要：只取与撮合语义相关的部分，忽略事件产出顺序
fn trade_digest(cmd: &OrderCommand) -> Vec<(Size, Price, OrderId)> {
    let mut v: Vec<_> = cmd
        .matcher_events
        .iter()
        .filter(|e| e.event_type == MatcherEventType::Trade)
        .map(|e| (e.size, e.price, e.matched_order_id))
        .collect();
    v.sort_unstable();
    v
}

/// 盘口快照摘要：两侧的价 / 量 / 笔数，外加总量。两个实现必须完全一致。
#[derive(Debug, PartialEq, Eq)]
struct BookDigest {
    ask_prices: Vec<Price>,
    ask_volumes: Vec<Size>,
    ask_counts: Vec<usize>,
    bid_prices: Vec<Price>,
    bid_volumes: Vec<Size>,
    bid_counts: Vec<usize>,
    total_ask: Size,
    total_bid: Size,
}

fn book_digest(book: &dyn OrderBook) -> BookDigest {
    let l2 = book.get_l2_data(64);
    BookDigest {
        ask_prices: l2.ask_prices,
        ask_volumes: l2.ask_volumes,
        ask_counts: l2.ask_order_counts,
        bid_prices: l2.bid_prices,
        bid_volumes: l2.bid_volumes,
        bid_counts: l2.bid_order_counts,
        total_ask: book.get_total_ask_volume(),
        total_bid: book.get_total_bid_volume(),
    }
}

fn run_stream(seed: u64, steps: usize) {
    let mut naive = NaiveOrderBook::new(spec());
    let mut direct = DirectOrderBook::new(spec());
    let mut rng = Rng(seed);
    let mut next_id: u64 = 1;
    // 已挂出的 (订单号, 下单人)，供撤单 / 减量挑选。记下 uid 是因为两个实现都会
    // 校验 uid，用错了会走进"当作不存在"的分支，产生与撮合无关的假分歧。
    let mut live: Vec<(OrderId, UserId)> = Vec::new();

    for step in 0..steps {
        let roll = rng.below(100);
        let mut a;
        let mut b;

        if roll < 60 || live.is_empty() {
            // 下单：价格窄区间，逼出同价多单与穿价成交
            let id = next_id;
            next_id += 1;
            let price = 95 + rng.below(11) as i64;
            let size = 1 + rng.below(8) as i64;
            let action = if rng.below(2) == 0 { OrderAction::Bid } else { OrderAction::Ask };
            let uid = 1 + rng.below(4);
            let mk = || OrderCommand {
                command: OrderCommandType::PlaceOrder,
                uid,
                order_id: id,
                symbol: 1,
                price,
                reserve_price: price,
                size,
                action,
                order_type: OrderType::Gtc,
                ..Default::default()
            };
            a = mk();
            b = mk();
            live.push((id, uid));
            let ra = naive.new_order(&mut a);
            let rb = direct.new_order(&mut b);
            assert_eq!(ra, rb, "seed {seed} step {step}: 下单结果码分歧");
        } else if roll < 85 {
            // 撤单
            let (id, uid) = live[rng.below(live.len() as u64) as usize];
            let mk = || OrderCommand {
                command: OrderCommandType::CancelOrder,
                uid,
                order_id: id,
                symbol: 1,
                ..Default::default()
            };
            a = mk();
            b = mk();
            let ra = naive.cancel_order(&mut a);
            let rb = direct.cancel_order(&mut b);
            assert_eq!(ra, rb, "seed {seed} step {step}: 撤单结果码分歧（订单 {id}）");
        } else {
            // 减量
            let (id, uid) = live[rng.below(live.len() as u64) as usize];
            let cut = 1 + rng.below(4) as i64;
            let mk = || OrderCommand {
                command: OrderCommandType::ReduceOrder,
                uid,
                order_id: id,
                symbol: 1,
                size: cut,
                ..Default::default()
            };
            a = mk();
            b = mk();
            let ra = naive.reduce_order(&mut a);
            let rb = direct.reduce_order(&mut b);
            assert_eq!(ra, rb, "seed {seed} step {step}: 减量结果码分歧（订单 {id}）");
        }

        assert_eq!(
            trade_digest(&a),
            trade_digest(&b),
            "seed {seed} step {step}: 成交明细分歧（naive vs direct）"
        );
        assert_eq!(
            book_digest(&naive),
            book_digest(&direct),
            "seed {seed} step {step}: 盘口状态分歧（naive vs direct）"
        );
    }
}

#[test]
fn differential_naive_vs_direct() {
    for seed in [1u64, 7, 42, 1337, 90210, 0xDEAD_BEEF, 0x5EED, 987_654_321] {
        run_stream(seed, 400);
    }
}
