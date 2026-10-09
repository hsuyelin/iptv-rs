use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use iptv_wasm::{AssetBundle, KeygenSigner, TicketRequest, TicketSigner};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    channel::Channel,
    constants::{
        ACTIVE_URL, API_FLOW_CONCURRENCY, API_FLOW_JITTER_MS, API_FLOW_MIN_INTERVAL_MS,
        API_FLOW_QUEUE_TIMEOUT_MS, API_FLOW_RETRIES, API_FLOW_RETRY_DELAY_MS, APP_VER,
        AUTH_SECRET, CHANNEL, DEFAULT_DEFN, DEFAULT_STREAM, LIVE_SECRET,
        M3U8_REFRESH_AFTER_MS, M3U8_STALE_GRACE_MS, M3U8_TTL_MS, PLATFORM, PLAYER_API,
        USER_AGENT, YSPAPPID,
    },
    error::{body_head, Result, UpstreamError},
    flow::{ApiFlowLimiter, FlowOptions, FlowSnapshot},
    sdk::{
        build_input, build_request_id, canonical_body_md5, fetch_openapi_token,
        sign_with_token, SdkState,
    },
    secret::Secret,
    sign::{
        build_ckey, generate_guid, json_scalar_to_string,
        md5_js_default_sorted_with_secret, random_string,
    },
    transport::{HttpRequest, HttpResponse, HttpTransport, Method, RequestBody},
};

impl FlowOptions {
    /// The limits the relay uses against the upstream API.
    pub fn relay_defaults() -> Self {
        Self {
            concurrency: API_FLOW_CONCURRENCY,
            min_interval: Duration::from_millis(API_FLOW_MIN_INTERVAL_MS),
            jitter: Duration::from_millis(API_FLOW_JITTER_MS),
            queue_timeout: Duration::from_millis(API_FLOW_QUEUE_TIMEOUT_MS),
            retries: API_FLOW_RETRIES,
            retry_delay: Duration::from_millis(API_FLOW_RETRY_DELAY_MS),
        }
    }
}

/// A playable upstream URL with its cache timing.
#[derive(Clone)]
pub struct SourceCacheEntry {
    /// Channel slug.
    pub ch: String,
    /// Upstream channel id.
    pub cnlid: String,
    /// Upstream live program id.
    pub livepid: String,
    /// Cache key of the channel.
    pub cache_key: String,
    /// Guid used to obtain the URL.
    pub guid: String,
    /// Playback URL. It carries upstream credentials, so it is hidden from `Debug`.
    pub url: String,
    /// Fetch time in ms since the epoch.
    pub fetched_at_ms: u128,
    /// Time after which a background refresh starts.
    pub refresh_after_ms: u128,
    /// Time after which the entry is stale.
    pub expires_at_ms: u128,
    /// Time after which the entry is dropped.
    pub stale_until_ms: u128,
}

impl std::fmt::Debug for SourceCacheEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCacheEntry")
            .field("ch", &self.ch)
            .field("cache_key", &self.cache_key)
            .field("url", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Default)]
struct SourceCacheState {
    entries: HashMap<String, SourceCacheEntry>,
    refresh_inflight: HashSet<String>,
}

#[derive(Debug)]
enum CacheLookup {
    Hit {
        entry: Box<SourceCacheEntry>,
        refresh: bool,
    },
    Miss,
}

struct LiveInner {
    transport: Arc<dyn HttpTransport>,
    keygen: KeygenSigner,
    ticket: TicketSigner,
    flow: ApiFlowLimiter,
    cache: Mutex<SourceCacheState>,
}

impl LiveInner {
    /// Cache bookkeeping never awaits while the guard is held.
    fn cache(&self) -> MutexGuard<'_, SourceCacheState> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Fetches and caches playback URLs from the upstream API.
#[derive(Clone)]
pub struct LiveClient {
    inner: Arc<LiveInner>,
}

impl LiveClient {
    /// Creates a client that talks through `transport` and signs with `assets`.
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        assets: &AssetBundle,
        flow: FlowOptions,
    ) -> Self {
        Self {
            inner: Arc::new(LiveInner {
                transport,
                keygen: assets.keygen_signer(),
                ticket: assets.ticket_signer(),
                flow: ApiFlowLimiter::new(flow),
                cache: Mutex::new(SourceCacheState::default()),
            }),
        }
    }

    /// Current state of the API flow limiter.
    pub fn flow_snapshot(&self) -> FlowSnapshot {
        self.inner.flow.snapshot()
    }

