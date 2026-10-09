//! Fixed protocol values of the upstream service.

pub(crate) const PLAYER_API: &str = "https://player-api.yangshipin.cn/";
pub(crate) const PAGE_URL: &str = "https://www.yangshipin.cn/tv/home?pid=600099502";
/// Origin the relay presents to the upstream service.
pub const ACTIVE_URL: &str = "https://www.yangshipin.cn";
/// Browser user agent sent with every upstream request.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";

pub(crate) const DEFAULT_STREAM: &str = "2";
pub(crate) const DEFAULT_DEFN: &str = "fhd";

pub(crate) const YSPAPPID: &str = "519748109";
pub(crate) const PLATFORM: &str = "5910204";
pub(crate) const APP_VER: &str = "V1.0.0";
pub(crate) const CHANNEL: &str = "ysp_tx";
pub(crate) const AUTH_SECRET: &str = "n@7QKk%YeSjfw%22";
pub(crate) const LIVE_SECRET: &str = "0f$IVHi9Qno?G";
pub(crate) const CKEY_AES_KEY_HEX: &str = "48e5918a74ae21c972b90cce8af6c8be";
pub(crate) const CKEY_AES_IV_HEX: &str = "9a7e7d23610266b1d9fbf98581384d92";

pub(crate) const M3U8_REFRESH_AFTER_MS: u64 = 60_000;
pub(crate) const M3U8_TTL_MS: u64 = 180_000;
pub(crate) const M3U8_STALE_GRACE_MS: u64 = 300_000;
/// Segments listed in a served playlist.
pub const MEDIA_PLAYLIST_WINDOW_SEGMENTS: usize = 12;
/// Segments remembered per channel.
pub const MEDIA_HISTORY_MAX_SEGMENTS: usize = 36;
/// Newest segments withheld from a served playlist.
pub const MEDIA_LIVE_EDGE_HOLDBACK_SEGMENTS: usize = 1;
/// Published segment ids remembered across all channels.
pub const PUBLISHED_SEGMENTS_MAX: usize = 512;
/// Pending segment requests per channel before callers are turned away.
pub const CHANNEL_QUEUE_CAPACITY: usize = 8;

pub(crate) const API_FLOW_CONCURRENCY: usize = 1;
pub(crate) const API_FLOW_MIN_INTERVAL_MS: u64 = 900;
pub(crate) const API_FLOW_JITTER_MS: u64 = 300;
pub(crate) const API_FLOW_QUEUE_TIMEOUT_MS: u64 = 600_000;
pub(crate) const API_FLOW_RETRIES: usize = 2;
pub(crate) const API_FLOW_RETRY_DELAY_MS: u64 = 2_500;
