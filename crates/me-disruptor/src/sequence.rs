use std::sync::atomic::{AtomicI64, Ordering};

/// A monotonic counter padded to 64 bytes to avoid false sharing with
/// adjacent atomics. Use one Sequence per producer or per consumer.
///
/// Convention: `-1` means "nothing yet" (no slot claimed / no event consumed).
#[repr(align(64))]
pub struct Sequence {
    value: AtomicI64,
    _padding: [u8; 56], // 64 - 8 bytes of AtomicI64
}

impl Sequence {
    pub const fn new(initial: i64) -> Self {
        Self {
            value: AtomicI64::new(initial),
            _padding: [0; 56],
        }
    }

    /// Acquire load — pairs with `set`'s Release store.
    #[inline]
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Acquire)
    }

    /// Release store — makes prior writes visible to any thread that
    /// subsequently observes this sequence via `get`.
    #[inline]
    pub fn set(&self, val: i64) {
        self.value.store(val, Ordering::Release);
    }

    /// Compare-and-swap. Used by multi-producer claim (not currently used —
    /// we're single-producer in M3.2 — but kept for the M5 multi-shard path).
    #[inline]
    pub fn compare_exchange(&self, expected: i64, new: i64) -> Result<i64, i64> {
        self.value
            .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)
    }
}

impl Default for Sequence {
    fn default() -> Self {
        Self::new(-1)
    }
}

impl std::fmt::Debug for Sequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sequence")
            .field("value", &self.get())
            .finish()
    }
}
