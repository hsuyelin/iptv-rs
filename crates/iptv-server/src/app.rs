use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::to_bytes,
    extract::{ConnectInfo, MatchedPath, Path as UrlPath, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use iptv_media::{build_channel_list, ChannelListStyle, PlaylistEntry};
use iptv_upstream::{FlowSnapshot, MediaPipeline, UpstreamError};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{debug, error, info, warn};

use crate::{
    admin::{client_ip, AdminGate, Verdict},
    config::{ChannelStore, StoreStatus},
    notice::{NoticeCache, NoticeCacheItem},
    prefix::{abs_url, append_recursive_prefix},
    stats::{Stats, StatsSnapshot},
};

/// Stream shown while a channel is unavailable.
pub const NOTICE_URL: &str = "https://cdn.jsdelivr.net/gh/jkwu5472/first/media.m3u8";
/// Logo of the notice entry in the channel list.
pub const NOTICE_LOGO_URL: &str = "https://cdn.jsdelivr.net/gh/jkwu5472/first/notice.jpg";
/// XMLTV guide announced by the channel list.
pub const EPG_URL: &str = "https://epg.zsdc.eu.org/t.xml";
/// Name of the notice entry in the channel list.
pub const NOTICE_NAME: &str = "注意事项";
/// How long a failing channel is redirected to the notice stream.
pub const NOTICE_CACHE_TTL_MS: u64 = 60_000;

const PLAYLIST_TYPE: &str = "application/vnd.apple.mpegurl; charset=utf-8";
const ROUTES: [&str; 5] = [
    "/list.m3u",
    "/live/<ch>.m3u8",
    "/segment/<ch>/<id>.ts",
    "/channels",
    "/health",
];

/// Milliseconds since the Unix epoch; injectable so tests control time.
pub type Clock = Arc<dyn Fn() -> u128 + Send + Sync>;

/// A clock reading the system time.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    })
}

/// Everything the handlers need. Cloning is cheap.
#[derive(Clone)]
pub struct AppState {
    channels: Arc<ChannelStore>,
    pipeline: MediaPipeline,
    stats: Arc<Stats>,
    notices: Arc<NoticeCache>,
    admin: Arc<AdminGate>,
    clock: Clock,
}

impl AppState {
    /// Assembles the state from its collaborators.
    pub fn new(
        channels: Arc<ChannelStore>,
        pipeline: MediaPipeline,
        admin: Arc<AdminGate>,
        clock: Clock,
    ) -> Self {
        Self {
            stats: Arc::new(Stats::new(clock())),
            notices: Arc::new(NoticeCache::new(NOTICE_CACHE_TTL_MS)),
            channels,
            pipeline,
            admin,
            clock,
        }
    }

    fn now_ms(&self) -> u128 {
        (self.clock)()
    }
}

/// Largest body `/admin/verify` reads; a key is far shorter.
const VERIFY_BODY_LIMIT: usize = 1024;

/// Builds the HTTP router. When `web_dir` is set, files from it answer every path that is
/// not an API route, and a single-segment path without a dot (the console's address with
/// an administrator key) answers with the console's `index.html`. Any other path without a
/// dot that no file answers gets the same page with status 404, for the console's own
/// "page not found".
pub fn router(state: AppState, web_dir: Option<&Path>) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/channels", get(channels))
        .route("/list.m3u", get(list_m3u))
        .route("/live/{file}", get(live_playlist))
        .route("/segment/{ch}/{file}", get(segment))
        .route("/admin/verify", post(admin_verify))
        .with_state(state);
    let app = match web_dir {
        Some(dir) => {
            let dir = Arc::new(dir.to_path_buf());
            api.fallback(move |request: Request| console(Arc::clone(&dir), request))
        }
        None => api,
    };
    app.layer(middleware::from_fn(log_request))
        .layer(CorsLayer::permissive())
}

/// True for the console's page addresses, which are not files: `/<key>`.
fn is_page_path(path: &str) -> bool {
    let trimmed = path.trim_matches('/');
    !trimmed.is_empty() && !trimmed.contains('/') && !trimmed.contains('.')
}

/// True for an address that no file answers but that is not a file name either (`/a/b`):
/// the console draws its own "page not found" for it.
fn is_unknown_page(path: &str) -> bool {
    !path.contains('.')
}

/// The console's `index.html` with `status`, never cached.
async fn console_page(dir: &Path, status: StatusCode) -> Response {
    match tokio::fs::read(dir.join("index.html")).await {
        Ok(page) => {
            let mut response = Response::new(axum::body::Body::from(page));
            *response.status_mut() = status;
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            response
        }
        Err(error) => {
            error!(dir = %dir.display(), %error, "cannot read the console's index.html");
            not_found()
        }
    }
}