    /// Returns the cached source for `channel`, fetching it when missing.
    ///
    /// A stale or soon-to-expire entry is returned immediately and refreshed in the
    /// background.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] when no cached source exists and the fetch fails.
    pub async fn fetch_source(&self, channel: Channel) -> Result<SourceCacheEntry> {
        let cache_key = channel.cache_key();
        if let Some(cached) = self.cached_source_or_refresh(&channel, &cache_key) {
            return Ok(cached);
        }
        self.refresh_source(channel).await
    }

    /// Marks the cached source as expired so the next use refreshes it.
    pub fn invalidate_source(&self, cache_key: &str) {
        let mut cache = self.inner.cache();
        if let Some(entry) = cache.entries.get_mut(cache_key) {
            let now = now_epoch_ms();
            entry.refresh_after_ms = now;
            entry.expires_at_ms = now;
        }
    }

    /// Starts a background refresh unless one is already running for the channel.
    pub fn refresh_source_background(&self, channel: Channel) {
        let cache_key = channel.cache_key();
        if !self
            .inner
            .cache()
            .refresh_inflight
            .insert(cache_key.clone())
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            match this.refresh_source(channel).await {
                Ok(entry) => {
                    debug!(channel = %entry.ch, "refreshed live source cache");
                }
                Err(error) => {
                    warn!(cache_key = %cache_key, error = %error, "background live source refresh failed");
                    this.inner.cache().refresh_inflight.remove(&cache_key);
                }
            }
        });
    }

    /// Fetches a fresh source now, bypassing the cache.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] when the upstream calls fail after retries.
    pub async fn refresh_source_now(&self, channel: Channel) -> Result<SourceCacheEntry> {
        self.refresh_source(channel).await
    }

    fn cached_source_or_refresh(
        &self,
        channel: &Channel,
        cache_key: &str,
    ) -> Option<SourceCacheEntry> {
        let now = now_epoch_ms();
        let lookup = lookup_cached_source(&mut self.inner.cache(), cache_key, now);
        match lookup {
            CacheLookup::Hit { entry, refresh } => {
                if refresh {
                    self.refresh_source_background(channel.clone());
                }
                Some(*entry)
            }
            CacheLookup::Miss => None,
        }
    }

    async fn refresh_source(&self, channel: Channel) -> Result<SourceCacheEntry> {
        let cache_key = channel.cache_key();
        let this = self.clone();
        let result = self
            .inner
            .flow
            .run(async move {
                let mut last_error = None;
                for attempt in 0..=API_FLOW_RETRIES {
                    match this.fetch_source_once(&channel).await {
                        Ok(value) => return Ok(value),
                        Err(error) => {
                            let retryable = error.is_retryable();
                            last_error = Some(error);
                            if attempt >= API_FLOW_RETRIES || !retryable {
                                break;
                            }
                            let step = u64::try_from(attempt + 1).unwrap_or(1);
                            tokio::time::sleep(Duration::from_millis(
                                API_FLOW_RETRY_DELAY_MS * step,
                            ))
                            .await;
                        }
                    }
                }
                Err(last_error.unwrap_or_else(|| UpstreamError::Rejected {
                    what: "get_live_info",
                    detail: "no attempt was made".to_string(),
                }))
            })
            .await;
        let mut cache = self.inner.cache();
        cache.refresh_inflight.remove(&cache_key);
        let value = result?;
        cache.entries.insert(cache_key, value.clone());
        Ok(value)
    }

    async fn send(
        &self,
        what: &'static str,
        request: HttpRequest,
    ) -> Result<HttpResponse> {
        self.inner
            .transport
            .send(request)
            .await
            .map_err(|source| UpstreamError::Transport { what, source })
    }

    async fn fetch_source_once(&self, channel: &Channel) -> Result<SourceCacheEntry> {
        let guid = generate_guid();
        let request_ts = unix_seconds();
        let ckey = build_ckey(&channel.cnlid, request_ts, &guid)?;
        let auth = self.auth_request(&channel.livepid, &guid).await?;
        let sign_body = build_live_sign_body(channel, &guid, &ckey.ckey);
        let body_md5 = canonical_body_md5(
            sign_body
                .iter()
                .map(|(key, value)| (key.clone(), json_scalar_to_string(value))),
        );
        let request_id = build_request_id();
        let sdk_input = build_input(&body_md5, &guid, 1, &request_id);
        let sdk_token =
            fetch_openapi_token(self.inner.transport.as_ref(), &self.inner.keygen, &guid)
                .await?;
        let mut sdk_state = SdkState::new(&guid, sdk_token.token.clone(), sdk_input);
        sdk_state.ts = sdk_token.ts.clone();
        let sdk_headers = sign_with_token(&self.inner.keygen, sdk_state, 1, request_id)?;
        let ticket = self.inner.ticket.sign(&TicketRequest {
            pid: &channel.livepid,
            auth_ts: &auth.ts,
            cnlid: &channel.cnlid,
            guid: &guid,
            app_id: YSPAPPID,
            app_ver: APP_VER,
        })?;

        let mut body = sign_body;
        body.insert(
            "rand_str".to_string(),
            serde_json::Value::String(random_string(10)),
        );
        let signature = md5_js_default_sorted_with_secret(
            body.iter()
                .map(|(key, value)| (key.clone(), json_scalar_to_string(value))),
            LIVE_SECRET,
        );
        body.insert(
            "signature".to_string(),
            serde_json::Value::String(signature),
        );

        let mut headers = base_headers(&guid);
        headers.extend([
            ("yspappid".to_string(), YSPAPPID.to_string()),
            (
                "content-type".to_string(),
                "application/json;charset=UTF-8".to_string(),
            ),
            ("yspsdkinput".to_string(), sdk_headers.yspsdkinput),
            (
                "yspsdksign".to_string(),
                sdk_headers.yspsdksign.expose().to_string(),
            ),
            ("seqId".to_string(), sdk_headers.seq_id.to_string()),
            ("request-id".to_string(), sdk_headers.request_id),
            (
                "yspPlayerToken".to_string(),
                auth.token.expose().to_string(),
            ),
            ("yspticket".to_string(), ticket),
        ]);
        let payload = serde_json::to_vec(&body).map_err(|error| {
            UpstreamError::InvalidRequest(format!("live info body: {error}"))
        })?;
        let request = HttpRequest {
            method: Method::Post,
            url: format!("{PLAYER_API}v1/player/get_live_info"),
            headers,
            body: RequestBody::Json(payload),
        };
        let response = self.send("get_live_info", request).await?;
        let text = response.text();
        if !response.is_success() {
            return Err(UpstreamError::Status {
                what: "get_live_info",
                status: response.status,
                body: body_head(&text, 1000),
            });
        }
        let parsed: LiveInfoResponse =
            serde_json::from_str(&text).map_err(|source| UpstreamError::Parse {
                what: "get_live_info",
                source,
                body: body_head(&text, 1000),
            })?;
        let data = parsed.data.unwrap_or_default();
        if parsed.code != 0 || data.iretcode != 0 || data.playurl.is_empty() {
            return Err(UpstreamError::Rejected {
                what: "get_live_info",
                detail: format!(
                    "code={} iretcode={}: {}",
                    parsed.code,
                    data.iretcode,
                    body_head(&text, 1000)
                ),
            });
        }

        let now = now_epoch_ms();
        Ok(SourceCacheEntry {
            ch: channel.ch.clone(),
            cnlid: channel.cnlid.clone(),
            livepid: channel.livepid.clone(),
            cache_key: channel.cache_key(),
            guid,
            url: build_playback_url(&data),
            fetched_at_ms: now,
            refresh_after_ms: now + u128::from(M3U8_REFRESH_AFTER_MS),
            expires_at_ms: now + u128::from(M3U8_TTL_MS),
            stale_until_ms: now
                + u128::from(M3U8_TTL_MS)
                + u128::from(M3U8_STALE_GRACE_MS),
        })
    }

    async fn auth_request(&self, pid: &str, guid: &str) -> Result<AuthData> {
        let mut body = HashMap::from([
            ("pid".to_string(), pid.to_string()),
            ("guid".to_string(), guid.to_string()),
            ("appid".to_string(), "ysp_pc".to_string()),
            ("rand_str".to_string(), random_string(10)),
        ]);
        let signature = md5_js_default_sorted_with_secret(
            body.iter().map(|(key, value)| (key.clone(), value.clone())),
            AUTH_SECRET,
        );
        body.insert("signature".to_string(), signature);
        let mut headers = base_headers(guid);
        headers.extend([
            ("yspappid".to_string(), YSPAPPID.to_string()),
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded;charset=UTF-8".to_string(),
            ),
        ]);
        let request = HttpRequest {
            method: Method::Post,
            url: format!("{PLAYER_API}v1/player/auth"),
            headers,
            body: RequestBody::Form(body.into_iter().collect()),
        };
        let response = self.send("auth", request).await?;
        let status = response.status;
        let text = response.text();
        let parsed: AuthResponse =
            serde_json::from_str(&text).map_err(|source| UpstreamError::Parse {
                what: "auth",
                source,
                body: body_head(&text, 500),
            })?;
        let data = parsed.data.unwrap_or_default();
        if !(200..300).contains(&status) || parsed.code != 0 || data.token.is_empty() {
            return Err(UpstreamError::Rejected {
                what: "auth",
                detail: format!("status={status} code={}", parsed.code),
            });
        }
        Ok(AuthData {
            token: Secret::new(data.token),
            ts: data.ts,
        })
    }
}

