use std::{
    future::Future,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rand::{rngs::OsRng, Rng};
use serde::Serialize;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::error::{Result, UpstreamError};

/// Limits applied to calls that hit the rate-limited upstream API.
#[derive(Debug, Clone)]
pub struct FlowOptions {
    /// Calls allowed to run at once.
    pub concurrency: usize,
    /// Minimum gap between call starts.
    pub min_interval: Duration,
    /// Random extra delay added to each start, up to this value.
    pub jitter: Duration,
    /// Longest wait for a free slot.
    pub queue_timeout: Duration,
    /// Retries the caller performs (reported only).
    pub retries: usize,
    /// Base delay between retries (reported only).
    pub retry_delay: Duration,
}

/// Counters kept by the limiter.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FlowStats {
    /// Calls queued.
    pub enqueued: u64,
    /// Calls started.
    pub started: u64,
    /// Calls that returned `Ok`.
    pub completed: u64,
    /// Calls that returned `Err`.
    pub failed: u64,
    /// Calls that gave up waiting for a slot.
    pub timed_out: u64,
    /// Deepest queue seen.
    pub max_queue_depth: usize,
    /// Wall clock of the latest start, in ms since the epoch.
    pub last_started_at: u128,
    /// Wall clock of the latest scheduling decision.
    pub last_scheduled_at: u128,
    /// Wall clock of the latest success.
    pub last_completed_at: u128,
    /// Wall clock of the latest failure.
    pub last_error_at: u128,
    /// Start of the latest error text.
    pub last_error: String,
    /// Total time spent waiting in the queue.
    pub total_wait_ms: u128,
    /// Total time spent running calls.
    pub total_run_ms: u128,
}

/// Point-in-time view of the limiter, as shown by `/health`.
#[derive(Debug, Serialize)]
pub struct FlowSnapshot {
    /// Configured concurrency.
    pub concurrency: usize,
    /// Calls running now.
    pub active: usize,
    /// Calls waiting now.
    pub queued: usize,
    /// Configured minimum gap in ms.
    pub min_interval_ms: u64,
    /// Configured jitter in ms.
    pub jitter_ms: u64,
    /// Configured queue timeout in ms.
    pub queue_timeout_ms: u64,
    /// Configured retries.
    pub retries: usize,
    /// Configured retry delay in ms.
    pub retry_delay_ms: u64,
    /// Mean queue wait.
    pub average_wait_ms: u128,
    /// Mean run time.
    pub average_run_ms: u128,
    /// All counters.
    #[serde(flatten)]
    pub stats: FlowStats,
}

struct Book {
    next_start_at: Instant,
    stats: FlowStats,
    queued: usize,
}

struct Inner {
    options: FlowOptions,
    semaphore: Arc<Semaphore>,
    book: Mutex<Book>,
}

impl Inner {
    /// Bookkeeping is tiny and never held across an await.
    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Spaces out calls to the upstream API and bounds how many run at once.
#[derive(Clone)]
pub struct ApiFlowLimiter {
    inner: Arc<Inner>,
}

impl ApiFlowLimiter {
    /// Creates a limiter; a concurrency of zero is treated as one.
    pub fn new(options: FlowOptions) -> Self {
        let concurrency = options.concurrency.max(1);
        Self {
            inner: Arc::new(Inner {
                options,
                semaphore: Arc::new(Semaphore::new(concurrency)),
                book: Mutex::new(Book {
                    next_start_at: Instant::now(),
                    stats: FlowStats::default(),
                    queued: 0,
                }),
            }),
        }
    }

    /// Runs `task` once a slot is free and the start schedule allows it.
    ///
    /// # Errors
    /// Returns [`UpstreamError::FlowQueueTimeout`] when no slot frees up within the
    /// queue timeout, [`UpstreamError::FlowClosed`] when the limiter is shut down, or
    /// the task's own error.
    pub async fn run<F, T>(&self, task: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send,
        T: Send,
    {
        let enqueued_at = Instant::now();
        {
            let mut book = self.inner.book();
            book.queued += 1;
            book.stats.enqueued += 1;
            book.stats.max_queue_depth = book.stats.max_queue_depth.max(book.queued);
        }

        let acquire = Arc::clone(&self.inner.semaphore).acquire_owned();
        let permit =
            match tokio::time::timeout(self.inner.options.queue_timeout, acquire).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => {
                    self.inner.book().queued -= 1;
                    return Err(UpstreamError::FlowClosed);
                }
                Err(_) => {
                    let mut book = self.inner.book();
                    book.queued = book.queued.saturating_sub(1);
                    book.stats.timed_out += 1;
                    warn!(
                        waited_ms = enqueued_at.elapsed().as_millis(),
                        queued = book.queued,
                        timed_out = book.stats.timed_out,
                        "upstream call gave up waiting for a free slot"
                    );
                    return Err(UpstreamError::FlowQueueTimeout {
                        waited_ms: enqueued_at.elapsed().as_millis(),
                    });
                }
            };

        let scheduled_at = {
            let mut book = self.inner.book();
            book.queued = book.queued.saturating_sub(1);
            let now = Instant::now();
            let jitter = self.inner.options.jitter;
            let jitter_ms = if jitter.is_zero() {
                0
            } else {
                OsRng.gen_range(0..=u64::try_from(jitter.as_millis()).unwrap_or(u64::MAX))
            };
            let scheduled_at =
                book.next_start_at.max(now) + Duration::from_millis(jitter_ms);
            book.next_start_at = scheduled_at + self.inner.options.min_interval;
            book.stats.last_scheduled_at = wall_clock_ms();
            scheduled_at
        };
        // Skipping the sleep when no delay is due avoids the timer wheel's 1 ms granularity.
        if scheduled_at > Instant::now() {
            tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled_at)).await;
        }

