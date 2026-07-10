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
        let permit = self.sem.clone().acquire_owned().await;
        self.queued.fetch_sub(1, Ordering::SeqCst);
        permit.map_err(|_| CoreError::QueueFull(self.max_queued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_build_queues_rather_than_failing() {
        let g = std::sync::Arc::new(BuildGate::new(1, 4));
        let held = g.acquire().await.unwrap();

        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire().await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // one running + one queued == capacity; the next must fail fast.
        let err = g.acquire().await.unwrap_err();
        assert!(matches!(err, CoreError::QueueFull(1)));

        queued.abort();
    }
}
