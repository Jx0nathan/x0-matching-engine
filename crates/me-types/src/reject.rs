use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Error)]
pub enum RejectReason {
    #[error("user does not exist")]
    UnknownUser,
    #[error("symbol does not exist")]
    UnknownSymbol,
    #[error("order id does not exist")]
    UnknownOrder,
    #[error("symbol is suspended")]
    SymbolSuspended,
    #[error("user is suspended")]
    UserSuspended,

    #[error("insufficient funds")]
    InsufficientFunds,
    #[error("insufficient margin")]
    InsufficientMargin,
    #[error("position limit exceeded")]
    PositionLimitExceeded,
    #[error("invalid reserve price for bid")]
    InvalidReservePrice,
    #[error("price outside allowed band")]
    PriceBandViolation,
    #[error("order size below minimum")]
    SizeBelowMinimum,
    #[error("order size above maximum")]
    SizeAboveMaximum,
    #[error("price not aligned to tick size")]
    PriceTickMisaligned,
    #[error("size not aligned to lot size")]
    SizeLotMisaligned,
    #[error("missing required price for order type")]
    MissingPrice,

    #[error("post-only order would take liquidity")]
    PostOnlyWouldCross,
    #[error("FOK order cannot be fully filled")]
    FokUnfillable,
    #[error("self-trade prevented")]
    SelfTradePrevented,
    #[error("stop price not triggered")]
    StopNotTriggered,
    #[error("order has expired")]
    Expired,
    #[error("invalid order book state")]
    InvalidOrderBookState,

    #[error("command type not supported in this context")]
    UnsupportedCommand,
    #[error("arithmetic overflow")]
    ArithmeticOverflow,
    #[error("serialization failed")]
    SerializationFailed,
}
