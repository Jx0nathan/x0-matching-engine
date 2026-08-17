//! L2 深度数据的顺序与一致性，四个订单簿实现统一校验。
//!
//! 买盘必须按价格降序（买一在前），卖盘按升序（卖一在前）。
//! NaiveOrderBook 曾因直接遍历 BTreeMap（升序）而把买盘顺序输出反了。

use matching_core::api::*;
use matching_core::core::orderbook::{
    AdvancedOrderBook, DirectOrderBook, DirectOrderBookOptimized, NaiveOrderBook, OrderBook,
};

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

fn place(book: &mut dyn OrderBook, uid: u64, id: u64, action: OrderAction, price: i64, size: i64) {
    let mut cmd = OrderCommand {
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
    book.new_order(&mut cmd);
}

/// 灌入互不成交的多档买卖单，校验 L2 顺序与总量一致性
fn check_l2(name: &str, book: &mut dyn OrderBook) {
    // 卖盘 103 / 101 / 102，乱序挂入
    place(book, 1, 1, OrderAction::Ask, 103, 3);
    place(book, 1, 2, OrderAction::Ask, 101, 1);
    place(book, 1, 3, OrderAction::Ask, 102, 2);
    // 买盘 97 / 99 / 98，乱序挂入
    place(book, 2, 4, OrderAction::Bid, 97, 7);
    place(book, 2, 5, OrderAction::Bid, 99, 9);
    place(book, 2, 6, OrderAction::Bid, 98, 8);

    let l2 = book.get_l2_data(10);

    assert_eq!(
        l2.ask_prices,
        vec![101, 102, 103],
        "{name}: 卖盘应按价格升序（卖一在前）"
    );
    assert_eq!(l2.ask_volumes, vec![1, 2, 3], "{name}: 卖盘量与价格未对齐");

    assert_eq!(
        l2.bid_prices,
        vec![99, 98, 97],
        "{name}: 买盘应按价格降序（买一在前）"
    );
    assert_eq!(l2.bid_volumes, vec![9, 8, 7], "{name}: 买盘量与价格未对齐");

    assert_eq!(
        l2.ask_volumes.iter().sum::<i64>(),
        book.get_total_ask_volume(),
        "{name}: L2 卖盘总量与簿内统计不一致"
    );
    assert_eq!(
        l2.bid_volumes.iter().sum::<i64>(),
        book.get_total_bid_volume(),
        "{name}: L2 买盘总量与簿内统计不一致"
    );
}

/// depth 限制必须取最优的若干档，而不是任意若干档
fn check_l2_depth(name: &str, book: &mut dyn OrderBook) {
    place(book, 1, 1, OrderAction::Ask, 103, 3);
    place(book, 1, 2, OrderAction::Ask, 101, 1);
    place(book, 1, 3, OrderAction::Ask, 102, 2);
    place(book, 2, 4, OrderAction::Bid, 97, 7);
    place(book, 2, 5, OrderAction::Bid, 99, 9);
    place(book, 2, 6, OrderAction::Bid, 98, 8);

    let l2 = book.get_l2_data(2);
    assert_eq!(l2.ask_prices, vec![101, 102], "{name}: depth 应取最优两档卖盘");
    assert_eq!(l2.bid_prices, vec![99, 98], "{name}: depth 应取最优两档买盘");
}

#[test]
fn l2_ordering_direct() {
    check_l2("DirectOrderBook", &mut DirectOrderBook::new(spec()));
    check_l2_depth("DirectOrderBook", &mut DirectOrderBook::new(spec()));
}

#[test]
fn l2_ordering_naive() {
    check_l2("NaiveOrderBook", &mut NaiveOrderBook::new(spec()));
    check_l2_depth("NaiveOrderBook", &mut NaiveOrderBook::new(spec()));
}

#[test]
fn l2_ordering_direct_optimized() {
    check_l2(
        "DirectOrderBookOptimized",
        &mut DirectOrderBookOptimized::new(spec()),
    );
    check_l2_depth(
        "DirectOrderBookOptimized",
        &mut DirectOrderBookOptimized::new(spec()),
    );
}

#[test]
fn l2_ordering_advanced() {
    check_l2("AdvancedOrderBook", &mut AdvancedOrderBook::new(spec()));
    check_l2_depth("AdvancedOrderBook", &mut AdvancedOrderBook::new(spec()));
}
