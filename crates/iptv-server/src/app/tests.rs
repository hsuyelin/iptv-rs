use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
};
use iptv_media::testkit::{synthetic_segment, StreamSpec};
use iptv_upstream::{
    testkit::{shared_assets, ScriptedTransport, XorCipherFactory},
    FlowOptions, LiveClient, PipelineConfig,
};
use tower::ServiceExt;

use super::*;
use crate::admin::{AdminGate, AdminKey, Limits};

const ADMIN_KEY: &str = "test-admin-key-0123";

const CHANNELS: &str = r#"
channels:
  - { ch: cctv1, chinese: "CCTV-1 综合", cnlid: "1", livepid: "pid-a", group: "央视", logo: "https://cdn/1.png" }
  - { ch: cctv2, chinese: "CCTV-2 财经", cnlid: "2", livepid: "pid-b", group: "央视" }
"#;

struct App {
    router: Router,
    transport: Arc<ScriptedTransport>,
    clock: Arc<AtomicU64>,
    dir: tempfile::TempDir,
}

fn upstream_playlist(first: i64, count: i64) -> String {
    let mut text = format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for sequence in first..first + count {
        text.push_str(&format!(
            "#EXTINF:2.000,\nhttps://cdn.test/a/seg_{sequence}.ts\n"
        ));
    }
    text
}

fn app_with(config: PipelineConfig, web_dir: Option<&Path>) -> App {
    let dir = tempfile::tempdir().unwrap();
    let channels_path = dir.path().join("channels.yaml");
    fs::write(&channels_path, CHANNELS).unwrap();
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", r#"{"code":0,"data":{"token":"T","ts":"1"}}"#)
        .ok("web/open/token", r#"{"data":{"token":"O"}}"#)
        .ok(
            "get_live_info",
            r#"{"code":0,"data":{"iretcode":0,"playurl":"https://cdn.test/a/index.m3u8?s=1"}}"#,
        )
        .ok("a/index.m3u8", upstream_playlist(100, 16))
        .ok("a/seg_", synthetic_segment(&StreamSpec::SMALL));
    let live = LiveClient::new(
        transport.clone(),
        &shared_assets(),
        FlowOptions {
            concurrency: 1,
            min_interval: Duration::ZERO,
            jitter: Duration::ZERO,
            queue_timeout: Duration::from_secs(30),
            retries: 0,
            retry_delay: Duration::ZERO,
        },
    );
    let pipeline = MediaPipeline::new(
        live,
        transport.clone(),
        Arc::new(XorCipherFactory::new()),
        config,
    );
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let reader = clock.clone();
    let store = Arc::new(ChannelStore::load(&channels_path, Duration::ZERO).unwrap());
    let admin_clock: Clock = {
        let reader = clock.clone();
        Arc::new(move || u128::from(reader.load(Ordering::SeqCst)))
    };
    let admin = Arc::new(AdminGate::new(
        AdminKey::parse(ADMIN_KEY).unwrap(),
        Limits {
            failure_delay: Duration::ZERO,
            ..Limits::default()
        },
        admin_clock,
    ));
    let state = AppState::new(
        store,
        pipeline,
        admin,
        Arc::new(move || u128::from(reader.load(Ordering::SeqCst))),
    );
    App {
        router: router(state, web_dir),
        transport,
        clock,
        dir,
    }
}

fn app() -> App {
    app_with(PipelineConfig::default(), None)
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: bytes::Bytes,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

async fn get(router: &Router, uri: &str) -> Reply {
    get_with(router, uri, &[("host", "relay.test:8787")]).await
}

async fn get_with(router: &Router, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Reply {
        status,
        headers,
        body,
    }
}

fn segment_paths(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            line.trim_start_matches("http://relay.test:8787")
                .to_string()
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn channel_list_is_m3u_with_absolute_stream_urls() {
    let app = app();
    let reply = get(&app.router, "/list.m3u").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl; charset=utf-8"
    );
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let text = reply.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], format!("#EXTM3U x-tvg-url=\"{EPG_URL}\""));
    assert!(lines[1].contains("注意事项"));
    assert_eq!(lines[2], NOTICE_URL);
    assert!(lines[3].contains("tvg-name=\"CCTV-1 综合\""));
    assert!(lines[3].ends_with("group-title=\"央视\",CCTV-1 综合"));
    assert_eq!(lines[4], "http://relay.test:8787/live/cctv1.m3u8");
    assert_eq!(lines[6], "http://relay.test:8787/live/cctv2.m3u8");
    assert_eq!(
        app.transport.requests().len(),
        0,
        "listing needs no upstream call"
    );
}

