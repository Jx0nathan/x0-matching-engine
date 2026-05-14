//! Minimal LMAX-style disruptor primitives.
//!
//! Scope (M3.2):
//! - Cache-padded `Sequence` (atomic i64 aligned to 64B).
//! - Single-producer ring buffer with power-of-two capacity and gating-sequence
//!   backpressure (producer blocks when slowest consumer falls a full lap behind).
//! - BusySpin and Yielding wait strategies.
//!
//! Not yet here (M5):
//! - True 3-handler R1/Match/R2 split. That requires UID-sharded risk state so
//!   handlers can write concurrently without touching the same account. The
//!   consumer-thread plumbing here generalises to that, but the matching
//!   engine in me-core currently runs all three logical stages on a single
//!   consumer thread because RiskEngine is unsharded.

pub mod ring;
pub mod sequence;
pub mod wait;

pub use ring::RingBuffer;
pub use sequence::Sequence;
pub use wait::{BusySpinStrategy, WaitStrategy, YieldingStrategy};
