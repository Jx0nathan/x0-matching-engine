use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct UserId(pub u64);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct OrderId(pub u64);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct SymbolId(pub u32);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct CurrencyId(pub u32);

/// Client-supplied request id, opaque to the engine.
/// Used to correlate rejections back to a submission without an engine-assigned OrderId.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(transparent)]
#[serde(transparent)]
pub struct ClientOrderId(pub u64);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct SeqNo(pub u64);

/// Engine-side timestamp in nanoseconds since the UNIX epoch.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Timestamp(pub i64);