fn build_live_sign_body(
    channel: &Channel,
    guid: &str,
    ckey: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let body = serde_json::json!({
        "cnlid": channel.cnlid,
        "livepid": channel.livepid,
        "stream": DEFAULT_STREAM,
        "guid": guid,
        "cKey": ckey,
        "adjust": 1,
        "sphttps": "1",
        "platform": PLATFORM,
        "cmd": "2",
        "encryptVer": "8.1",
        "dtype": "1",
        "devid": "devid",
        "otype": "ojson",
        "appVer": APP_VER,
        "app_version": APP_VER,
        "channel": CHANNEL,
        "defn": DEFAULT_DEFN,
    });
    match body {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<AuthWire>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AuthWire {
    #[serde(default)]
    token: String,
    #[serde(default)]
    ts: String,
}

struct AuthData {
    token: Secret,
    ts: String,
}

#[derive(Debug, Deserialize)]
struct LiveInfoResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<LiveInfoData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LiveInfoData {
    #[serde(default)]
    iretcode: i64,
    #[serde(default)]
    playurl: String,
    #[serde(default)]
    extended_param: String,
    #[serde(default)]
    chanll: Option<serde_json::Value>,
}

fn base_headers(guid: &str) -> Vec<(String, String)> {
    vec![
        ("accept".into(), "application/json, text/plain, */*".into()),
        ("origin".into(), ACTIVE_URL.into()),
        ("referer".into(), format!("{ACTIVE_URL}/")),
        ("user-agent".into(), USER_AGENT.into()),
        ("cookie".into(), build_cookie(guid)),
    ]
}

fn build_cookie(guid: &str) -> String {
    [
        format!("guid={guid}"),
        "versionName=99.99.99".to_string(),
        "versionCode=999999".to_string(),
        "vplatform=109".to_string(),
        "platformVersion=Chrome".to_string(),
        "deviceModel=148".to_string(),
        "newLogin=1".to_string(),
        "pc_version=1.1.16".to_string(),
    ]
    .join("; ")
}

fn build_playback_url(data: &LiveInfoData) -> String {
    let revoi = decode_revoi(data.chanll.as_ref());
    let mut url = format!("{}&revoi={}", data.playurl, revoi);
    if !data.extended_param.is_empty() {
        url.push_str(&data.extended_param);
    }
    url
}

fn decode_revoi(chanll: Option<&serde_json::Value>) -> String {
    let Some(chanll) = chanll else {
        return String::new();
    };
    let code = if let Some(code) = chanll.get("code").and_then(|value| value.as_str()) {
        code.to_string()
    } else if let Some(text) = chanll.as_str() {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| {
                value
                    .get("code")
                    .and_then(|code| code.as_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    if code.is_empty() {
        return String::new();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(code) else {
        return String::new();
    };
    let decoded = String::from_utf8_lossy(&bytes);
    serde_json::from_str::<String>(decoded.trim()).unwrap_or_default()
}

fn lookup_cached_source(
    cache: &mut SourceCacheState,
    cache_key: &str,
    now: u128,
) -> CacheLookup {
    let Some(entry) = cache.entries.get(cache_key).cloned() else {
        return CacheLookup::Miss;
    };
    if entry.expires_at_ms > now {
        return CacheLookup::Hit {
            refresh: entry.refresh_after_ms <= now,
            entry: Box::new(entry),
        };
    }
    if entry.stale_until_ms > now {
        return CacheLookup::Hit {
            entry: Box::new(entry),
            refresh: true,
        };
    }
    cache.entries.remove(cache_key);
    CacheLookup::Miss
}

fn unix_seconds() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests;
