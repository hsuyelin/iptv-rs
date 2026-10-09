#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use iptv_media::testkit::{synthetic_segment, StreamSpec};
use iptv_upstream::{
    build_ckey, md5_js_default_sorted_with_secret,
    testkit::{shared_assets, ScriptedTransport, XorCipherFactory},
    ApiFlowLimiter, Channel, FlowOptions, LiveClient, MediaPipeline, PipelineConfig,
};

fn signing(c: &mut Criterion) {
    c.bench_function("sign/build_ckey", |b| {
        b.iter(|| {
            build_ckey(
                black_box("2027249301"),
                1_778_337_597,
                "moygaemw_oj9xhxuw53",
            )
        });
    });
    let body: Vec<(String, String)> = (0..17)
        .map(|i| (format!("key{i:02}"), format!("value-{i}-%41")))
        .collect();
    c.bench_function("sign/md5_17_keys", |b| {
        b.iter(|| {
            md5_js_default_sorted_with_secret(
                black_box(body.iter().map(|(k, v)| (k.clone(), v.clone()))),
                "secret",
            )
        });
    });
}

fn flow(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let limiter = ApiFlowLimiter::new(FlowOptions {
        concurrency: 4,
        min_interval: Duration::ZERO,
        jitter: Duration::ZERO,
        queue_timeout: Duration::from_secs(5),
        retries: 0,
        retry_delay: Duration::ZERO,
    });
    c.bench_function("flow/run_overhead", |b| {
        b.iter(|| runtime.block_on(limiter.run(async { Ok(black_box(1u32)) })));
    });
}

fn pipeline(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let transport = Arc::new(ScriptedTransport::new());
    let mut playlist = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n");
    for sequence in 100..160 {
        playlist.push_str(&format!(
            "#EXTINF:2.000,\nhttps://cdn.test/a/seg_{sequence}.ts\n"
        ));
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
    let channel = Channel {
        ch: "a".into(),
        logo: String::new(),
        chinese: String::new(),
        cnlid: "1".into(),
        livepid: "pid-a".into(),
        group: String::new(),
    };
    let text = runtime
        .block_on(pipeline.local_playlist(&channel, |s| s.id.clone()))
        .unwrap();
    let newest = text
        .lines()
        .rfind(|l| !l.starts_with('#'))
        .unwrap()
        .to_string();
    // Warm the worker so the benchmark measures the cached path.
    runtime
        .block_on(pipeline.segment(&channel, &newest))
        .unwrap();

    c.bench_function("pipeline/local_playlist_60_segments", |b| {
        b.iter(|| runtime.block_on(pipeline.local_playlist(&channel, |s| s.id.clone())));
    });
    c.bench_function("pipeline/cached_segment", |b| {
        b.iter(|| runtime.block_on(pipeline.segment(&channel, black_box(&newest))));
    });
}

criterion_group!(benches, signing, flow, pipeline);
criterion_main!(benches);
