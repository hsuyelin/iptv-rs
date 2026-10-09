//! Log set-up. Every line carries the time (RFC 3339, UTC, microseconds), the level, the
//! module, the source file and line, the thread, the message and its fields.
//!
//! The default shows `info` and above for the relay's own crates and `warn` for
//! everything else; `-v` adds `debug` and `-vv` adds `trace` for the relay's crates.
//! `RUST_LOG` overrides all of it. Secrets are never passed to the log macros.

use std::io::IsTerminal;

use tracing::Subscriber;
use tracing_subscriber::{
    fmt::{time::SystemTime, MakeWriter},
    EnvFilter,
};

/// The filter directive for a number of `-v` flags.
pub fn filter_for(verbosity: u8) -> String {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    format!(
        "warn,iptv_rs={level},iptv_server={level},iptv_upstream={level},iptv_wasm={level},iptv_media={level}"
    )
}

/// A subscriber writing detailed lines to `writer`.
pub fn subscriber<W>(
    writer: W,
    filter: EnvFilter,
    ansi: bool,
) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(ansi)
        .with_timer(SystemTime)
        .with_level(true)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_names(true)
        .finish()
}

/// Installs the process-wide logger on standard error and a panic hook that logs the
/// panic with its location and a backtrace before the default handler runs.
pub fn init(verbosity: u8) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(filter_for(verbosity)));
    let ansi = std::io::stderr().is_terminal();
    // A second call (for example from a test) keeps the first logger.
    let _ = tracing::subscriber::set_global_default(subscriber(
        std::io::stderr,
        filter,
        ansi,
    ));

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or_else(
            || "unknown".to_string(),
            |l| format!("{}:{}", l.file(), l.line()),
        );
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-text panic payload".to_string());
        tracing::error!(
            %location,
            %message,
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "the relay panicked"
        );
        previous(info);
    }));
}

#[cfg(test)]
mod tests;
