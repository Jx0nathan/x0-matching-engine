//! Synchronous facade for the matching pipeline.
//!
//! M2 scope: single-threaded `submit(Command) -> CommandReceipt`. M3 will
//! introduce the Disruptor three-stage pipeline; this synchronous path remains
//! the reference semantics for replay and tests.
//!
//! The fund-leak guarantee enforced here:
//! - For every `Command::PlaceOrder` that passes `risk.pre_check_place`,
//!   `submit` always reaches one of: `risk.apply_trade` (for each fill)
//!   followed by `risk.release_hold` (for any non-resting remainder), OR
//!   `risk.release_hold` directly (for FOK/PostOnly rejection by the book).
//!   No code path drops a Hold silently.

pub mod engine;

pub use engine::*;