#[tokio::test(start_paused = true)]
async fn explicit_prefix_is_carried_into_every_entry() {
    let app = app();
    let reply = get(&app.router, "/list.m3u?prefix=https%3A%2F%2Ftv.example.com").await;
    let text = reply.text();
    assert!(text.contains(
        "https://tv.example.com/live/cctv1.m3u8?prefix=https%3A%2F%2Ftv.example.com"
    ));
}

#[tokio::test(start_paused = true)]
async fn unknown_suffixes_are_json_404s() {
    let app = app();
    for uri in [
        "/live/cctv1.txt",
        "/live/.m3u8",
        "/segment/cctv1/abc.mp4",
        "/segment/cctv1/.ts",
    ] {
        let reply = get(&app.router, uri).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            reply.json(),
            serde_json::json!({"ok": false, "error": "not found"})
        );
    }
    let reply = get(&app.router, "/nothing-here").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test(start_paused = true)]
async fn unknown_channels_redirect_to_the_notice_stream() {
    let app = app();
    for uri in ["/live/nope.m3u8", "/segment/nope/abc.ts"] {
        let reply = get(&app.router, uri).await;
        assert_eq!(reply.status, StatusCode::TEMPORARY_REDIRECT, "{uri}");
        assert_eq!(reply.headers[header::LOCATION], NOTICE_URL);
    }
}

#[tokio::test(start_paused = true)]
async fn live_playlist_lists_local_segment_urls() {
    let app = app();
    let reply = get(&app.router, "/live/cctv1.m3u8").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let text = reply.text();
    assert!(text.starts_with("#EXTM3U\n#EXT-X-VERSION:3\n"));
    let paths = segment_paths(&text);
    assert_eq!(paths.len(), 12);
    assert!(paths
        .iter()
        .all(|p| p.starts_with("/segment/cctv1/") && p.ends_with(".ts")));
    // Slug lookup ignores case.
    assert_eq!(
        get(&app.router, "/live/CCTV1.m3u8").await.status,
        StatusCode::OK
    );
}

