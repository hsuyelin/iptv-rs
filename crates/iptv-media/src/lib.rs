//! Pure MPEG-TS and HLS logic for the IPTV relay.
//!
//! This crate has no network, async or WASM dependencies. The payload decryptor is
//! injected through [`PayloadCipher`], so demux, remux and playlist code can be tested
//! and benchmarked without any engine.

mod cipher;
mod error;
mod hls;
mod playlist;
mod remux;
mod ts;

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use cipher::{CipherError, PayloadCipher};
pub use error::MediaError;
pub use hls::{
    parse_media_playlist, playable_window, render_local_playlist, segment_id,
    ParsedPlaylist, ParsedSegment, SegmentRef, WindowPolicy,
};
pub use playlist::{
    build_channel_list, escape_attribute, ChannelListStyle, PlaylistEntry,
};
pub use remux::{decrypt_and_remux, RemuxStats, VideoState};
pub use ts::mux::MuxState;
