//! Matching engine: spot order books only in M2.
//!
//! Derivatives (margin, perp/future contracts) land in M4. The trait surface
//! here is intentionally narrow: place / cancel / match. No global state, no
//! account access — risk lives in `me-risk`, money never moves here.
//!
//! Time-in-force coverage for M2:
//! - Gtc, PostOnly: rest if any remainder
//! - Ioc: fill what you can, drop the rest
//! - Fok: pre-check full fillability, all-or-nothing
//! - Day, Gtd: treated as Gtc for now (expiry logic in M4 alongside funding)

pub mod book;

pub use book::*;