async fn console(dir: Arc<PathBuf>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if is_page_path(&path) {
        return console_page(&dir, StatusCode::OK).await;
    }
    match ServeDir::new(&*dir).oneshot(request).await {
        Ok(response)
            if response.status() == StatusCode::NOT_FOUND && is_unknown_page(&path) =>
        {
            console_page(&dir, StatusCode::NOT_FOUND).await
        }
        Ok(response) => response.into_response(),
        Err(never) => match never {},
    }
}

static REQUEST_IDS: AtomicU64 = AtomicU64::new(1);

/// Logs one line per request: id, method, route, status, size and time. The address is
/// logged only for API routes; a console page address may contain the administrator key,
/// so it is shown as `<static>`.
async fn log_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let id = REQUEST_IDS.fetch_add(1, Ordering::Relaxed);
    let method = request.method().clone();
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string());
    let path = match &matched {
        Some(_) => request.uri().path().to_string(),
        None => "<static>".to_string(),
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let client = client_ip(peer, request.headers());
    let user_agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .chars()
        .take(80)
        .collect::<String>();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let bytes = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let elapsed_ms = started.elapsed().as_millis();
    let route = matched.as_deref().unwrap_or("<fallback>");
    match status {
        500..=599 => {
            error!(id, %method, %path, route, status, bytes, elapsed_ms, %client, user_agent, "request failed")
        }
        400..=499 => {
            warn!(id, %method, %path, route, status, bytes, elapsed_ms, %client, user_agent, "request rejected")
        }
        _ => {
            info!(id, %method, %path, route, status, bytes, elapsed_ms, %client, user_agent, "request served")
        }
    }
    response
}

#[derive(Deserialize)]
struct VerifyBody {
    key: String,
}

