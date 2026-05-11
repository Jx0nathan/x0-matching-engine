use crate::Sequence;

/// Strategy for how a consumer waits on a dependency sequence.
pub trait WaitStrategy: Send + Sync + Clone + 'static {
    /// Block until `dependency >= target`, return the actual available
    /// sequence (which may be larger if the producer batches).
    fn wait_for(&self, target: i64, dependency: &Sequence) -> i64;
}

/// Lowest latency. Spins indefinitely on the dependency. Pins one core to 100%.
/// Use for ultra-low-latency single-tenant deployments.
#[derive(Debug, Clone, Default)]
pub struct BusySpinStrategy;

impl WaitStrategy for BusySpinStrategy {
    #[inline]
    fn wait_for(&self, target: i64, dependency: &Sequence) -> i64 {
        loop {
            let available = dependency.get();
            if available >= target {
                return available;
            }
            std::hint::spin_loop();
        }
    }
}

/// Spins with `yield_now()`. Polite to other threads on the same core but
/// adds wakeup latency. Better default for shared hosts and test runs.
#[derive(Debug, Clone, Default)]
pub struct YieldingStrategy;

impl WaitStrategy for YieldingStrategy {
    fn wait_for(&self, target: i64, dependency: &Sequence) -> i64 {
        loop {
            let available = dependency.get();
            if available >= target {
                return available;
            }
            std::thread::yield_now();
        }
    }
}
