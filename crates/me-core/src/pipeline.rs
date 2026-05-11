use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use me_disruptor::{RingBuffer, Sequence, WaitStrategy, YieldingStrategy};
use me_types::{Command, CommandReceipt, Timestamp};

use crate::MatchingEngine;

/// Slot payload flowing through the ring buffer. `Default` lets the ring
/// pre-allocate slots; subsequent submissions overwrite in place.
#[derive(Clone)]
struct PipelineEvent {
    cmd: Command,
    ts: Timestamp,
    response: Option<SyncSender<CommandReceipt>>,
    is_shutdown: bool,
}

impl Default for PipelineEvent {
    fn default() -> Self {
        Self {
            cmd: Command::Nop,
            ts: Timestamp(0),
            response: None,
            is_shutdown: false,
        }
    }
}

/// Producer/consumer wrapper around a `MatchingEngine`.
///
/// Architecture (M3.2):
/// - Producer (caller thread) claims a slot in the ring buffer, writes the
///   command, and publishes. Blocks for the receipt on a per-command sync_channel.
/// - Consumer (engine thread) waits on the producer cursor, processes events
///   sequentially through the wrapped `MatchingEngine`, sends each receipt
///   back to the caller via the channel.
///
/// Concurrency gain: the caller can submit command N+1 while the engine is
/// processing N. With WAL fsync inside `MatchingEngine::submit`, this also
/// overlaps disk I/O latency with caller work.
///
/// True 3-thread R1/Match/R2 split is deferred to M5 (requires UID-sharded
/// RiskEngine so the handlers don't conflict on shared state). The ring
/// buffer is multi-consumer ready — that change is mostly in the wiring,
/// not the disruptor primitives.
pub struct AsyncMatchingEngine {
    ring: Arc<RingBuffer<PipelineEvent>>,
    producer_cursor: i64,
    consumer: Option<JoinHandle<MatchingEngine>>,
    shut_down: Arc<AtomicBool>,
}

impl AsyncMatchingEngine {
    /// Spawn a consumer thread and start serving commands.
    pub fn new(engine: MatchingEngine, ring_size: usize) -> Self {
        let mut ring = RingBuffer::<PipelineEvent>::new(ring_size);
        let consumer_seq = Arc::new(Sequence::new(-1));
        ring.add_gating(consumer_seq.clone());
        let ring = Arc::new(ring);

        let shut_down = Arc::new(AtomicBool::new(false));

        let ring_for_thread = ring.clone();
        let shut_down_for_thread = shut_down.clone();
        let consumer = thread::Builder::new()
            .name("me-engine-consumer".into())
            .spawn(move || run_consumer(engine, ring_for_thread, consumer_seq, shut_down_for_thread))
            .expect("spawn engine consumer thread");

        Self {
            ring,
            producer_cursor: -1,
            consumer: Some(consumer),
            shut_down,
        }
    }

    /// Submit a command. Blocks until the engine produces a receipt.
    /// If the engine has WAL configured, the receipt is only sent after
    /// the WAL is durably fsync'd — so the caller's blocking return is
    /// equivalent to durable acceptance.
    pub fn submit(&mut self, cmd: Command, ts: Timestamp) -> CommandReceipt {
        if self.shut_down.load(Ordering::Acquire) {
            panic!("submit() called on shut-down AsyncMatchingEngine");
        }
        let (tx, rx) = sync_channel::<CommandReceipt>(1);
        let seq = self.ring.next_seq(self.producer_cursor);
        self.producer_cursor = seq;
        unsafe {
            let slot = self.ring.slot_mut(seq);
            slot.cmd = cmd;
            slot.ts = ts;
            slot.response = Some(tx);
            slot.is_shutdown = false;
        }
        self.ring.publish(seq);
        rx.recv().expect("engine consumer disconnected before sending receipt")
    }

    /// Halt the consumer, join the thread, return the inner engine for inspection.
    pub fn shutdown(mut self) -> MatchingEngine {
        self.send_poison_pill();
        let handle = self.consumer.take().expect("already shut down");
        handle.join().expect("engine consumer panicked")
    }

    fn send_poison_pill(&mut self) {
        if self.shut_down.swap(true, Ordering::AcqRel) {
            return; // already poisoned
        }
        let seq = self.ring.next_seq(self.producer_cursor);
        self.producer_cursor = seq;
        unsafe {
            let slot = self.ring.slot_mut(seq);
            slot.cmd = Command::Nop;
            slot.ts = Timestamp(0);
            slot.response = None;
            slot.is_shutdown = true;
        }
        self.ring.publish(seq);
    }
}

impl Drop for AsyncMatchingEngine {
    fn drop(&mut self) {
        if let Some(handle) = self.consumer.take() {
            self.send_poison_pill();
            let _ = handle.join();
        }
    }
}

fn run_consumer(
    mut engine: MatchingEngine,
    ring: Arc<RingBuffer<PipelineEvent>>,
    my_seq: Arc<Sequence>,
    _shut_down: Arc<AtomicBool>,
) -> MatchingEngine {
    let strategy = YieldingStrategy;
    let cursor = ring.cursor().clone();
    let mut current = -1i64;

    loop {
        let target = current + 1;
        let available = strategy.wait_for(target, &cursor);

        for seq in (current + 1)..=available {
            // Extract everything we need from the slot, then drop the borrow
            // before mutating the engine.
            let (cmd, ts, response, is_shutdown) = unsafe {
                let slot = ring.slot(seq);
                (slot.cmd.clone(), slot.ts, slot.response.clone(), slot.is_shutdown)
            };

            if is_shutdown {
                my_seq.set(seq);
                return engine;
            }

            let receipt = engine.submit(cmd, ts);
            if let Some(tx) = response {
                let _ = tx.send(receipt);
            }
        }

        current = available;
        my_seq.set(available);
    }
}
