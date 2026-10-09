//! Administrator mode: the key, its constant-time check and the brute-force limits.
//!
//! The console shows its admin pages only to a visitor whose URL path carries the key.
//! The browser proves it by posting the key to `/admin/verify`; this module decides.
//! Failed attempts are counted per client and overall, and a client that keeps failing
//! is locked out for longer each time. While a lock is active every attempt is refused
//! without looking at the key, so locking cannot be bypassed by guessing right.

use std::{
    collections::HashMap,
    fmt,
    net::IpAddr,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use axum::http::HeaderMap;
use rand::{distributions::Alphanumeric, rngs::OsRng, Rng};
use tracing::{info, warn};

use crate::Clock;

/// Shortest key accepted from the environment.
pub const MIN_KEY_LEN: usize = 12;
/// Length of a generated key: 32 letters and digits, about 190 bits.
pub const GENERATED_KEY_LEN: usize = 32;

/// Why a configured key was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    /// Too short to resist guessing.
    #[error("the administrator key must have at least {MIN_KEY_LEN} characters")]
    TooShort,
    /// The key becomes a URL path segment, so it must be URL-safe.
    #[error("the administrator key may only contain letters, digits, '-' and '_'")]
    BadCharacter,
}

/// The administrator key. Its `Debug` output never shows the value.
#[derive(Clone)]
pub struct AdminKey(String);

impl fmt::Debug for AdminKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminKey(<redacted>)")
    }
}

impl AdminKey {
    /// Checks and wraps a key given by the operator.
    ///
    /// # Errors
    /// Returns [`KeyError`] when the key is too short or not URL-safe.
    pub fn parse(value: &str) -> Result<Self, KeyError> {
        if value.chars().count() < MIN_KEY_LEN {
            return Err(KeyError::TooShort);
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(KeyError::BadCharacter);
        }
        Ok(Self(value.to_string()))
    }

    /// A fresh random key from the operating system's generator.
    pub fn generate() -> Self {
        Self(
            OsRng
                .sample_iter(&Alphanumeric)
                .take(GENERATED_KEY_LEN)
                .map(char::from)
                .collect(),
        )
    }

    /// The key itself, for printing it once at start-up.
    pub fn expose(&self) -> &str {
        &self.0
    }

    fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
    }
}

/// Compares in time that depends only on the longer input, not on where they differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        let x = a.get(index).copied().unwrap_or(0);
        let y = b.get(index).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// How hard a guess is made. See [`Limits::default`].
#[derive(Debug, Clone)]
pub struct Limits {
    /// Failures from one client inside `window` that trigger a lock.
    pub max_failures: u32,
    /// Window in which a client's failures are counted.
    pub window: Duration,
    /// First lock; doubles with every further lock of the same client.
    pub lockout: Duration,
    /// Upper bound of the doubled lock.
    pub max_lockout: Duration,
    /// Failures from everyone inside `global_window` that lock all attempts.
    pub global_max_failures: u32,
    /// Window of the overall count.
    pub global_window: Duration,
    /// Length of the overall lock.
    pub global_lockout: Duration,
    /// Most clients remembered at once; the least recent are forgotten first.
    pub max_clients: usize,
    /// Pause before answering a wrong key, to slow a guesser down.
    pub failure_delay: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_failures: 5,
            window: Duration::from_secs(15 * 60),
            lockout: Duration::from_secs(15 * 60),
            max_lockout: Duration::from_secs(24 * 3600),
            global_max_failures: 60,
            global_window: Duration::from_secs(10 * 60),
            global_lockout: Duration::from_secs(10 * 60),
            max_clients: 4096,
            failure_delay: Duration::from_millis(300),
        }
    }
}

/// The outcome of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The key is right.
    Granted,
    /// The key is wrong.
    Denied,
    /// Attempts are refused for now; try again after this many seconds.
    Locked {
        /// Seconds until the lock ends, rounded up.
        retry_after_secs: u64,
    },
}

#[derive(Debug, Default)]
struct Client {
    failures: u32,
    window_start: u128,
    locked_until: u128,
    strikes: u32,
    last_seen: u128,
}

#[derive(Debug, Default)]
struct Book {
    clients: HashMap<IpAddr, Client>,
    global_failures: u32,
    global_window_start: u128,
    global_locked_until: u128,
}

/// The key together with its limits and the memory of past failures.
pub struct AdminGate {
    key: AdminKey,
    limits: Limits,
    clock: Clock,
    book: Mutex<Book>,
}

impl fmt::Debug for AdminGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminGate")
            .field("key", &self.key)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

fn millis(duration: Duration) -> u128 {
    duration.as_millis()
}