#[tokio::test(start_paused = true)]
async fn segments_stream_with_media_headers_and_are_counted() {
    let app = app();
    let playlist = get(&app.router, "/live/cctv1.m3u8").await.text();
    let newest = segment_paths(&playlist).pop().unwrap();
    let reply = get(&app.router, &newest).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(reply.headers[header::CACHE_CONTROL], "public, max-age=300");
    assert_eq!(reply.body.len() % 188, 0);
    assert!(!reply.body.is_empty());

    let health = get(&app.router, "/health").await.json();
    assert_eq!(health["ok"], true);
    assert_eq!(health["mode"], "rust-staged-mpegts");
    assert_eq!(health["stats"]["playlist_requests"], 1);
    assert_eq!(health["stats"]["segment_requests"], 1);
    assert_eq!(health["stats"]["segment_streamed"], 1);
    assert_eq!(health["stats"]["segment_errors"], 0);
    assert_eq!(health["stats"]["live_info_fetches"], 1);
    assert_eq!(health["channels"]["count"], 2);
    assert!(health["channels"]["reload_error"].is_null());
    assert_eq!(health["notice"]["url"], NOTICE_URL);
    assert!(health["api_flow"]["completed"].as_u64().unwrap() >= 1);
    assert_eq!(health["routes"].as_array().unwrap().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn foreign_and_unknown_segment_ids_are_bad_gateway_and_counted() {
    let app = app();
    let playlist = get(&app.router, "/live/cctv1.m3u8").await.text();
    let newest = segment_paths(&playlist).pop().unwrap();
    let id = newest.rsplit('/').next().unwrap();
    // The id belongs to cctv1; asking for it under cctv2 is refused.
    let reply = get(&app.router, &format!("/segment/cctv2/{id}")).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(reply.json()["ok"], false);
    let reply = get(&app.router, "/segment/cctv1/deadbeef.ts").await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    let health = get(&app.router, "/health").await.json();
    assert_eq!(health["stats"]["segment_errors"], 2);
    assert_eq!(health["stats"]["segment_streamed"], 0);
}

#[tokio::test(start_paused = true)]
async fn failing_channels_redirect_until_the_notice_ttl_passes() {
    let app = app();
    app.transport.set_status("a/index.m3u8", 500, "down");
    let first = get(&app.router, "/live/cctv1.m3u8").await;
    assert_eq!(first.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(first.headers[header::LOCATION], NOTICE_URL);
    let upstream_calls = app.transport.count("a/index.m3u8");

    // Within the TTL no upstream call is made.
    app.clock
        .fetch_add(NOTICE_CACHE_TTL_MS - 1, Ordering::SeqCst);
    let again = get(&app.router, "/live/cctv1.m3u8").await;
    assert_eq!(again.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(app.transport.count("a/index.m3u8"), upstream_calls);
    let health = get(&app.router, "/health").await.json();
    assert!(
        health["notice"]["cache"]["cctv1"]["ttl_ms"]
            .as_u64()
            .unwrap()
            <= 1
    );

    // The channel recovers and the next request after expiry reaches the upstream.
    app.transport
        .set_ok("a/index.m3u8", upstream_playlist(100, 16));
    app.clock.fetch_add(2, Ordering::SeqCst);
    let recovered = get(&app.router, "/live/cctv1.m3u8").await;
    assert_eq!(recovered.status, StatusCode::OK);
    assert!(app.transport.count("a/index.m3u8") > upstream_calls);
}

#[tokio::test(start_paused = true)]
async fn channels_endpoint_lists_the_loaded_channels() {
    let app = app();
    let reply = get(&app.router, "/channels").await.json();
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["count"], 2);
    assert_eq!(reply["channels"][0]["ch"], "cctv1");
    assert_eq!(reply["channels"][0]["group"], "央视");
    assert!(reply["path"].as_str().unwrap().ends_with("channels.yaml"));
}

#[tokio::test(start_paused = true)]
async fn edited_channel_files_are_served_without_a_restart() {
    let app = app();
    let path = app.dir.path().join("channels.yaml");
    let extended =
        format!("{CHANNELS}  - {{ ch: cctv3, cnlid: \"3\", livepid: \"pid-c\" }}\n");
    fs::write(&path, extended).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    assert_eq!(get(&app.router, "/channels").await.json()["count"], 3);

    // A broken edit keeps the last good list and is reported by /health.
    fs::write(&path, "channels: [oops").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(120))
        .unwrap();
    assert_eq!(get(&app.router, "/channels").await.json()["count"], 3);
    let health = get(&app.router, "/health").await.json();
    assert!(health["channels"]["reload_error"]
        .as_str()
        .unwrap()
        .contains("cannot parse"));
}

#[tokio::test(start_paused = true)]
async fn a_busy_channel_answers_429_with_retry_after_and_health_stays_responsive() {
    let app = app_with(
        PipelineConfig {
            queue_capacity: 1,
            ..PipelineConfig::default()
        },
        None,
    );
    let playlist = get(&app.router, "/live/cctv1.m3u8").await.text();
    let paths = segment_paths(&playlist);
    let gate = app.transport.gate("a/seg_");
    let mut parked = Vec::new();
    for path in [&paths[11], &paths[10]] {
        let (router, path) = (app.router.clone(), path.clone());
        parked.push(tokio::spawn(async move { get(&router, &path).await }));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let busy = get(&app.router, &paths[9]).await;
    assert_eq!(busy.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(busy.headers[header::RETRY_AFTER], "1");
    assert_eq!(busy.json()["ok"], false);

    // Reading health is not delayed by the parked segment work.
    let health = get(&app.router, "/health").await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json()["stats"]["segment_rejected"], 1);

    gate.add_permits(100);
    for handle in parked {
        assert_eq!(handle.await.unwrap().status, StatusCode::OK);
    }
}

#[tokio::test(start_paused = true)]
async fn no_web_dir_means_no_static_routes() {
    let app = app();
    assert_eq!(get(&app.router, "/").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&app.router, "/index.html").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(start_paused = true)]
async fn web_dir_is_served_while_api_routes_win() {
    let web = tempfile::tempdir().unwrap();
    fs::write(web.path().join("index.html"), "<h1>console</h1>").unwrap();
    // A file that shadows an API path must lose to the API route.
    fs::create_dir(web.path().join("channels")).unwrap();
    let app = app_with(PipelineConfig::default(), Some(web.path()));
    let index = get(&app.router, "/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.text().contains("console"));
    let health = get(&app.router, "/health").await;
    assert_eq!(health.json()["ok"], true);
    assert_eq!(get(&app.router, "/channels").await.json()["ok"], true);
    assert_eq!(
        get(&app.router, "/missing.js").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(start_paused = true)]
async fn cors_is_permissive_for_browser_clients() {
    let app = app();
    let reply = get_with(
        &app.router,
        "/channels",
        &[("origin", "http://localhost:5173"), ("host", "relay.test")],
    )
    .await;
    assert_eq!(reply.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
}

async fn post_key(router: &Router, body: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder()
        .method("POST")
        .uri("/admin/verify")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Reply {
        status,
        headers,
        body,
    }
}

fn key_body(key: &str) -> String {
    serde_json::json!({ "key": key }).to_string()
}

#[tokio::test]
async fn the_admin_check_accepts_the_key_and_refuses_everything_else() {
    let app = app();
    let good = post_key(&app.router, &key_body(ADMIN_KEY), &[]).await;
    assert_eq!(good.status, StatusCode::OK);
    assert_eq!(good.json()["ok"], true);
    assert_eq!(good.headers[header::CACHE_CONTROL], "no-store");

    let wrong = post_key(&app.router, &key_body("not-the-key"), &[]).await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    assert_eq!(wrong.json()["ok"], false);
    // Each of these comes from a different client, so none of them reaches the lock.
    for (index, body) in ["", "not json", "{}", r#"{"key": 5}"#, r#"{"other": "x"}"#]
        .into_iter()
        .enumerate()
    {
        let client = format!("198.51.100.{}", 100 + index);
        let reply = post_key(&app.router, body, &[("x-forwarded-for", &client)]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{body}");
    }
}

#[tokio::test]
async fn the_admin_check_is_post_only_and_caps_the_body() {
    let app = app();
    assert_eq!(
        get(&app.router, "/admin/verify").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let huge = "x".repeat(10_000);
    assert_eq!(
        post_key(&app.router, &key_body(&huge), &[]).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn repeated_wrong_keys_lock_the_client_out_with_retry_after() {
    let app = app();
    let client = [("x-forwarded-for", "198.51.100.20")];
    for _ in 0..5 {
        let reply = post_key(&app.router, &key_body("guess"), &client).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }
    // Locked: even the right key is refused.
    let locked = post_key(&app.router, &key_body(ADMIN_KEY), &client).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(locked.headers[header::RETRY_AFTER], "900");
    assert_eq!(locked.json()["retry_after"], 900);
    assert_eq!(locked.json()["ok"], false);

    // Someone else, named by the proxy, is unaffected.
    let other = [("x-forwarded-for", "198.51.100.21")];
    assert_eq!(
        post_key(&app.router, &key_body(ADMIN_KEY), &other)
            .await
            .status,
        StatusCode::OK
    );

    // After the lock ends the right key works again.
    app.clock.fetch_add(15 * 60 * 1000 + 1000, Ordering::SeqCst);
    assert_eq!(
        post_key(&app.router, &key_body(ADMIN_KEY), &client)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test(start_paused = true)]
async fn console_page_addresses_serve_the_console_and_files_stay_files() {
    let web = tempfile::tempdir().unwrap();
    fs::write(web.path().join("index.html"), "<h1>console</h1>").unwrap();
    fs::create_dir(web.path().join("assets")).unwrap();
    fs::write(web.path().join("assets/app.js"), "console.log(1)").unwrap();
    let app = app_with(PipelineConfig::default(), Some(web.path()));

    let page = get(&app.router, &format!("/{ADMIN_KEY}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("console"));
    assert_eq!(
        page.headers[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-cache");
    // Any single word is a page address; the console decides what it means.
    assert_eq!(get(&app.router, "/whatever").await.status, StatusCode::OK);

    assert_eq!(
        get(&app.router, "/assets/app.js").await.text(),
        "console.log(1)"
    );
    assert_eq!(
        get(&app.router, "/assets/missing.js").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app.router, "/x.js").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(start_paused = true)]
async fn an_unknown_page_gets_the_console_with_a_404_and_files_get_a_bare_one() {
    let web = tempfile::tempdir().unwrap();
    fs::write(web.path().join("index.html"), "<h1>console</h1>").unwrap();
    let with_console = app_with(PipelineConfig::default(), Some(web.path()));

    let page = get(&with_console.router, "/a/b").await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    assert!(page.text().contains("console"));
    assert_eq!(
        page.headers[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-cache");

    // A missing file is a plain 404, not a page.
    let file = get(&with_console.router, "/assets/missing.js").await;
    assert_eq!(file.status, StatusCode::NOT_FOUND);
    assert!(!file.text().contains("console"));
    // Without a console there is nothing to draw the page with.
    assert_eq!(
        get(&app().router, "/a/b").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn without_a_web_dir_a_key_path_is_just_not_found() {
    let app = app();
    assert_eq!(
        get(&app.router, &format!("/{ADMIN_KEY}")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> LogBuffer {
        self.clone()
    }
}

/// One process-wide logger shared by the whole test binary. A per-test logger would miss
/// events whose call sites other tests already hit while no logger was installed.
fn captured_logs() -> LogBuffer {
    static BUFFER: std::sync::OnceLock<LogBuffer> = std::sync::OnceLock::new();
    BUFFER
        .get_or_init(|| {
            let buffer = LogBuffer::default();
            let _ = tracing::subscriber::set_global_default(crate::logging::subscriber(
                buffer.clone(),
                tracing_subscriber::EnvFilter::new("debug"),
                false,
            ));
            buffer
        })
        .clone()
}

#[tokio::test]
async fn logs_describe_requests_but_never_show_the_key_or_a_guess() {
    let buffer = captured_logs();
    let web = tempfile::tempdir().unwrap();
    fs::write(web.path().join("index.html"), "<h1>console</h1>").unwrap();
    let app = app_with(PipelineConfig::default(), Some(web.path()));

    get(&app.router, &format!("/{ADMIN_KEY}")).await;
    post_key(&app.router, &key_body("a-secret-guess-value"), &[]).await;
    post_key(&app.router, &key_body(ADMIN_KEY), &[]).await;
    get(&app.router, "/channels").await;

    let log = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(
        !log.contains(ADMIN_KEY),
        "the key leaked into the log:\n{log}"
    );
    assert!(
        !log.contains("a-secret-guess-value"),
        "a guess leaked:\n{log}"
    );
    assert!(log.contains("<static>"), "{log}");
    assert!(log.contains("request served"), "{log}");
    assert!(
        log.contains("route=\"/channels\"") || log.contains("route=/channels"),
        "{log}"
    );
    assert!(log.contains("administrator key rejected"), "{log}");
    assert!(log.contains("administrator key accepted"), "{log}");
    assert!(log.contains("crates/iptv-server/src/app.rs:"), "{log}");
    assert!(log.contains("status=200"), "{log}");
    assert!(log.contains("status=403"), "{log}");
}
