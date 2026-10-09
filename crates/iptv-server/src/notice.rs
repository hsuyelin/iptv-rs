//! The short-lived list of channels redirected to the notice stream.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use serde::Serialize;

/// Channels that failed recently and are redirected to the notice stream.
#[derive(Debug)]
pub struct NoticeCache {
    ttl_ms: u128,
    expiries: Mutex<HashMap<String, u128>>,
}

/// One entry as shown by `/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoticeCacheItem {
    /// When the entry lapses, in ms since the epoch.
    pub expires_at_ms: u128,
    /// Milliseconds left.
    pub ttl_ms: u128,
}

impl NoticeCache {
    /// A cache whose entries last `ttl_ms` milliseconds.
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms: u128::from(ttl_ms),
            expiries: Mutex::new(HashMap::new()),
        }
    }

    /// Configured lifetime of an entry.
    pub fn ttl_ms(&self) -> u128 {
        self.ttl_ms
    }

    /// Whether `ch` is currently redirected. An expired entry is removed.
    pub fn is_active(&self, ch: &str, now_ms: u128) -> bool {
        let key = ch.to_ascii_lowercase();
        let mut entries = self.expiries.lock().unwrap_or_else(PoisonError::into_inner);
        match entries.get(&key) {
            Some(&expires_at) if expires_at > now_ms => true,
            Some(_) => {
                entries.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Redirects `ch` to the notice stream until the lifetime elapses.
    pub fn mark(&self, ch: &str, now_ms: u128) {
        self.expiries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ch.to_ascii_lowercase(), now_ms + self.ttl_ms);
    }

    /// Active entries, for `/health`.
    pub fn snapshot(&self, now_ms: u128) -> HashMap<String, NoticeCacheItem> {
        self.expiries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, &expires_at)| expires_at > now_ms)
            .map(|(ch, &expires_at)| {
                (
                    ch.clone(),
                    NoticeCacheItem {
                        expires_at_ms: expires_at,
                        ttl_ms: expires_at.saturating_sub(now_ms),
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_last_exactly_the_ttl() {
        let cache = NoticeCache::new(1000);
        assert!(!cache.is_active("cctv1", 0));
        cache.mark("CCTV1", 100);
        assert!(cache.is_active("cctv1", 100));
        assert!(cache.is_active("cctv1", 1099));
        assert!(!cache.is_active("cctv1", 1100));
        // The expired entry is gone.
        assert!(cache.snapshot(0).is_empty());
    }

    #[test]
    fn snapshot_lists_only_active_entries_with_remaining_time() {
        let cache = NoticeCache::new(1000);
        cache.mark("a", 0);
        cache.mark("b", 500);
        let snapshot = cache.snapshot(600);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot["a"].ttl_ms, 400);
        let later = cache.snapshot(1200);
        assert_eq!(later.len(), 1);
        assert_eq!(later["b"].expires_at_ms, 1500);
    }
}
