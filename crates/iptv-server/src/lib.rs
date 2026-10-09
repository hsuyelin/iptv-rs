//! HTTP server of the IPTV relay.
//!
//! The server owns request handling and presentation of JSON and playlists. Parsing,
//! remuxing and upstream access live in the other crates and are injected through
//! [`AppState`].

mod app;
pub mod config;
pub mod notice;
pub mod prefix;
pub mod stats;

pub use app::{
    router, system_clock, AppState, Clock, EPG_URL, NOTICE_CACHE_TTL_MS, NOTICE_LOGO_URL,
    NOTICE_NAME, NOTICE_URL,
};
pub use config::{ChannelIndex, ChannelStore, ConfigError, StoreStatus};
