#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::{fmt::Write as _, fs, sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use iptv_media::testkit::{synthetic_segment, StreamSpec};
use iptv_server::{
    router, system_clock, AdminGate, AdminKey, AppState, ChannelIndex, ChannelStore,
};
use iptv_upstream::{
    testkit::{shared_assets, ScriptedTransport, XorCipherFactory},
    FlowOptions, LiveClient, MediaPipeline, PipelineConfig,
};
use tower::ServiceExt;

fn channels_yaml(count: usize) -> String {
    let mut text = String::from("channels:\n");
    for i in 0..count {
        let _ = writeln!(
            text,
            "  - {{ ch: channel{i}, chinese: \"Channel {i}\", cnlid: \"{i}\", livepid: \"pid-{i}\", group: \"G{}\", logo: \"https://cdn.example.com/{i}.png\" }}",
            i % 7
        );
    }
    text
}

fn lookup(c: &mut Criterion) {
    let yaml = channels_yaml(200);
    let index = ChannelIndex::parse(&yaml, std::path::Path::new("bench.yaml")).unwrap();
    c.bench_function("config/find_by_slug_200", |b| {
        b.iter(|| index.find_by_slug(black_box("channel137")));
    });
    c.bench_function("config/find_by_slug_mixed_case_200", |b| {
        b.iter(|| index.find_by_slug(black_box("Channel137")));
    });
    c.bench_function("config/parse_yaml_200", |b| {
        b.iter(|| {
            ChannelIndex::parse(black_box(&yaml), std::path::Path::new("bench.yaml"))
        });
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channels.yaml");
    fs::write(&path, &yaml).unwrap();
    let store = ChannelStore::load(&path, Duration::from_secs(1)).unwrap();
    c.bench_function("config/store_snapshot_hot", |b| {
        b.iter(|| store.snapshot());
    });
}

async fn call(router: &Router, uri: &str) -> usize {
    let request = Request::builder()
        .uri(uri)
        .header("host", "relay.bench:8787")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .len()
}

fn routes(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channels.yaml");
    fs::write(&path, channels_yaml(200)).unwrap();

    let transport = Arc::new(ScriptedTransport::new());
    let mut playlist = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n");
    for sequence in 100..160 {
        let _ = write!(
            playlist,
            "#EXTINF:2.000,\nhttps://cdn.test/a/seg_{sequence}.ts\n"
        );
    }
    transport
        .ok("v1/player/auth", r#"{"code":0,"data":{"token":"T","ts":"1"}}"#)
        .ok("web/open/token", r#"{"data":{"token":"O"}}"#)
        .ok(
            "get_live_info",
            r#"{"code":0,"data":{"iretcode":0,"playurl":"https://cdn.test/a/index.m3u8?s=1"}}"#,
        )
        .ok("a/index.m3u8", playlist)
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
        transport,
        Arc::new(XorCipherFactory::new()),
        PipelineConfig::default(),
    );
    let store = Arc::new(ChannelStore::load(&path, Duration::from_secs(1)).unwrap());
    let app = router(
        AppState::new(
            store,
            pipeline,
            Arc::new(AdminGate::with_defaults(
                AdminKey::generate(),
                system_clock(),
            )),
            system_clock(),
        ),
        None,
    );

    // Warm the channel so the benchmarks measure the steady state.
    let playlist = runtime.block_on(async {
        let request = Request::builder()
            .uri("/live/channel1.m3u8")
            .header("host", "relay.bench:8787")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    });
    let newest = playlist
        .lines()
        .rfind(|line| !line.starts_with('#'))
        .unwrap()
        .trim_start_matches("http://relay.bench:8787")
        .to_string();
    runtime.block_on(call(&app, &newest));

    c.bench_function("router/list_m3u_200", |b| {
        b.iter(|| runtime.block_on(call(&app, "/list.m3u")));
    });
    c.bench_function("router/channels_json_200", |b| {
        b.iter(|| runtime.block_on(call(&app, "/channels")));
    });
    c.bench_function("router/health", |b| {
        b.iter(|| runtime.block_on(call(&app, "/health")));
    });
    c.bench_function("router/live_playlist", |b| {
        b.iter(|| runtime.block_on(call(&app, "/live/channel1.m3u8")));
    });
    c.bench_function("router/cached_segment", |b| {
        b.iter(|| runtime.block_on(call(&app, black_box(&newest))));
    });
}

criterion_group!(benches, lookup, routes);
criterion_main!(benches);
