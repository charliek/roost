use std::sync::atomic::{AtomicBool, Ordering};

use roost_ipc::session_launch;
use tokio::sync::Notify;

/// The disconnect signal, level-triggered.
///
/// A bare [`Notify`] is edge-triggered: `notify_waiters` on a task that
/// happens to be *between* two of its `select!` arms is lost, and the
/// only thing that ever stopped the task would be the abort the flush
/// contract now forbids. The flag is what makes the signal durable — a
/// task that misses the wake still sees it on its next check.
#[derive(Debug, Default)]
pub struct Shutdown {
    requested: AtomicBool,
    wake: Notify,
}

impl Shutdown {
    /// Ask the task to wind down. Idempotent.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        // Both calls, for two different waiters. A task keeps two —
        // its connection loop and its grace timer — and `notify_one`
        // alone wakes whichever parked first, leaving the other asleep
        // on a wake that never comes; the loop is the one that must
        // hear this, and it is the one that re-parks (and so ends up at
        // the back of the queue). `notify_waiters` wakes everyone
        // already parked, and `notify_one` then leaves a permit for a
        // waiter that had read the flag but not yet parked when this
        // ran.
        self.wake.notify_waiters();
        self.wake.notify_one();
    }

    /// Resolves once a disconnect has been asked for — however many
    /// times, and from however many places, it is awaited.
    ///
    /// The flag is read *before* parking, so a signal raised in the
    /// window between the two is seen rather than slept through.
    pub async fn requested(&self) {
        while !self.requested.load(Ordering::Acquire) {
            self.wake.notified().await;
        }
    }
}

/// The scale every budget in this module is stretched by, read once.
///
/// [`session_launch::timeout_scale`] reads the environment on each call,
/// and [`leg`] is per-op work on the control plane — the answer cannot
/// change while the process runs, so it is worth remembering.
pub fn scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(session_launch::timeout_scale)
}
