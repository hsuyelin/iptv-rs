use std::sync::Arc;

use super::*;
use crate::testkit::{shared_assets, ScriptedTransport};

fn sample_entry(now: u128) -> SourceCacheEntry {
    SourceCacheEntry {
        ch: "cctv1".to_string(),
        cnlid: "2024078201".to_string(),
        livepid: "600001859".to_string(),
        cache_key: "2024078201:600001859".to_string(),
        guid: "guid".to_string(),
        url: "https://example.test/live.m3u8".to_string(),
        fetched_at_ms: now,
        refresh_after_ms: now + 10,
        expires_at_ms: now + 20,
        stale_until_ms: now + 40,
    }
}

fn channel() -> Channel {
    Channel {
        ch: "cctv1".into(),
        logo: String::new(),
        chinese: "CCTV-1".into(),
        cnlid: "2024078201".into(),
        livepid: "600001859".into(),
        group: String::new(),
    }
}

fn flow() -> FlowOptions {
    FlowOptions {
        concurrency: 1,
        min_interval: Duration::ZERO,
        jitter: Duration::ZERO,
        queue_timeout: Duration::from_secs(10),
        retries: 2,
        retry_delay: Duration::ZERO,
    }
}

const AUTH_OK: &str = r#"{"code":0,"data":{"token":"AUTHSECRET","ts":"1778337597"}}"#;
const TOKEN_OK: &str = r#"{"data":{"token":"OPENSECRET","expire":600}}"#;
const LIVE_OK: &str = r#"{"code":0,"data":{"iretcode":0,"playurl":"https://cdn.test/live/index.m3u8?sig=abc","extended_param":"&x=1","chanll":{"code":"ImFiYyI="}}}"#;

fn scripted() -> Arc<ScriptedTransport> {
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK)
        .ok("get_live_info", LIVE_OK);
    transport
}

fn client(transport: &Arc<ScriptedTransport>) -> LiveClient {
    LiveClient::new(transport.clone(), &shared_assets(), flow())
}

#[test]
fn stale_source_is_returned_while_refresh_is_needed() {
    let mut cache = SourceCacheState::default();
    let entry = sample_entry(100);
    cache.entries.insert(entry.cache_key.clone(), entry.clone());
    match lookup_cached_source(&mut cache, &entry.cache_key, 125) {
        CacheLookup::Hit {
            entry: hit,
            refresh,
        } => {
            assert!(refresh);
            assert_eq!(hit.url, entry.url);
        }
        CacheLookup::Miss => panic!("stale source should still be usable"),
    }
}

#[test]
fn fresh_source_needs_no_refresh_until_its_refresh_time() {
    let mut cache = SourceCacheState::default();
    let entry = sample_entry(100);
    cache.entries.insert(entry.cache_key.clone(), entry.clone());
    assert!(matches!(
        lookup_cached_source(&mut cache, &entry.cache_key, 105),
        CacheLookup::Hit { refresh: false, .. }
    ));
    assert!(matches!(
        lookup_cached_source(&mut cache, &entry.cache_key, 110),
        CacheLookup::Hit { refresh: true, .. }
    ));
    assert!(matches!(
        lookup_cached_source(&mut cache, "unknown", 105),
        CacheLookup::Miss
    ));
}

#[test]
fn source_is_removed_after_stale_grace() {
    let mut cache = SourceCacheState::default();
    let entry = sample_entry(100);
    cache.entries.insert(entry.cache_key.clone(), entry.clone());
    assert!(matches!(
        lookup_cached_source(&mut cache, &entry.cache_key, 145),
        CacheLookup::Miss
    ));
    assert!(!cache.entries.contains_key(&entry.cache_key));
}

