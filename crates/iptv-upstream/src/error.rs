use iptv_media::MediaError;
use iptv_wasm::WasmError;

use crate::transport::TransportError;

/// Errors from the upstream client and the segment pipeline.
///
/// Messages never contain tokens, signatures or key material.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The HTTP request could not be completed.
    #[error("{what} request failed: {source}")]
    Transport {
        /// Which call failed.
        what: &'static str,
        /// Transport failure.
        #[source]
        source: TransportError,
    },
    /// The service answered with an error status.
    #[error("{what} failed status={status}: {body}")]
    Status {
        /// Which call failed.
        what: &'static str,
        /// HTTP status code.
        status: u16,
        /// Start of the response body.
        body: String,
    },
    /// The response body was not the expected JSON.
    #[error("cannot parse {what} response: {source}: {body}")]
    Parse {
        /// Which call failed.
        what: &'static str,
        /// Parser error.
        #[source]
        source: serde_json::Error,
        /// Start of the response body.
        body: String,
    },
    /// The service answered 200 but reported failure.
    #[error("{what} rejected: {detail}")]
    Rejected {
        /// Which call failed.
        what: &'static str,
        /// Codes and body start.
        detail: String,
    },
    /// A WASM signer or cipher failed.
    #[error(transparent)]
    Wasm(#[from] WasmError),
    /// Media parsing or remuxing failed.
    #[error("segment {sequence}: {source}")]
    Media {
        /// Sequence number of the segment.
        sequence: i64,
        /// Underlying media error.
        #[source]
        source: MediaError,
    },
    /// A playlist could not be parsed.
    #[error(transparent)]
    Playlist(#[from] MediaError),
    /// An upstream M3U8 had no segments and no usable child playlist.
    #[error("m3u8 has no segments or child playlists: {0}")]
    EmptyPlaylist(String),
    /// Master playlists nested deeper than allowed.
    #[error("m3u8 recursion limit exceeded for {0}")]
    PlaylistDepth(String),
    /// No segment remained after windowing.
    #[error("no playable upstream TS segments available")]
    NoPlayableSegments,
    /// The segment id was never published or has been forgotten.
    #[error("unknown segment; refresh playlist first")]
    UnknownSegment,
    /// The segment id belongs to another channel.
    #[error("segment does not belong to channel")]
    SegmentNotInChannel,
    /// The segment is older than what the channel runtime already processed.
    #[error(
        "segment {sequence} is older than runtime sequence {last} and was not cached"
    )]
    SegmentTooOld {
        /// Requested sequence.
        sequence: i64,
        /// Newest processed sequence.
        last: i64,
    },
    /// The channel's work queue is full.
    #[error("channel {0} is busy; retry shortly")]
    Overloaded(String),
    /// The channel worker stopped before answering.
    #[error("channel worker stopped")]
    WorkerStopped,
    /// A blocking task failed to complete.
    #[error("blocking task failed: {0}")]
    Join(String),
    /// The API flow queue timed out.
    #[error("api flow queue timeout after {waited_ms}ms")]
    FlowQueueTimeout {
        /// Milliseconds spent waiting.
        waited_ms: u128,
    },
    /// The API flow limiter was shut down.
    #[error("api flow semaphore closed")]
    FlowClosed,
    /// A header value or URL was invalid.
    #[error("invalid request data: {0}")]
    InvalidRequest(String),
    /// The AES key material is malformed.
    #[error("invalid signing key material: {0}")]
    Key(String),
}

impl UpstreamError {
    /// Whether a retry of the same call is likely to help.
    pub fn is_retryable(&self) -> bool {
        let message = self.to_string().to_ascii_lowercase();
        [
            "408",
            "425",
            "429",
            "500",
            "502",
            "503",
            "504",
            "timeout",
            "timed out",
            "rate",
            "limit",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, UpstreamError>;

pub(crate) fn body_head(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
