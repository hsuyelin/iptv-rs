use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path as UrlPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use iptv_media::{build_channel_list, ChannelListStyle, PlaylistEntry};
use iptv_upstream::{FlowSnapshot, MediaPipeline, UpstreamError};
use serde::Serialize;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::warn;

use crate::{
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
    clock: Clock,
}

impl AppState {
    /// Assembles the state from its collaborators.
    pub fn new(
        channels: Arc<ChannelStore>,
        pipeline: MediaPipeline,
        clock: Clock,
    ) -> Self {
        Self {
            stats: Arc::new(Stats::new(clock())),
            notices: Arc::new(NoticeCache::new(NOTICE_CACHE_TTL_MS)),
            channels,
            pipeline,
            clock,
        }
    }

    fn now_ms(&self) -> u128 {
        (self.clock)()
    }
}

/// Builds the HTTP router. When `web_dir` is set, files from it answer every path that is
/// not an API route.
pub fn router(state: AppState, web_dir: Option<&Path>) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/channels", get(channels))
        .route("/list.m3u", get(list_m3u))
        .route("/live/{file}", get(live_playlist))
        .route("/segment/{ch}/{file}", get(segment))
        .with_state(state);
    let app = match web_dir {
        Some(dir) => api.fallback_service(ServeDir::new(dir)),
        None => api,
    };
    app.layer(CorsLayer::permissive())
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
        return notice_redirect();
    };
    if state.notices.is_active(&channel.ch, state.now_ms()) {
        return notice_redirect();
    }
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
        Ok(text) => text_response(StatusCode::OK, PLAYLIST_TYPE, text),
        Err(error) => {
            warn!(channel = %channel.ch, error = %error, "temporary notice fallback");
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
        return notice_redirect();
    };
    match state.pipeline.segment(channel, id).await {
        Ok(body) => {
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