#[test]
fn revoi_is_decoded_from_object_or_json_string() {
    let object = serde_json::json!({ "code": "ImFiYyI=" });
    assert_eq!(decode_revoi(Some(&object)), "abc");
    let text = serde_json::Value::String(r#"{"code":"ImFiYyI="}"#.to_string());
    assert_eq!(decode_revoi(Some(&text)), "abc");
    assert_eq!(decode_revoi(None), "");
    assert_eq!(
        decode_revoi(Some(&serde_json::json!({ "code": "!!!" }))),
        ""
    );
    assert_eq!(decode_revoi(Some(&serde_json::json!(5))), "");
}

#[test]
fn playback_url_appends_revoi_and_extended_param() {
    let data = LiveInfoData {
        playurl: "https://h/p.m3u8?a=1".into(),
        extended_param: "&x=1".into(),
        chanll: Some(serde_json::json!({ "code": "ImFiYyI=" })),
        ..LiveInfoData::default()
    };
    assert_eq!(
        build_playback_url(&data),
        "https://h/p.m3u8?a=1&revoi=abc&x=1"
    );
}

#[test]
fn cookie_and_headers_carry_the_guid() {
    let cookie = build_cookie("g1");
    assert!(cookie.starts_with("guid=g1; "));
    let headers = base_headers("g1");
    assert!(headers.iter().any(|(k, v)| k == "cookie" && v == &cookie));
    assert!(headers
        .iter()
        .any(|(k, v)| k == "origin" && v == ACTIVE_URL));
}

#[test]
fn source_entry_debug_hides_the_url() {
    let rendered = format!("{:?}", sample_entry(1));
    assert!(!rendered.contains("example.test"));
}

#[tokio::test(start_paused = true)]
async fn fetches_a_source_with_signed_requests_and_caches_it() {
    let transport = scripted();
    let live = client(&transport);
    let entry = live.fetch_source(channel()).await.unwrap();
    assert_eq!(
        entry.url,
        "https://cdn.test/live/index.m3u8?sig=abc&revoi=abc&x=1"
    );
    assert_eq!(entry.cache_key, "2024078201:600001859");

    let requests = transport.requests();
    let urls: Vec<&str> = requests.iter().map(|r| r.url.as_str()).collect();
    assert!(urls.iter().any(|u| u.ends_with("v1/player/auth")));
    assert!(urls.iter().any(|u| u.contains("web/open/token")));
    let live_info = requests
        .iter()
        .find(|r| r.url.ends_with("v1/player/get_live_info"))
        .unwrap();
    assert_eq!(live_info.method, Method::Post);
    for header in [
        "yspticket",
        "yspsdksign",
        "yspsdkinput",
        "yspPlayerToken",
        "request-id",
        "cookie",
    ] {
        assert!(
            live_info.header(header).is_some_and(|v| !v.is_empty()),
            "{header}"
        );
    }
    assert_eq!(live_info.header("yspPlayerToken"), Some("AUTHSECRET"));
    let RequestBody::Json(body) = &live_info.body else {
        panic!("live info body must be JSON");
    };
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(body["livepid"], "600001859");
    assert!(body["signature"].as_str().is_some_and(|s| s.len() == 32));
    assert!(body["cKey"].as_str().is_some_and(|s| s.starts_with("--01")));

    // A second call is answered from the cache.
    let before = transport.requests().len();
    live.fetch_source(channel()).await.unwrap();
    assert_eq!(transport.requests().len(), before);
}

#[tokio::test(start_paused = true)]
async fn retryable_failures_are_retried_and_others_are_not() {
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK)
        .status("get_live_info", 503, "busy")
        .ok("get_live_info", LIVE_OK);
    let entry = client(&transport).fetch_source(channel()).await.unwrap();
    assert!(entry.url.contains("revoi=abc"));
    assert_eq!(transport.count("get_live_info"), 2);

    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK)
        .status("get_live_info", 400, "bad request");
    let error = client(&transport)
        .fetch_source(channel())
        .await
        .unwrap_err();
    assert!(matches!(error, UpstreamError::Status { status: 400, .. }));
    assert_eq!(transport.count("get_live_info"), 1);
}

#[tokio::test(start_paused = true)]
async fn persistent_retryable_failure_stops_after_the_retry_budget() {
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK)
        .status("get_live_info", 503, "busy");
    let error = client(&transport)
        .fetch_source(channel())
        .await
        .unwrap_err();
    assert!(matches!(error, UpstreamError::Status { status: 503, .. }));
    assert_eq!(transport.count("get_live_info"), API_FLOW_RETRIES + 1);
}

#[tokio::test(start_paused = true)]
async fn rejected_responses_and_bad_json_are_typed_errors() {
    let transport = Arc::new(ScriptedTransport::new());
    transport.status("v1/player/auth", 200, r#"{"code":7,"data":null}"#);
    let error = client(&transport)
        .fetch_source(channel())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        UpstreamError::Rejected { what: "auth", .. }
    ));

    let transport = Arc::new(ScriptedTransport::new());
    transport.ok("v1/player/auth", "<html>nope</html>");
    let error = client(&transport)
        .fetch_source(channel())
        .await
        .unwrap_err();
    assert!(matches!(error, UpstreamError::Parse { what: "auth", .. }));

    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK)
        .ok(
            "get_live_info",
            r#"{"code":0,"data":{"iretcode":0,"playurl":""}}"#,
        );
    let error = client(&transport)
        .fetch_source(channel())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        UpstreamError::Rejected {
            what: "get_live_info",
            ..
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn errors_and_debug_output_never_contain_tokens() {
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK)
        .status("get_live_info", 403, "forbidden");
    let error = client(&transport)
        .fetch_source(channel())
        .await
        .unwrap_err();
    for text in [format!("{error}"), format!("{error:?}")] {
        assert!(!text.contains("AUTHSECRET"), "{text}");
        assert!(!text.contains("OPENSECRET"), "{text}");
    }
    let requests = format!("{:?}", transport.requests());
    assert!(!requests.contains("AUTHSECRET"));
}

#[tokio::test(start_paused = true)]
async fn invalidated_sources_are_refreshed_on_next_use() {
    let transport = scripted();
    let live = client(&transport);
    let entry = live.fetch_source(channel()).await.unwrap();
    live.invalidate_source(&entry.cache_key);
    // The stale entry is served at once while a refresh runs in the background.
    let served = live.fetch_source(channel()).await.unwrap();
    assert_eq!(served.url, entry.url);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(transport.count("get_live_info"), 2);
    assert!(live.flow_snapshot().stats.completed >= 2);
}

#[tokio::test(start_paused = true)]
async fn timeouts_and_connection_failures_are_retried() {
    use crate::transport::{TransportError, TransportErrorKind};
    for kind in [TransportErrorKind::Timeout, TransportErrorKind::Connect] {
        let transport = Arc::new(ScriptedTransport::new());
        transport
            .ok("v1/player/auth", AUTH_OK)
            .ok("web/open/token", TOKEN_OK)
            .respond(
                "get_live_info",
                Err(TransportError::new(kind.clone(), "transport failed")),
            )
            .ok("get_live_info", LIVE_OK);
        let result = client(&transport).fetch_source(channel()).await;
        // Timeouts are retried; a refused connection is not.
        assert_eq!(
            result.is_ok(),
            kind == TransportErrorKind::Timeout,
            "{kind:?}"
        );
    }
}
