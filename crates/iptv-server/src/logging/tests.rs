use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use tracing::{debug, error, info, warn};

use super::*;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

fn run(filter: &str, emit: impl FnOnce()) -> String {
    let capture = Capture::default();
    let subscriber = subscriber(capture.clone(), EnvFilter::new(filter), false);
    tracing::subscriber::with_default(subscriber, emit);
    let bytes = capture.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn every_line_has_time_level_file_line_and_message() {
    let text = run("info", || info!(channel = "cctv1", "segment served"));
    let line = text.lines().next().unwrap();
    // 2026-10-09T08:15:30.123456Z  INFO thread iptv_server::logging::tests: file:line: msg k=v
    let (stamp, _) = line.split_once(' ').unwrap();
    assert!(stamp.ends_with('Z') && stamp.contains('T'), "{line}");
    let digits: Vec<&str> = stamp.split(|c: char| !c.is_ascii_digit()).collect();
    assert!(digits.len() >= 7 && digits[0].len() == 4, "{line}");
    assert!(line.contains(" INFO "), "{line}");
    assert!(line.contains("iptv_server::logging::tests"), "{line}");
    assert!(
        line.contains("crates/iptv-server/src/logging/tests.rs:"),
        "{line}"
    );
    let after_file = line.split("tests.rs:").nth(1).unwrap();
    assert!(
        after_file.chars().next().unwrap().is_ascii_digit(),
        "{line}"
    );
    assert!(line.contains("segment served"), "{line}");
    assert!(line.contains("channel=\"cctv1\""), "{line}");
}

#[test]
fn levels_are_named() {
    let text = run("trace", || {
        error!("e");
        warn!("w");
        info!("i");
        debug!("d");
    });
    for level in ["ERROR", "WARN", "INFO", "DEBUG"] {
        assert!(text.contains(level), "{level} missing in {text}");
    }
}

#[test]
fn verbosity_picks_the_levels_of_the_relay_crates() {
    assert!(filter_for(0).contains("iptv_upstream=info"));
    // The binary's own messages (start-up, shutdown) come from the `iptv_rs` target.
    assert!(filter_for(0).contains("iptv_rs=info"));
    assert!(filter_for(1).contains("iptv_upstream=debug"));
    assert!(filter_for(2).contains("iptv_server=trace"));
    assert!(filter_for(9).contains("iptv_wasm=trace"));
    assert!(filter_for(0).starts_with("warn,"));
    // The directive is valid for the real filter.
    for verbosity in 0..3 {
        EnvFilter::try_new(filter_for(verbosity)).unwrap();
    }
}

#[test]
fn quieter_levels_are_filtered_out() {
    let text = run(&filter_for(0), || {
        debug!("hidden detail");
        info!("shown");
    });
    assert!(!text.contains("hidden detail"));
    assert!(text.contains("shown"));
    let verbose = run("debug", || debug!("now shown"));
    assert!(verbose.contains("now shown"));
}
