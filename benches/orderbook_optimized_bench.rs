use criterion::{black_box, criterion_group, criterion_main, Criterion};
use matching_core::api::*;
use matching_core::core::orderbook::{DirectOrderBook, NaiveOrderBook, OrderBook};

fn bench_naive_orderbook(c: &mut Criterion) {
    let spec = CoreSymbolSpecification::default();
    let mut orderbook = NaiveOrderBook::new(spec);

    c.bench_function("NaiveOrderBook_GTC", |b| {
        b.iter(|| {
            let mut cmd = OrderCommand {
                command: OrderCommandType::PlaceOrder,
                uid: black_box(1001),
                order_id: black_box(5001),
                symbol: 100,
                price: black_box(100),
                size: black_box(10),
                action: OrderAction::Ask,
                order_type: OrderType::Gtc,
                ..Default::default()
            };
            orderbook.new_order(&mut cmd);
        });
    });
}

fn bench_direct_orderbook(c: &mut Criterion) {
    let spec = CoreSymbolSpecification::default();
    let mut orderbook = DirectOrderBook::new(spec);

    c.bench_function("DirectOrderBook_GTC", |b| {
        b.iter(|| {
            let mut cmd = OrderCommand {
                command: OrderCommandType::PlaceOrder,
                uid: black_box(1001),
                order_id: black_box(5001),
                symbol: 100,
                price: black_box(100),
                size: black_box(10),
                action: OrderAction::Ask,
                order_type: OrderType::Gtc,
                ..Default::default()
            };
            orderbook.new_order(&mut cmd);
        });
    });
}

// DirectOrderBookOptimized 已从对比中移除：它的链表与价格桶记账未维护
// （详见该类型的文档注释），"快" 有相当部分来自撤单几乎不干活、撮合提前退出，
// 与 DirectOrderBook 同台比较得不出有意义的结论。
criterion_group!(
    benches,
    bench_naive_orderbook,
    bench_direct_orderbook
);
criterion_main!(benches);