        let started_at = Instant::now();
        debug!(
            waited_ms = started_at.duration_since(enqueued_at).as_millis(),
            "upstream call starting"
        );
        {
            let mut book = self.inner.book();
            book.stats.started += 1;
            book.stats.last_started_at = wall_clock_ms();
            book.stats.total_wait_ms +=
                started_at.duration_since(enqueued_at).as_millis();
        }

        let result = task.await;
        drop(permit);

        let mut book = self.inner.book();
        match &result {
            Ok(_) => {
                book.stats.completed += 1;
                book.stats.last_completed_at = wall_clock_ms();
                book.stats.total_run_ms += started_at.elapsed().as_millis();
            }
            Err(error) => {
                book.stats.failed += 1;
                book.stats.last_error_at = wall_clock_ms();
                book.stats.last_error = error.to_string().chars().take(500).collect();
            }
        }
        result
    }

    /// Current counters and configuration.
    pub fn snapshot(&self) -> FlowSnapshot {
        let (stats, queued) = {
            let book = self.inner.book();
            (book.stats.clone(), book.queued)
        };
        let options = &self.inner.options;
        let available = self.inner.semaphore.available_permits();
        let millis =
            |value: Duration| u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
        FlowSnapshot {
            concurrency: options.concurrency,
            active: options.concurrency.saturating_sub(available),
            queued,
            min_interval_ms: millis(options.min_interval),
            jitter_ms: millis(options.jitter),
            queue_timeout_ms: millis(options.queue_timeout),
            retries: options.retries,
            retry_delay_ms: millis(options.retry_delay),
            average_wait_ms: stats
                .total_wait_ms
                .checked_div(u128::from(stats.started))
                .unwrap_or(0),
            average_run_ms: stats
                .total_run_ms
                .checked_div(u128::from(stats.completed))
                .unwrap_or(0),
            stats,
        }
    }
}

fn wall_clock_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn options(
        concurrency: usize,
        min_interval_ms: u64,
        queue_timeout_ms: u64,
    ) -> FlowOptions {
        FlowOptions {
            concurrency,
            min_interval: Duration::from_millis(min_interval_ms),
            jitter: Duration::ZERO,
            queue_timeout: Duration::from_millis(queue_timeout_ms),
            retries: 2,
            retry_delay: Duration::from_millis(10),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn starts_are_spaced_by_the_minimum_interval() {
        let limiter = ApiFlowLimiter::new(options(1, 900, 600_000));
        let origin = tokio::time::Instant::now();
        let mut starts = Vec::new();
        for _ in 0..3 {
            let started = limiter
                .run(async { Ok(tokio::time::Instant::now()) })
                .await
                .unwrap();
            starts.push(started.duration_since(origin));
        }
        assert!(starts[1] - starts[0] >= Duration::from_millis(900));
        assert!(starts[2] - starts[1] >= Duration::from_millis(900));
        let snapshot = limiter.snapshot();
        assert_eq!((snapshot.stats.started, snapshot.stats.completed), (3, 3));
        assert_eq!(snapshot.queued, 0);
        assert_eq!(snapshot.active, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_is_bounded() {
        let limiter = ApiFlowLimiter::new(options(2, 0, 600_000));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..6 {
            let (limiter, running, peak) =
                (limiter.clone(), running.clone(), peak.clone());
            handles.push(tokio::spawn(async move {
                limiter
                    .run(async {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_past_the_queue_timeout_fails_and_is_counted() {
        let limiter = ApiFlowLimiter::new(options(1, 0, 100));
        let blocker = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                limiter
                    .run(async {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Ok(())
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(1)).await;
        let error = limiter.run(async { Ok(()) }).await.unwrap_err();
        assert!(matches!(error, UpstreamError::FlowQueueTimeout { .. }));
        assert_eq!(limiter.snapshot().stats.timed_out, 1);
        assert_eq!(limiter.snapshot().queued, 0);
        blocker.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_recorded_with_a_truncated_message() {
        let limiter = ApiFlowLimiter::new(options(1, 0, 1000));
        let result: Result<()> = limiter
            .run(async {
                Err(UpstreamError::Rejected {
                    what: "test",
                    detail: "x".repeat(2000),
                })
            })
            .await;
        assert!(result.is_err());
        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.stats.failed, 1);
        assert_eq!(snapshot.stats.last_error.chars().count(), 500);
    }
}
