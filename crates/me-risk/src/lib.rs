//! Risk engine: balance management with paired hold ↔ settlement.
//!
//! Design contract — this is the fund-leak fix from the legacy design:
//! - Every successful `pre_check_place` registers exactly one `Hold` keyed
//!   by `OrderId`.
//! - Every `Hold` is removed by exactly one of: `apply_trade` (when its
//!   remaining size reaches zero) or `release_hold` (explicit return-to-free).
//! - There is no other code path that touches `holds`. If the matching
//!   engine returns without producing fills AND without rebound through
//!   `release_hold`, money would leak — so the facade (me-core) must always
//!   call one of these terminators.

pub mod account;
pub mod engine;

pub use account::*;
pub use engine::*;
