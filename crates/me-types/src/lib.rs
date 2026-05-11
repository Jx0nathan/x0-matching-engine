//! Core types for the matching engine.
//!
//! Invariants enforced by this crate:
//! - Numeric types are i64-backed newtypes. Cross-type arithmetic (Price × Size)
//!   widens to i128 to make overflow impossible at the multiplication step.
//!   The caller decides where to truncate back to i64.
//! - Price, Size, Amount are not implicitly interchangeable. The type system
//!   should refuse `price + size` at compile time.
//! - Commands (input), Events (output), and Receipts (final outcome) are three
//!   separate types, not one mutable struct that flows through the pipeline.
//!   This is the deliberate departure from the legacy `OrderCommand` design.
//! - serde derives are present everywhere for snapshots. rkyv (for the WAL hot
//!   path) is added in M3 only on the subset of types that actually go through
//!   the journal.

pub mod ids;
pub mod numeric;
pub mod enums;
pub mod reject;
pub mod command;
pub mod event;
pub mod receipt;
pub mod symbol;
pub mod invariants;

pub use ids::*;
pub use numeric::*;
pub use enums::*;
pub use reject::*;
pub use command::*;
pub use event::*;
pub use receipt::*;
pub use symbol::*;
