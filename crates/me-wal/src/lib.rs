//! Write-ahead log + snapshot store.
//!
//! M3.1 scope: synchronous WAL (every append fsyncs) + bincode-framed
//! records. Group commit batching and CRC checksums land in M3.3 once the
//! Disruptor pipeline (M3.2) exposes the natural batching boundary.
//!
//! Recovery contract:
//! - Snapshot files are named `snapshot_{seq}.bin` where `seq` is the
//!   `last_applied_seq` at the time of saving.
//! - WAL files are append-only journals of `CommandEnvelope`.
//! - On startup: load the highest-numbered snapshot, then replay every WAL
//!   record with `seq_no > snapshot.last_applied_seq`.
//! - This means the WAL is NOT truncated on snapshot — it just grows. WAL
//!   truncation is an M5 concern; for now keep the audit trail intact.

pub mod journal;
pub mod snapshot;

pub use journal::*;
pub use snapshot::*;
