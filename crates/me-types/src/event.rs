use crate::{enums::*, ids::*, numeric::*, reject::*};
use serde::{Deserialize, Serialize};

/// Events emitted as the pipeline processes a Command. Consumed by the risk
/// engine for settlement and by external subscribers (market data, audit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    OrderAccepted(OrderAccepted),
    OrderRejected(OrderRejected),
    OrderCancelled(OrderCancelled),
    OrderFilled(OrderFilled),
    OrderPartiallyFilled(OrderPartiallyFilled),
    Trade(Trade),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub symbol_id: SymbolId,
    pub price: Price,
    pub size: Size,
    pub taker_order_id: OrderId,
    pub taker_user_id: UserId,
    pub maker_order_id: OrderId,
    pub maker_user_id: UserId,
    pub taker_side: Side,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderAccepted {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRejected {
    pub client_order_id: ClientOrderId,
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub reason: RejectReason,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderCancelled {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub remaining_size: Size,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderFilled {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub filled_size: Size,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPartiallyFilled {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub filled_size: Size,
    pub remaining_size: Size,
    pub timestamp: Timestamp,
}
