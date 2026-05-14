//! Synchronous and async facades for the matching pipeline.
//!
//! - `MatchingEngine` (engine.rs): single-threaded `submit(Command) → CommandReceipt`.
//!   This is the reference semantics for replay, tests, and embedding.
//! - `AsyncMatchingEngine` (pipeline.rs, M3.2): producer/consumer split via
//!   a lock-free ring buffer. Caller submits on its own thread; an engine
//!   thread drains the ring and runs the inner `MatchingEngine`. Receipts
//!   come back through per-command channels.
//!
//! The fund-leak guarantee enforced by the inner engine:
//! - For every `Command::PlaceOrder` that passes `risk.pre_check_place`,
//!   `submit` always reaches one of: `risk.apply_trade` (for each fill)
//!   followed by `risk.release_hold` (for any non-resting remainder), OR
//!   `risk.release_hold` directly (for FOK/PostOnly rejection by the book).
//!   No code path drops a Hold silently.

pub mod engine;
pub mod metrics;
pub mod pipeline;

pub use engine::*;
pub use metrics::*;
pub use pipeline::*;
