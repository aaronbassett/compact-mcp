use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::CoreError;

/// Builds queue on a semaphore; only an over-full queue rejects. Parallel
/// proving-key generation thrashes memory, so `max_concurrent` defaults to 1.
pub struct BuildGate {
    sem: Arc<Semaphore>,
    queued: AtomicUsize,
    max_queued: usize,
}

impl BuildGate {
    pub fn new(max_concurrent: usize, max_queued: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            queued: AtomicUsize::new(0),
            max_queued,
        }
    }

    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, CoreError> {
        // Fast path: a free permit means we never touch the queue counter.
        if let Ok(p) = self.sem.clone().try_acquire_owned() {
            return Ok(p);
        }
        if self.queued.fetch_add(1, Ordering::SeqCst) >= self.max_queued {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            return Err(CoreError::QueueFull(self.max_queued));
        }
        // RAII: decrement the queue counter on EVERY exit from here on — normal
        // completion, the error map below, AND cancellation (the enclosing
        // future being dropped while parked in `acquire_owned().await`). A plain
        // post-`.await` `fetch_sub` is skipped on cancellation, leaking a queue
        // slot permanently (cancelling queued builds is routine). The guard is
        // constructed only AFTER the admission check so the reject path's own
        // `fetch_sub` is never double-counted.
        struct QueuedGuard<'a>(&'a AtomicUsize);
        impl Drop for QueuedGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _guard = QueuedGuard(&self.queued);

        let permit = self.sem.clone().acquire_owned().await;
        // The only `Err` is a CLOSED semaphore; this gate never closes it, so
        // this arm is currently unreachable. (If graceful shutdown ever calls
        // `Semaphore::close`, revisit — "queue full" would misdescribe it.)
        permit.map_err(|_| CoreError::QueueFull(self.max_queued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait deterministically until a spawned waiter has actually entered the
    /// queue (incremented the counter and parked in `acquire_owned`), rather
    /// than guessing with a fixed sleep that can race — or, worse, let the main
    /// task itself block on `acquire` before the waiter registered.
    async fn wait_until_queued(g: &BuildGate, n: usize) {
        while g.queued.load(Ordering::SeqCst) < n {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_second_build_queues_rather_than_failing() {
        let g = std::sync::Arc::new(BuildGate::new(1, 4));
        let held = g.acquire().await.unwrap();

        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire().await.map(|_| ()) });
        wait_until_queued(&g, 1).await;
        assert!(
            !waiter.is_finished(),
            "the second build must queue, not fail"
        );

        drop(held);
        waiter.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn only_an_overfull_queue_rejects() {
        let g = std::sync::Arc::new(BuildGate::new(1, 1));
        let _held = g.acquire().await.unwrap();

        let g2 = g.clone();
        let queued = tokio::spawn(async move { g2.acquire().await.map(|_| ()) });
        wait_until_queued(&g, 1).await;

        // one running + one queued == capacity; the next must fail fast.
        let err = g.acquire().await.unwrap_err();
        assert!(matches!(err, CoreError::QueueFull(1)));

        queued.abort();
    }

    #[tokio::test]
    async fn cancelling_a_queued_waiter_does_not_leak_the_queue_counter() {
        // Cancelling a build while it is parked in the queue must return its
        // slot. Without the RAII guard the post-`.await` `fetch_sub` is skipped
        // on drop and the counter leaks upward until the gate wrongly rejects
        // every build with QueueFull forever.
        let g = std::sync::Arc::new(BuildGate::new(1, 4));
        let held = g.acquire().await.unwrap(); // occupy the only concurrency slot

        let g2 = g.clone();
        let queued = tokio::spawn(async move { g2.acquire().await.map(|_| ()) });
        wait_until_queued(&g, 1).await;
        assert_eq!(g.queued.load(Ordering::SeqCst), 1);

        queued.abort();
        assert!(
            queued.await.unwrap_err().is_cancelled(),
            "the waiter future must actually have been dropped"
        );
        assert_eq!(
            g.queued.load(Ordering::SeqCst),
            0,
            "cancelling a queued waiter must return its slot"
        );

        // Liveness belt (the counter assertion above is the actual leak check):
        // with the slot freed, the gate is still usable end-to-end.
        drop(held);
        let _permit = g.acquire().await.unwrap();
    }
}