fn seconds_left(until: u128, now: u128) -> u64 {
    u64::try_from(until.saturating_sub(now).div_ceil(1000)).unwrap_or(u64::MAX)
}

impl AdminGate {
    /// Builds a gate with explicit limits.
    pub fn new(key: AdminKey, limits: Limits, clock: Clock) -> Self {
        Self {
            key,
            limits,
            clock,
            book: Mutex::new(Book::default()),
        }
    }

    /// Builds a gate with the default limits.
    pub fn with_defaults(key: AdminKey, clock: Clock) -> Self {
        Self::new(key, Limits::default(), clock)
    }

    /// How long a wrong answer should be held back.
    pub fn failure_delay(&self) -> Duration {
        self.limits.failure_delay
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Judges one attempt from `client`. The candidate is never logged.
    pub fn check(&self, client: IpAddr, candidate: &str) -> Verdict {
        let now = (self.clock)();
        let mut book = self.book();

        if book.global_locked_until > now {
            let retry_after_secs = seconds_left(book.global_locked_until, now);
            warn!(%client, retry_after_secs, "admin attempt refused: global lock is active");
            return Verdict::Locked { retry_after_secs };
        }
        if let Some(entry) = book.clients.get(&client) {
            if entry.locked_until > now {
                let retry_after_secs = seconds_left(entry.locked_until, now);
                warn!(%client, retry_after_secs, "admin attempt refused: client is locked");
                return Verdict::Locked { retry_after_secs };
            }
        }

        if self.key.matches(candidate) {
            book.clients.remove(&client);
            info!(%client, "administrator key accepted");
            return Verdict::Granted;
        }

        let limits = &self.limits;
        let entry = book.clients.entry(client).or_default();
        if now.saturating_sub(entry.window_start) > millis(limits.window) {
            entry.failures = 0;
            entry.window_start = now;
        }
        entry.failures += 1;
        entry.last_seen = now;
        let failures = entry.failures;
        warn!(%client, failures, "administrator key rejected");
        if failures >= limits.max_failures {
            let factor = 1u128 << entry.strikes.min(16);
            let lock = (millis(limits.lockout) * factor).min(millis(limits.max_lockout));
            entry.locked_until = now + lock;
            entry.strikes += 1;
            entry.failures = 0;
            warn!(
                %client,
                lock_secs = lock / 1000,
                strikes = entry.strikes,
                "client locked out after repeated wrong administrator keys"
            );
        }

        if now.saturating_sub(book.global_window_start) > millis(limits.global_window) {
            book.global_failures = 0;
            book.global_window_start = now;
        }
        book.global_failures += 1;
        if book.global_failures >= limits.global_max_failures {
            book.global_locked_until = now + millis(limits.global_lockout);
            book.global_failures = 0;
            warn!(
                lock_secs = limits.global_lockout.as_secs(),
                "too many wrong administrator keys overall; all attempts locked"
            );
        }
        self.forget_stale(&mut book, now);
        Verdict::Denied
    }

    /// Keeps the table bounded: drops quiet clients, then the least recent ones.
    fn forget_stale(&self, book: &mut Book, now: u128) {
        let limits = &self.limits;
        if book.clients.len() <= limits.max_clients {
            return;
        }
        let window = millis(limits.window);
        book.clients.retain(|_, c| {
            c.locked_until > now || now.saturating_sub(c.last_seen) < window
        });
        while book.clients.len() > limits.max_clients {
            let oldest = book
                .clients
                .iter()
                .min_by_key(|(_, c)| c.last_seen)
                .map(|(ip, _)| *ip);
            match oldest {
                Some(ip) => {
                    book.clients.remove(&ip);
                }
                None => break,
            }
        }
    }
}

/// The address to hold failures against. Behind a proxy on this machine or network the
/// peer is the proxy, so the address the proxy appended to `X-Forwarded-For` (the last
/// one) or `X-Real-IP` is used; from anywhere else the headers are ignored, because the
/// caller could write anything in them.
pub fn client_ip(peer: Option<IpAddr>, headers: &HeaderMap) -> IpAddr {
    let unknown = IpAddr::from([0, 0, 0, 0]);
    let Some(peer) = peer else {
        return forwarded(headers).unwrap_or(unknown);
    };
    if is_trusted_proxy(peer) {
        forwarded(headers).unwrap_or(peer)
    } else {
        peer
    }
}

fn forwarded(headers: &HeaderMap) -> Option<IpAddr> {
    let last_hop = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|part| part.trim().parse::<IpAddr>().ok());
    last_hop.or_else(|| {
        headers
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
    })
}

fn is_trusted_proxy(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments().first().copied().unwrap_or(0) & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(test)]
mod tests;
