use serde::{Deserialize, Serialize};
use crate::{enums::*, ids::*, numeric::*, symbol::SymbolSpec};

/// All user-originated input to the engine. Distinct from Event (output) and
/// CommandReceipt (final outcome). Each variant carries only the fields it
/// actually needs — no single mutable god-struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    PlaceOrder(PlaceOrder),
    CancelOrder(CancelOrder),
    ModifyOrder(ModifyOrder),
    AddUser(AddUser),
    AdjustBalance(AdjustBalance),
    SuspendUser(UserId),
    ResumeUser(UserId),
    /// Register a new tradable symbol. Routed through the WAL so a restored
    /// engine reconstructs its symbol set without out-of-band config replay.
    RegisterSymbol(SymbolSpec),
    /// Update the engine's mark price for a derivative symbol. Triggers a
    /// liquidation sweep — positions whose margin ratio falls below MMR are
    /// force-closed at the new mark.
    SetMarkPrice(SetMarkPrice),
    /// Apply a perpetual funding settlement. Positive `rate_bps` → longs pay
    /// shorts; negative → shorts pay longs. Funding charge per position is
    /// `signed_size × mark × rate / scale / 10_000`.
    ApplyFunding(ApplyFunding),
    /// Settle and expire a futures symbol: force-close every open position
    /// on `symbol_id` at `settlement_price`, then suspend the symbol so no
    /// new orders are accepted.
    SettleFuture(SettleFuture),
    Nop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetMarkPrice {
    pub symbol_id: SymbolId,
    pub mark_price: Price,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyFunding {
    pub symbol_id: SymbolId,
    /// Funding rate in basis points (signed: +100 = longs pay shorts 1%).
    pub rate_bps: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleFuture {
    pub symbol_id: SymbolId,
    pub settlement_price: Price,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceOrder {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    /// None for Market orders (price discovered at match time).
    pub price: Option<Price>,
    pub size: Size,
    /// Upper-bound price for bid risk-check. None ⇒ use `price`.
    /// For asks this field is ignored.
    pub reserve_price: Option<Price>,
    /// Trigger price for StopLimit/StopMarket.
    pub stop_price: Option<Price>,
    /// Iceberg only. None ⇒ entire size is visible.
    pub visible_size: Option<Size>,
    pub self_trade_prevention: SelfTradePrevention,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelOrder {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub order_id: OrderId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModifyOrder {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub order_id: OrderId,
    pub new_price: Option<Price>,
    pub new_size: Option<Size>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddUser {
    pub user_id: UserId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjustBalance {
    pub user_id: UserId,
    pub currency_id: CurrencyId,
    /// Signed: positive = deposit-equivalent (external in), negative = withdrawal.
    pub delta: Amount,
    /// Idempotency key from the upstream system. Engine rejects duplicates.
    pub transaction_id: u64,
}

/// Envelope adding engine-assigned metadata. Constructed at the WAL boundary;
/// the inner `Command` is what the user submitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub seq_no: SeqNo,
    pub received_at: Timestamp,
    pub command: Command,
}