/// Checks an administrator key. Right: 200. Wrong: 403, after a short pause. Too many
/// wrong ones from this client or overall: 429 with `Retry-After`.
async fn admin_verify(State(state): State<AppState>, request: Request) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let client = client_ip(peer, request.headers());
    let body = match to_bytes(request.into_body(), VERIFY_BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => {
            warn!(%client, "administrator check refused: body too large or unreadable");
            return no_store(json_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                serde_json::json!({ "ok": false }),
            ));
        }
    };
    // A body that is not the expected JSON counts as a wrong key.
    let candidate = serde_json::from_slice::<VerifyBody>(&body)
        .map(|parsed| parsed.key)
        .unwrap_or_default();
    let response = match state.admin.check(client, &candidate) {
        Verdict::Granted => {
            json_status(StatusCode::OK, serde_json::json!({ "ok": true }))
        }
        Verdict::Denied => {
            tokio::time::sleep(state.admin.failure_delay()).await;
            json_status(StatusCode::FORBIDDEN, serde_json::json!({ "ok": false }))
        }
        Verdict::Locked { retry_after_secs } => {
            let mut response = json_status(
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({ "ok": false, "retry_after": retry_after_secs }),
            );
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    };
    no_store(response)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    mode: &'static str,
    stats: StatsSnapshot,
    channels: ChannelHealth,
    notice: NoticeHealth,
    api_flow: FlowSnapshot,
    routes: [&'static str; 5],
}

#[derive(Serialize)]
struct ChannelHealth {
    path: String,
    count: usize,
    reload_error: Option<String>,
}

#[derive(Serialize)]
struct NoticeHealth {
    url: &'static str,
    ttl_ms: u128,
    cache: std::collections::HashMap<String, NoticeCacheItem>,
}

async fn health(State(state): State<AppState>) -> Response {
    let StoreStatus {
        path,
        count,
        reload_error,
    } = state.channels.status();
    let api_flow = state.pipeline.flow_snapshot();
    let mut stats = state.stats.snapshot();
    stats.live_info_fetches = api_flow.stats.completed;
    Json(Health {
        ok: true,
        mode: "rust-staged-mpegts",
        stats,
        channels: ChannelHealth {
            path: path.display().to_string(),
            count,
            reload_error,
        },
        notice: NoticeHealth {
            url: NOTICE_URL,
            ttl_ms: state.notices.ttl_ms(),
            cache: state.notices.snapshot(state.now_ms()),
        },
        api_flow,
        routes: ROUTES,
    })
    .into_response()
}

async fn channels(State(state): State<AppState>) -> Response {
    let status = state.channels.status();
    let index = state.channels.snapshot();
    let channels: Vec<&iptv_upstream::Channel> = index
        .channels()
        .iter()
        .map(|channel| channel.as_ref())
        .collect();
    Json(serde_json::json!({
        "ok": true,
        "path": status.path,
        "count": index.len(),
        "channels": channels,
    }))
    .into_response()
}

async fn list_m3u(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let index = state.channels.snapshot();
    let entries: Vec<PlaylistEntry> = index
        .channels()
        .iter()
        .map(|channel| PlaylistEntry {
            slug: channel.ch.clone(),
            name: channel.display_name().to_string(),
            logo: channel.logo.clone(),
            group: channel.group.clone(),
        })
        .collect();
    let style = ChannelListStyle {
        epg_url: EPG_URL,
        notice_name: NOTICE_NAME,
        notice_logo: NOTICE_LOGO_URL,
        notice_url: NOTICE_URL,
    };
    let text = build_channel_list(&entries, &style, |slug| {
        let url = abs_url(&headers, &uri, &format!("/live/{slug}.m3u8"));
        append_recursive_prefix(&uri, &url)
    });
    text_response(StatusCode::OK, PLAYLIST_TYPE, text)
}

async fn live_playlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    UrlPath(file): UrlPath<String>,
) -> Response {
    let Some(requested) = strip_suffix(&file, ".m3u8") else {
        return not_found();
    };
    state.stats.playlist_requested();
    let index = state.channels.snapshot();
    let Some(channel) = index.find_by_slug(requested) else {
        warn!(
            requested,
            "playlist requested for an unknown channel; sending the notice stream"
        );
        return notice_redirect();
    };
    if state.notices.is_active(&channel.ch, state.now_ms()) {
        debug!(channel = %channel.ch, "channel is in its notice period; sending the notice stream");
        return notice_redirect();
    }
    debug!(channel = %channel.ch, livepid = %channel.livepid, "building the channel playlist");
    let rendered = state
        .pipeline
        .local_playlist(channel, |segment| {
            let url = abs_url(
                &headers,
                &uri,
                &format!("/segment/{}/{}.ts", channel.ch, segment.id),
            );
            append_recursive_prefix(&uri, &url)
        })
        .await;
    match rendered {
        Ok(text) => {
            debug!(channel = %channel.ch, bytes = text.len(), "playlist ready");
            text_response(StatusCode::OK, PLAYLIST_TYPE, text)
        }
        Err(error) => {
            warn!(channel = %channel.ch, error = %error, debug_error = ?error, "temporary notice fallback");
            state.notices.mark(&channel.ch, state.now_ms());
            notice_redirect()
        }
    }
}

async fn segment(
    State(state): State<AppState>,
    UrlPath((ch, file)): UrlPath<(String, String)>,
) -> Response {
    let Some(id) = strip_suffix(&file, ".ts") else {
        return not_found();
    };
    state.stats.segment_requested();
    let index = state.channels.snapshot();
    let Some(channel) = index.find_by_slug(&ch) else {
        warn!(ch, id, "segment requested for an unknown channel");
        return notice_redirect();
    };
    match state.pipeline.segment(channel, id).await {
        Ok(body) => {
            debug!(channel = %channel.ch, id, bytes = body.len(), "segment streamed");
            state.stats.segment_streamed();
            let mut response = body.into_response();
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=300"),
            );
            response
        }
        Err(UpstreamError::Overloaded(busy)) => {
            warn!(channel = %busy, id, "channel queue is full; asking the player to retry");
            state.stats.segment_rejected();
            let mut response = json_status(
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({ "ok": false, "error": format!("channel {busy} is busy; retry shortly") }),
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        Err(error) => {
            warn!(channel = %channel.ch, id, error = %error, debug_error = ?error, "segment failed");
            state.stats.segment_failed();
            json_status(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({ "ok": false, "error": error.to_string() }),
            )
        }
    }
}

fn strip_suffix<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    value.strip_suffix(suffix).filter(|value| !value.is_empty())
}

fn notice_redirect() -> Response {
    Redirect::temporary(NOTICE_URL).into_response()
}

fn not_found() -> Response {
    json_status(
        StatusCode::NOT_FOUND,
        serde_json::json!({ "ok": false, "error": "not found" }),
    )
}

fn text_response(
    status: StatusCode,
    content_type: &'static str,
    text: String,
) -> Response {
    let mut response = (status, text).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json_status(status: StatusCode, value: serde_json::Value) -> Response {
    (status, Json(value)).into_response()
}

#[cfg(test)]
mod tests;
