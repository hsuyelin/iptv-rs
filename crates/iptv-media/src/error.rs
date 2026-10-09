use crate::cipher::CipherError;

/// Errors produced while parsing, decrypting or remuxing media data.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The transport stream input had no bytes.
    #[error("TS segment is empty")]
    Empty,
    /// The input length is not a multiple of 188 bytes.
    #[error("TS segment is not 188-byte aligned: {len} bytes")]
    Misaligned {
        /// Length of the rejected input.
        len: usize,
    },
    /// A packet did not start with the 0x47 sync byte.
    #[error("bad TS sync byte at offset {offset}")]
    BadSync {
        /// Byte offset of the offending packet.
        offset: usize,
    },
    /// The PAT did not list a PMT PID.
    #[error("PAT did not expose a PMT PID")]
    NoPmt,
    /// The PMT listed no elementary streams.
    #[error("PMT did not expose elementary streams")]
    NoStreams,
    /// The PMT had no H.264 video stream.
    #[error("PMT did not expose an H264 video PID")]
    NoVideoStream,
    /// The PMT had no AAC audio stream.
    #[error("PMT did not expose an AAC audio PID")]
    NoAudioStream,
    /// Demux produced no video samples.
    #[error("no video samples after TS demux")]
    NoVideoSamples,
    /// Demux produced no audio samples.
    #[error("no audio samples after TS demux")]
    NoAudioSamples,
    /// The injected cipher failed.
    #[error("payload cipher failed at {stage} (PES {index}): {source}")]
    Cipher {
        /// Processing stage that called the cipher.
        stage: &'static str,
        /// Index of the PES being processed.
        index: usize,
        /// Underlying cipher error.
        #[source]
        source: CipherError,
    },
    /// The cipher returned more bytes than it was given.
    #[error(
        "cipher output grew from {before} to {after} bytes for H264 NAL type {nal_type}"
    )]
    CipherGrew {
        /// Input NAL length.
        before: usize,
        /// Output NAL length.
        after: usize,
        /// H.264 NAL unit type.
        nal_type: u8,
    },
    /// A rebuilt PES does not fit the TS packets it came from.
    #[error("rebuilt PES is larger than available TS payload: {needed} > {available}")]
    PesOverflow {
        /// Bytes needed.
        needed: usize,
        /// Bytes available.
        available: usize,
    },
    /// A playlist URL could not be parsed or joined.
    #[error("invalid playlist URL {url}: {reason}")]
    PlaylistUrl {
        /// URL text.
        url: String,
        /// Parser message.
        reason: String,
    },
}
