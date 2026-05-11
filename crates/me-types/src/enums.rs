use serde::{Deserialize, Serialize};
use crate::ids::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderType {
    Limit,
    Market,
    StopLimit,
    StopMarket,
    Iceberg,
}

/// Separated from OrderType because PostOnly / Gtc / Ioc / Fok are orthogonal
/// to Limit / Market / Stop / Iceberg. The legacy design conflated the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
    Day,
    Gtd(Timestamp),
    /// Conventionally a flag but commonly treated as a TIF in venue APIs.
    PostOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SelfTradePrevention {
    /// Permit self-trades. Use only for backwards compat or test scenarios.
    None,
    CancelTaker,
    CancelMaker,
    CancelBoth,
    DecrementAndCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Spot,
    PerpetualSwap,
    Future,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PositionSide {
    Long,
    Short,
    Flat,
}
