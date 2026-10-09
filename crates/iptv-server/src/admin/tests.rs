use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use super::*;

const KEY: &str = "correct-horse-battery";

struct Fixture {
    gate: AdminGate,
    now: Arc<AtomicU64>,
}

fn fixture(limits: Limits) -> Fixture {
    let now = Arc::new(AtomicU64::new(1_000_000));
    let reader = now.clone();
    let clock: Clock = Arc::new(move || u128::from(reader.load(Ordering::SeqCst)));
    Fixture {
        gate: AdminGate::new(AdminKey::parse(KEY).unwrap(), limits, clock),
        now,
    }
}

fn ip(last: u8) -> IpAddr {
    IpAddr::from([203, 0, 113, last])
}

impl Fixture {
    fn advance(&self, duration: Duration) {
        self.now.fetch_add(
            u64::try_from(duration.as_millis()).unwrap(),
            Ordering::SeqCst,
        );
    }
}

#[test]
fn keys_are_checked_for_length_and_characters() {
    assert!(AdminKey::parse("abcdefghijkl").is_ok());
    assert!(AdminKey::parse("A-b_c-D_e-F1").is_ok());
    assert_eq!(AdminKey::parse("short").unwrap_err(), KeyError::TooShort);
    assert_eq!(
        AdminKey::parse("has a space in it").unwrap_err(),
        KeyError::BadCharacter
    );
    assert_eq!(
        AdminKey::parse("with/slash-in-it").unwrap_err(),
        KeyError::BadCharacter
    );
    assert_eq!(
        AdminKey::parse("dots.break.paths").unwrap_err(),
        KeyError::BadCharacter
    );
}

#[test]
fn generated_keys_are_long_url_safe_and_different() {
    let first = AdminKey::generate();
    let second = AdminKey::generate();
    assert_eq!(first.expose().len(), GENERATED_KEY_LEN);
    assert!(first.expose().chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(first.expose(), second.expose());
    assert!(AdminKey::parse(first.expose()).is_ok());
}

#[test]
fn debug_output_never_shows_the_key() {
    let gate = fixture(Limits::default()).gate;
    let shown = format!("{gate:?} {:?}", AdminKey::parse(KEY).unwrap());
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("redacted"));
}

#[test]
fn comparison_looks_at_every_byte_and_the_length() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
    assert!(!constant_time_eq(b"abcd", b"abc"));
    assert!(!constant_time_eq(b"", b"a"));
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn the_right_key_is_granted_and_a_wrong_one_denied() {
    let f = fixture(Limits::default());
    assert_eq!(f.gate.check(ip(1), KEY), Verdict::Granted);
    assert_eq!(f.gate.check(ip(1), "wrong"), Verdict::Denied);
    assert_eq!(f.gate.check(ip(1), ""), Verdict::Denied);
    assert_eq!(f.gate.check(ip(1), &format!("{KEY}x")), Verdict::Denied);
}

#[test]
fn repeated_failures_lock_the_client_out_even_for_the_right_key() {
    let f = fixture(Limits::default());
    for _ in 0..5 {
        assert_eq!(f.gate.check(ip(1), "guess"), Verdict::Denied);
    }
    // Locked: the right key is refused too, so guessing cannot continue unchecked.
    assert_eq!(
        f.gate.check(ip(1), KEY),
        Verdict::Locked {
            retry_after_secs: 15 * 60
        }
    );
    f.advance(Duration::from_secs(60));
    assert_eq!(
        f.gate.check(ip(1), KEY),
        Verdict::Locked {
            retry_after_secs: 14 * 60
        }
    );
    // Another client is not affected.
    assert_eq!(f.gate.check(ip(2), KEY), Verdict::Granted);
}

#[test]
fn the_lock_ends_and_the_next_one_is_twice_as_long() {
    let f = fixture(Limits::default());
    for _ in 0..5 {
        f.gate.check(ip(1), "guess");
    }
    f.advance(Duration::from_secs(15 * 60 + 1));
    for _ in 0..5 {
        assert_eq!(f.gate.check(ip(1), "guess"), Verdict::Denied);
    }
    assert_eq!(
        f.gate.check(ip(1), KEY),
        Verdict::Locked {
            retry_after_secs: 30 * 60
        }
    );
}

