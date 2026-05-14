use std::cell::UnsafeCell;
use std::sync::Arc;

use crate::Sequence;

/// Single-producer ring buffer. Capacity must be a power of two.
///
/// Slots are pre-allocated with `T::default()` and reused. A slot at index
/// `seq & mask` belongs to whoever's currently authorized for `seq`:
/// - The producer has exclusive write access to `seq` before it calls
///   `publish(seq)`.
/// - After publish, the slot is read-only and stable until *every* registered
///   gating consumer has advanced past `seq`. Once that happens the producer
///   may reclaim it for `seq + capacity`.
///
/// Safety contract: caller must use `next_seq` / `slot_mut` / `publish` in
/// that order, single-threaded on the producer side. Consumers obtain
/// read-only access via `slot` after observing `cursor() >= seq`.
pub struct RingBuffer<T> {
    buffer: Box<[UnsafeCell<T>]>,
    capacity: usize,
    mask: usize,
    /// Producer cursor — highest sequence successfully published. Consumers
    /// wait on this. Starts at -1.
    cursor: Arc<Sequence>,
    /// Consumer sequences that gate the producer (it cannot wrap past the
    /// slowest gating consumer). Populated via `add_gating`.
    gating: Vec<Arc<Sequence>>,
}

// SAFETY: `UnsafeCell<T>` is not Sync but RingBuffer enforces the access
// discipline described above. Producer/consumer roles are externally separated
// by `next_seq` / `slot_mut` / `publish` / `slot` API contract.
unsafe impl<T: Send> Send for RingBuffer<T> {}
unsafe impl<T: Send> Sync for RingBuffer<T> {}

impl<T: Default> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two() && capacity > 0,
            "ring buffer capacity must be a power of two > 0; got {capacity}"
        );
        let mut buf = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            buf.push(UnsafeCell::new(T::default()));
        }
        Self {
            buffer: buf.into_boxed_slice(),
            capacity,
            mask: capacity - 1,
            cursor: Arc::new(Sequence::new(-1)),
            gating: Vec::new(),
        }
    }
}

impl<T> RingBuffer<T> {
    /// Capacity (number of slots). Power of two.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Last sequence the producer published. Consumers wait on this.
    pub fn cursor(&self) -> &Arc<Sequence> {
        &self.cursor
    }

    /// Register a consumer's sequence to gate the producer.
    ///
    /// Must be called before any commands are published. Adding gating
    /// consumers after the ring is in motion would let the producer overrun
    /// them.
    pub fn add_gating(&mut self, seq: Arc<Sequence>) {
        self.gating.push(seq);
    }

    /// Block until claiming sequence `current + 1` is safe (i.e., no gating
    /// consumer is more than `capacity - 1` behind). Returns the claimed
    /// sequence. Single-producer — caller must externally serialize.
    pub fn next_seq(&self, current: i64) -> i64 {
        let next = current + 1;
        let wrap_point = next - self.capacity as i64;
        if wrap_point >= 0 {
            loop {
                let min = self
                    .gating
                    .iter()
                    .map(|s| s.get())
                    .min()
                    .unwrap_or(i64::MAX);
                if min >= wrap_point {
                    break;
                }
                std::hint::spin_loop();
            }
        }
        next
    }

    /// Producer-side exclusive mut access to the slot for `seq`. Must be
    /// called only AFTER `next_seq` returned `seq` and BEFORE `publish(seq)`.
    ///
    /// # Safety
    /// Caller must ensure exclusive access (single producer, slot not yet
    /// published).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slot_mut(&self, seq: i64) -> &mut T {
        let idx = (seq as usize) & self.mask;
        &mut *self.buffer[idx].get()
    }

    /// Read-only access to slot for `seq`. Caller must have already
    /// observed `cursor() >= seq` to guarantee the slot is published, and
    /// the consumer's gating sequence must be `< seq` (otherwise producer
    /// could be writing the next lap).
    ///
    /// # Safety
    /// See above.
    pub unsafe fn slot(&self, seq: i64) -> &T {
        let idx = (seq as usize) & self.mask;
        &*self.buffer[idx].get()
    }

    /// Publish `seq` to consumers. Must be called in strict ascending order
    /// (single producer; no holes).
    pub fn publish(&self, seq: i64) {
        self.cursor.set(seq);
    }
}

