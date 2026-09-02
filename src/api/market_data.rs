use crate::api::*;

/// L2 市场深度数据
///
/// `*_order_counts` 是 L3 语义的补充：每一档除总量外还报该档的挂单笔数，
/// 下标与同侧的 `*_prices` / `*_volumes` 一一对应。
#[derive(Debug, Clone)]
pub struct L2MarketData {
    pub ask_prices: Vec<Price>,
    pub ask_volumes: Vec<Size>,
    pub ask_order_counts: Vec<usize>,
    pub bid_prices: Vec<Price>,
    pub bid_volumes: Vec<Size>,
    pub bid_order_counts: Vec<usize>,
}

impl L2MarketData {
    pub fn new(depth: usize) -> Self {
        Self {
            ask_prices: Vec::with_capacity(depth),
            ask_volumes: Vec::with_capacity(depth),
            ask_order_counts: Vec::with_capacity(depth),
            bid_prices: Vec::with_capacity(depth),
            bid_volumes: Vec::with_capacity(depth),
            bid_order_counts: Vec::with_capacity(depth),
        }
    }
}
