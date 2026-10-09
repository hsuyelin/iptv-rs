//! Lock-free request counters.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Request counters. Updating them never blocks.
#[derive(Debug)]
pub struct Stats {
    started_at_ms: u128,
    playlist_requests: AtomicU64,
    segment_requests: AtomicU64,
    segment_streamed: AtomicU64,
    segment_errors: AtomicU64,
    segment_rejected: AtomicU64,
}

/// Counters at one instant, in the shape `/health` serves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatsSnapshot {
    /// Process start, in ms since the epoch.
    pub started_at_ms: u128,
    /// Playlist requests received.
    pub playlist_requests: u64,
    /// Segment requests received.
    pub segment_requests: u64,
    /// Completed upstream live-info fetches (filled from the flow limiter).
    pub live_info_fetches: u64,
    /// Segments served.
    pub segment_streamed: u64,
    /// Segment requests that failed.
    pub segment_errors: u64,
    /// Segment requests turned away because the channel was busy.
    pub segment_rejected: u64,
}

impl Stats {
    /// Counters at zero, stamped with the start time.
    pub fn new(started_at_ms: u128) -> Self {
        Self {
            started_at_ms,
            playlist_requests: AtomicU64::new(0),
            segment_requests: AtomicU64::new(0),
            segment_streamed: AtomicU64::new(0),
            segment_errors: AtomicU64::new(0),
            segment_rejected: AtomicU64::new(0),
        }
    }

    /// Counts a playlist request.
    pub fn playlist_requested(&self) {
        self.playlist_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a segment request.
    pub fn segment_requested(&self) {
        self.segment_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a segment that was served.
    pub fn segment_streamed(&self) {
        self.segment_streamed.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a failed segment request.
    pub fn segment_failed(&self) {
        self.segment_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a segment request rejected for overload.
    pub fn segment_rejected(&self) {
        self.segment_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Reads all counters.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            started_at_ms: self.started_at_ms,
            playlist_requests: self.playlist_requests.load(Ordering::Relaxed),
            segment_requests: self.segment_requests.load(Ordering::Relaxed),
            live_info_fetches: 0,
            segment_streamed: self.segment_streamed.load(Ordering::Relaxed),
            segment_errors: self.segment_errors.load(Ordering::Relaxed),
            segment_rejected: self.segment_rejected.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_independently() {
        let stats = Stats::new(42);
        stats.playlist_requested();
        stats.segment_requested();
        stats.segment_requested();
        stats.segment_streamed();
        stats.segment_failed();
        stats.segment_rejected();
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.started_at_ms, 42);
        assert_eq!(
            (
                snapshot.playlist_requests,
                snapshot.segment_requests,
                snapshot.segment_streamed,
                snapshot.segment_errors,
                snapshot.segment_rejected
            ),
            (1, 2, 1, 1, 1)
        );
    }

    #[test]
    fn counters_are_safe_across_threads() {
        let stats = std::sync::Arc::new(Stats::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let stats = stats.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        stats.segment_requested();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(stats.snapshot().segment_requests, 8000);
    }
}