impl<T> std::fmt::Debug for RingBuffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingBuffer")
            .field("capacity", &self.capacity)
            .field("cursor", &self.cursor.get())
            .field("gating_count", &self.gating.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use crate::{BusySpinStrategy, WaitStrategy, YieldingStrategy};

    #[test]
    #[should_panic(expected = "must be a power of two")]
    fn non_power_of_two_panics() {
        let _: RingBuffer<u64> = RingBuffer::new(7);
    }

    #[test]
    fn single_producer_single_consumer_roundtrip() {
        let mut ring: RingBuffer<u64> = RingBuffer::new(8);
        let consumer_seq = Arc::new(Sequence::new(-1));
        ring.add_gating(consumer_seq.clone());
        let ring = Arc::new(ring);

        let producer_ring = ring.clone();
        let producer = thread::spawn(move || {
            let mut cur = -1i64;
            for i in 0..100u64 {
                let seq = producer_ring.next_seq(cur);
                cur = seq;
                unsafe {
                    *producer_ring.slot_mut(seq) = i * 10;
                }
                producer_ring.publish(seq);
            }
        });

        let cursor = ring.cursor().clone();
        let strategy = YieldingStrategy;
        let mut my_seq = -1i64;
        let mut collected = Vec::new();
        while collected.len() < 100 {
            let avail = strategy.wait_for(my_seq + 1, &cursor);
            for s in (my_seq + 1)..=avail {
                let v = unsafe { *ring.slot(s) };
                collected.push(v);
            }
            my_seq = avail;
            consumer_seq.set(avail);
        }

        producer.join().unwrap();
        assert_eq!(collected.len(), 100);
        for (i, v) in collected.iter().enumerate() {
            assert_eq!(*v, (i as u64) * 10);
        }
    }

    #[test]
    fn backpressure_blocks_producer_until_consumer_catches_up() {
        let mut ring: RingBuffer<u64> = RingBuffer::new(4);
        let consumer_seq = Arc::new(Sequence::new(-1));
        ring.add_gating(consumer_seq.clone());
        let ring = Arc::new(ring);

        // Fill the ring without advancing the consumer.
        let mut cur = -1i64;
        for i in 0..4u64 {
            let seq = ring.next_seq(cur);
            cur = seq;
            unsafe {
                *ring.slot_mut(seq) = i;
            }
            ring.publish(seq);
        }

        // Now attempt to claim a 5th slot in a producer thread. Should block
        // until we advance the consumer.
        let producer_ring = ring.clone();
        let producer_done = Arc::new(AtomicUsize::new(0));
        let pd = producer_done.clone();
        let cur_for_thread = cur;
        let handle = thread::spawn(move || {
            let _seq = producer_ring.next_seq(cur_for_thread);
            pd.store(1, Ordering::Release);
        });

        // Give the producer a moment to confirm it's blocked.
        thread::sleep(Duration::from_millis(50));
        assert_eq!(producer_done.load(Ordering::Acquire), 0);

        // Advance the consumer by 1 — producer should now unblock.
        consumer_seq.set(0);
        handle.join().unwrap();
        assert_eq!(producer_done.load(Ordering::Acquire), 1);
    }

    #[test]
    fn busy_spin_strategy_returns_immediately_when_satisfied() {
        let seq = Sequence::new(5);
        let strategy = BusySpinStrategy;
        let available = strategy.wait_for(3, &seq);
        assert_eq!(available, 5);
    }

    #[test]
    fn yielding_strategy_returns_immediately_when_satisfied() {
        let seq = Sequence::new(5);
        let strategy = YieldingStrategy;
        let available = strategy.wait_for(3, &seq);
        assert_eq!(available, 5);
    }
}