#[test]
fn locks_stop_growing_at_the_maximum() {
    let limits = Limits {
        lockout: Duration::from_secs(600),
        max_lockout: Duration::from_secs(1000),
        ..Limits::default()
    };
    let f = fixture(limits);
    for round in 0..3 {
        for _ in 0..5 {
            f.gate.check(ip(1), "guess");
        }
        let expected = if round == 0 { 600 } else { 1000 };
        assert_eq!(
            f.gate.check(ip(1), "guess"),
            Verdict::Locked {
                retry_after_secs: expected
            },
            "round {round}"
        );
        f.advance(Duration::from_secs(expected + 1));
    }
}

#[test]
fn old_failures_age_out_of_the_window() {
    let f = fixture(Limits::default());
    for _ in 0..4 {
        f.gate.check(ip(1), "guess");
    }
    f.advance(Duration::from_secs(16 * 60));
    // The earlier four no longer count, so four more do not lock.
    for _ in 0..4 {
        assert_eq!(f.gate.check(ip(1), "guess"), Verdict::Denied);
    }
    assert_eq!(f.gate.check(ip(1), KEY), Verdict::Granted);
}

#[test]
fn a_success_clears_the_clients_failures() {
    let f = fixture(Limits::default());
    for _ in 0..4 {
        f.gate.check(ip(1), "guess");
    }
    assert_eq!(f.gate.check(ip(1), KEY), Verdict::Granted);
    for _ in 0..4 {
        assert_eq!(f.gate.check(ip(1), "guess"), Verdict::Denied);
    }
}

#[test]
fn many_clients_together_trigger_the_global_lock() {
    let limits = Limits {
        global_max_failures: 10,
        ..Limits::default()
    };
    let f = fixture(limits);
    for client in 0..10 {
        assert_eq!(f.gate.check(ip(client), "guess"), Verdict::Denied);
    }
    // A fresh client with the right key is refused while the overall lock holds.
    assert_eq!(
        f.gate.check(ip(200), KEY),
        Verdict::Locked {
            retry_after_secs: 10 * 60
        }
    );
    f.advance(Duration::from_secs(10 * 60 + 1));
    assert_eq!(f.gate.check(ip(200), KEY), Verdict::Granted);
}

#[test]
fn the_table_of_clients_stays_bounded() {
    let limits = Limits {
        max_clients: 8,
        global_max_failures: u32::MAX,
        ..Limits::default()
    };
    let f = fixture(limits);
    for client in 0..=200u8 {
        f.advance(Duration::from_millis(1));
        f.gate.check(ip(client), "guess");
    }
    assert!(f.gate.book().clients.len() <= 8);
}

#[test]
fn proxies_on_the_local_network_may_name_the_client() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-forwarded-for",
        "198.51.100.9, 192.0.2.44".parse().unwrap(),
    );
    let proxy = Some(IpAddr::from([127, 0, 0, 1]));
    // The last entry is the one our own proxy appended.
    assert_eq!(client_ip(proxy, &headers), IpAddr::from([192, 0, 2, 44]));
    assert_eq!(
        client_ip(Some(IpAddr::from([172, 18, 0, 3])), &headers),
        IpAddr::from([192, 0, 2, 44])
    );

    let mut real = HeaderMap::new();
    real.insert("x-real-ip", "192.0.2.50".parse().unwrap());
    assert_eq!(client_ip(proxy, &real), IpAddr::from([192, 0, 2, 50]));
    assert_eq!(
        client_ip(proxy, &HeaderMap::new()),
        IpAddr::from([127, 0, 0, 1])
    );
}

#[test]
fn headers_from_the_open_internet_are_ignored() {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", "192.0.2.44".parse().unwrap());
    headers.insert("x-real-ip", "192.0.2.45".parse().unwrap());
    let stranger = IpAddr::from([203, 0, 113, 77]);
    assert_eq!(client_ip(Some(stranger), &headers), stranger);
    // Garbage in a trusted proxy's header falls back to the proxy itself.
    let mut bad = HeaderMap::new();
    bad.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
    assert_eq!(
        client_ip(Some(IpAddr::from([10, 0, 0, 2])), &bad),
        IpAddr::from([10, 0, 0, 2])
    );
}
