use crate::api::*;
use serde::{Deserialize, Serialize};
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

/// 撮合事件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum MatcherEventType {
    Trade,      // 成交
    Reject,     // 拒绝
    Reduce,     // 减少
}

/// 撮合事件
#[derive(Debug, Clone, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MatcherTradeEvent {
    pub event_type: MatcherEventType,
    pub size: Size,
    pub price: Price,
    pub matched_order_id: OrderId,
    pub matched_order_uid: UserId,
    pub bidder_hold_price: Price, // 买单预留价格
}

impl Default for MatcherTradeEvent {
    fn default() -> Self {
        Self {
            event_type: MatcherEventType::Trade,
            size: 0,
            price: 0,
            matched_order_id: 0,
            matched_order_uid: 0,
            bidder_hold_price: 0,
        }
    }
}

impl MatcherTradeEvent {
    pub fn new_trade(
        size: Size,
        price: Price,
        matched_order_id: OrderId,
        matched_order_uid: UserId,
        bidder_hold_price: Price,
    ) -> Self {
        Self {
            event_type: MatcherEventType::Trade,
            size,
            price,
            matched_order_id,
            matched_order_uid,
            bidder_hold_price,
        }
    }

    /// 构造拒绝/撤销事件。
    ///
    /// `bidder_hold_price` 必须是下单时实际冻结资金所用的价格（买单的 reserve_price），
    /// R2 结算依赖它计算退款金额；传 0 会导致买单冻结的资金无法归还。
    pub fn new_reject(size: Size, price: Price, bidder_hold_price: Price) -> Self {
        Self {
            event_type: MatcherEventType::Reject,
            size,
            price,
            matched_order_id: 0,
            matched_order_uid: 0,
            bidder_hold_price,
        }
    }
}
