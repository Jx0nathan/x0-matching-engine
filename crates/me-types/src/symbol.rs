use serde::{Deserialize, Serialize};
use crate::{ids::*, numeric::*};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSpec {
    pub symbol_id: SymbolId,
    pub base_currency: CurrencyId,
    pub quote_currency: CurrencyId,

    pub base_minor_per_major: i64,
    pub quote_minor_per_major: i64,

    pub tick_size: Price,
    pub lot_size: Size,
    pub min_order_size: Size,
    pub max_order_size: Size,

    pub fee_schedule: FeeSchedule,
    pub price_band: PriceBand,

    /// Spot vs derivative parameters. The kind-specific data lives here rather
    /// than in a parallel enum so SymbolSpec stays a single value to pass around.
    pub kind_params: SymbolKindParams,

    pub is_suspended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SymbolKindParams {
    Spot,
    PerpetualSwap(PerpParams),
    Future(FutureParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerpParams {
    pub initial_margin_bps: Bps,
    pub maintenance_margin_bps: Bps,
    pub funding_interval_secs: i64,
    pub max_leverage: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FutureParams {
    pub expiry: Timestamp,
    pub initial_margin_bps: Bps,
    pub maintenance_margin_bps: Bps,
    /// Usually equal to quote_currency, but not always (e.g., inverse contracts).
    pub settlement_currency: CurrencyId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeSchedule {
    pub maker_bps: Bps,
    pub taker_bps: Bps,
}

/// Reject orders whose price drifts more than `upper_bps_from_ref` above or
/// `lower_bps_from_ref` below `reference_price`. Any `None` field disables that side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceBand {
    pub upper_bps_from_ref: Option<Bps>,
    pub lower_bps_from_ref: Option<Bps>,
    pub reference_price: Option<Price>,
}

impl PriceBand {
    pub const fn none() -> Self {
        Self {
            upper_bps_from_ref: None,
            lower_bps_from_ref: None,
            reference_price: None,
        }
    }
}
