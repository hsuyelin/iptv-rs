use std::{sync::Arc, time::Duration};

use iptv_media::{
    segment_id,
    testkit::{synthetic_segment, StreamSpec},
};

use super::*;
use crate::{
    flow::FlowOptions,
    testkit::{shared_assets, ScriptedTransport, XorCipherFactory},
};

const AUTH_OK: &str = r#"{"code":0,"data":{"token":"AUTH","ts":"1778337597"}}"#;
const TOKEN_OK: &str = r#"{"data":{"token":"OPEN"}}"#;

fn live_ok(host_path: &str) -> String {
    format!(
        r#"{{"code":0,"data":{{"iretcode":0,"playurl":"https://cdn.test/{host_path}/index.m3u8?sig=1"}}}}"#
    )
}

fn channel(slug: &str, livepid: &str) -> Channel {
    Channel {
        ch: slug.into(),
        logo: String::new(),
        chinese: slug.into(),
        cnlid: "100".into(),
        livepid: livepid.into(),
        group: String::new(),
    }
}

fn media_playlist(path: &str, first: i64, count: i64) -> String {
    let mut text = format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for sequence in first..first + count {
        text.push_str(&format!(
            "#EXTINF:2.000,\nhttps://cdn.test/{path}/seg_{sequence}.ts\n"
        ));
    }
    text
}

struct Fixture {
    pipeline: MediaPipeline,
    transport: Arc<ScriptedTransport>,
    ciphers: Arc<XorCipherFactory>,
}

fn fixture_with(config: PipelineConfig) -> Fixture {
    let transport = Arc::new(ScriptedTransport::new());
    transport
        .ok("v1/player/auth", AUTH_OK)
        .ok("web/open/token", TOKEN_OK);
    let flow = FlowOptions {
        concurrency: 1,
        min_interval: Duration::ZERO,
        jitter: Duration::ZERO,
        queue_timeout: Duration::from_secs(60),
        retries: 2,
        retry_delay: Duration::ZERO,
    };
    let live = LiveClient::new(transport.clone(), &shared_assets(), flow);
    let ciphers = Arc::new(XorCipherFactory::new());
    let pipeline = MediaPipeline::new(live, transport.clone(), ciphers.clone(), config);
    Fixture {
        pipeline,
        transport,
        ciphers,
    }
}

fn fixture() -> Fixture {
    fixture_with(PipelineConfig::default())
}

fn segment_url(segment: &SegmentRef) -> String {
    format!("/segment/x/{}.ts", segment.id)
}

fn ids(playlist: &str) -> Vec<String> {
    playlist
        .lines()
        .filter_map(|line| line.strip_prefix("/segment/x/"))
        .filter_map(|line| line.strip_suffix(".ts"))
        .map(str::to_string)
        .collect()
}

fn script_channel(fixture: &Fixture, path: &str, first: i64, count: i64) {
    fixture
        .transport
        .ok("get_live_info", live_ok(path))
        .ok(
            &format!("{path}/index.m3u8"),
            media_playlist(path, first, count),
        )
        .ok(
            &format!("{path}/seg_"),
            synthetic_segment(&StreamSpec::SMALL),
        );
}

#[tokio::test(start_paused = true)]
async fn playlist_lists_a_bounded_window_without_the_live_edge() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let text = fixture
        .pipeline
        .local_playlist(&channel("a", "pid-a"), segment_url)
        .await
        .unwrap();
    let listed = ids(&text);
    // 16 upstream segments, one held back, the newest 12 of the remaining 15.
    assert_eq!(listed.len(), 12);
    assert_eq!(listed[0], segment_id("pid-a", 103));
    assert_eq!(listed[11], segment_id("pid-a", 114));
    assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:103\n"));
    assert!(text.contains("#EXT-X-TARGETDURATION:2\n"));
}

#[tokio::test(start_paused = true)]
async fn segment_requests_validate_the_id_and_the_channel() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    let id = ids(&text).remove(0);
    assert!(matches!(
        fixture.pipeline.segment(&a, "deadbeef").await,
        Err(UpstreamError::UnknownSegment)
    ));
    let other = channel("b", "pid-b");
    assert!(matches!(
        fixture.pipeline.segment(&other, &id).await,
        Err(UpstreamError::SegmentNotInChannel)
    ));
}

#[tokio::test(start_paused = true)]
async fn first_segment_primes_predecessors_and_repeat_requests_are_cached() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    let newest = ids(&text).pop().unwrap();

    let bytes = fixture.pipeline.segment(&a, &newest).await.unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(bytes.len() % 188, 0);
    assert!(bytes.chunks(188).all(|packet| packet[0] == 0x47));
    // 11 predecessors plus the requested segment, all through one cipher.
    assert_eq!(fixture.transport.count("a/seg_"), 12);
    assert_eq!(fixture.ciphers.starts(), 1);

    let again = fixture.pipeline.segment(&a, &newest).await.unwrap();
    assert_eq!(again, bytes);
    assert_eq!(fixture.transport.count("a/seg_"), 12);

    // Predecessors were processed and cached, so they are served without fetching.
    let oldest = ids(&text).remove(0);
    fixture.pipeline.segment(&a, &oldest).await.unwrap();
    assert_eq!(fixture.transport.count("a/seg_"), 12);
}

#[tokio::test(start_paused = true)]
async fn continuous_playback_processes_each_new_segment_once() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    fixture
        .pipeline
        .segment(&a, &ids(&text).pop().unwrap())
        .await
        .unwrap();
    let after_first = fixture.transport.count("a/seg_");

    // The upstream playlist moves forward by one segment.
    fixture
        .transport
        .set_ok("a/index.m3u8", media_playlist("a", 101, 16));
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    let newest = ids(&text).pop().unwrap();
    assert_eq!(newest, segment_id("pid-a", 115));
    fixture.pipeline.segment(&a, &newest).await.unwrap();
    assert_eq!(fixture.transport.count("a/seg_"), after_first + 1);
    assert_eq!(fixture.ciphers.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_sequence_gap_resets_the_channel_runtime() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    fixture
        .pipeline
        .segment(&a, &ids(&text).pop().unwrap())
        .await
        .unwrap();
    assert_eq!(fixture.ciphers.starts(), 1);

    // The stream jumps far ahead: the processed state is useless now.
    fixture
        .transport
        .set_ok("a/index.m3u8", media_playlist("a", 400, 16));
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    let newest = ids(&text).pop().unwrap();
    fixture.pipeline.segment(&a, &newest).await.unwrap();
    assert_eq!(fixture.ciphers.starts(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_segment_older_than_the_runtime_and_not_cached_is_rejected() {
    let fixture = fixture();
    let mut worker = Worker {
        livepid: "pid-a".to_string(),
        runtime: Some(ChannelRuntime {
            cipher: Box::new(iptv_media::testkit::XorCipher::default()),
            video: VideoState::default(),
            mux: MuxState::default(),
            processed: BTreeMap::new(),
            last_processed: Some(200),
        }),
        reset_count: 0,
    };
    let error = worker
        .process(&fixture.pipeline.shared, &seg(100))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        UpstreamError::SegmentTooOld {
            sequence: 100,
            last: 200
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn forgotten_segment_ids_are_unknown() {
    let fixture = fixture_with(PipelineConfig {
        published_max: 4,
        ..PipelineConfig::default()
    });
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    // Only the newest four ids are remembered.
    let listed = ids(&text);
    assert!(matches!(
        fixture.pipeline.segment(&a, &listed[0]).await,
        Err(UpstreamError::UnknownSegment)
    ));
    assert!(fixture.pipeline.segment(&a, &listed[11]).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn malformed_segment_bytes_fail_without_breaking_the_channel() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    let newest = ids(&text).pop().unwrap();
    fixture.transport.set_ok("a/seg_", vec![1u8; 100]);
    let error = fixture.pipeline.segment(&a, &newest).await.unwrap_err();
    assert!(matches!(error, UpstreamError::Media { .. }), "{error}");

    // Good data arrives again; the same channel recovers.
    fixture
        .transport
        .set_ok("a/seg_", synthetic_segment(&StreamSpec::SMALL));
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    fixture
        .pipeline
        .segment(&a, &ids(&text).pop().unwrap())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn upstream_segment_errors_are_typed() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let text = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    fixture.transport.set_status("a/seg_", 403, "denied");
    let error = fixture
        .pipeline
        .segment(&a, &ids(&text).pop().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, UpstreamError::Status { status: 403, .. }));
}

#[tokio::test(start_paused = true)]
async fn playlist_failure_without_history_surfaces_the_error() {
    let fixture = fixture();
    fixture.transport.ok("get_live_info", live_ok("a")).status(
        "a/index.m3u8",
        403,
        "denied",
    );
    let error = fixture
        .pipeline
        .local_playlist(&channel("a", "pid-a"), segment_url)
        .await
        .unwrap_err();
    assert!(matches!(error, UpstreamError::Status { status: 403, .. }));
}

#[tokio::test(start_paused = true)]
async fn cached_history_keeps_playlists_alive_when_upstream_fails() {
    let fixture = fixture();
    script_channel(&fixture, "a", 100, 16);
    let a = channel("a", "pid-a");
    let first = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    fixture.transport.set_status("a/index.m3u8", 500, "oops");
    let second = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    assert_eq!(ids(&first), ids(&second));
}

#[tokio::test(start_paused = true)]
async fn master_playlists_are_followed_and_loops_are_cut() {
    let fixture = fixture();
    fixture
        .transport
        .ok("get_live_info", live_ok("a"))
        .ok(
            "a/index.m3u8",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nchild/index.m3u8\n",
        )
        .ok("a/child/index.m3u8", media_playlist("a", 7, 5))
        .ok("a/seg_", synthetic_segment(&StreamSpec::SMALL));
    let text = fixture
        .pipeline
        .local_playlist(&channel("a", "pid-a"), segment_url)
        .await
        .unwrap();
    assert_eq!(ids(&text).len(), 4);

    let fixture = fixture_with(PipelineConfig::default());
    fixture
        .transport
        .ok("get_live_info", live_ok("loop"))
        .ok("loop/index.m3u8", "#EXTM3U\nindex.m3u8\n");
    let error = fixture
        .pipeline
        .local_playlist(&channel("l", "pid-l"), segment_url)
        .await
        .unwrap_err();
    assert!(matches!(error, UpstreamError::PlaylistDepth(_)), "{error}");
    assert!(!error.to_string().contains('?'));
}

#[tokio::test(start_paused = true)]
async fn a_busy_channel_turns_requests_away_without_affecting_others() {
    let fixture = fixture_with(PipelineConfig {
        queue_capacity: 2,
        ..PipelineConfig::default()
    });
    script_channel(&fixture, "a", 100, 16);
    // Channel b is served by the same scripted live-info route after a.
    let a = channel("a", "pid-a");
    let text_a = fixture
        .pipeline
        .local_playlist(&a, segment_url)
        .await
        .unwrap();
    fixture
        .transport
        .set_ok("get_live_info", live_ok("b"))
        .ok("b/index.m3u8", media_playlist("b", 500, 16))
        .ok("b/seg_", synthetic_segment(&StreamSpec::SMALL));
    let b = channel("b", "pid-b");
    let text_b = fixture
        .pipeline
        .local_playlist(&b, segment_url)
        .await
        .unwrap();

    let gate = fixture.transport.gate("a/seg_");
    let listed = ids(&text_a);
    let mut blocked = Vec::new();
    // The first request is taken by the worker and parks at the gate.
    blocked.push(tokio::spawn({
        let (pipeline, a, id) = (fixture.pipeline.clone(), a.clone(), listed[11].clone());
        async move { pipeline.segment(&a, &id).await }
    }));
    tokio::time::sleep(Duration::from_millis(10)).await;
    // Two more fill the queue.
    for index in [10, 9] {
        blocked.push(tokio::spawn({
            let (pipeline, a, id) =
                (fixture.pipeline.clone(), a.clone(), listed[index].clone());
            async move { pipeline.segment(&a, &id).await }
        }));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    let overflow = fixture.pipeline.segment(&a, &listed[8]).await.unwrap_err();
    assert!(
        matches!(overflow, UpstreamError::Overloaded(ref ch) if ch == "a"),
        "{overflow}"
    );

    // Another channel is unaffected while `a` is parked.
    let other = fixture
        .pipeline
        .segment(&b, &ids(&text_b).pop().unwrap())
        .await
        .unwrap();
    assert!(!other.is_empty());

    gate.add_permits(100);
    for handle in blocked {
        handle.await.unwrap().unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn cipher_start_failure_is_reported_and_retried_on_the_next_request() {
    struct Flaky(std::sync::atomic::AtomicUsize);
    impl CipherFactory for Flaky {
        fn start(&self, _livepid: &str) -> Result<Box<dyn PayloadCipher + Send>> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err(UpstreamError::WorkerStopped);
            }
            Ok(Box::new(iptv_media::testkit::XorCipher::default()))
        }
    }
    let base = fixture();
    script_channel(&base, "a", 100, 16);
    let pipeline = MediaPipeline::new(
        LiveClient::new(
            base.transport.clone(),
            &shared_assets(),
            FlowOptions {
                concurrency: 1,
                min_interval: Duration::ZERO,
                jitter: Duration::ZERO,
                queue_timeout: Duration::from_secs(60),
                retries: 0,
                retry_delay: Duration::ZERO,
            },
        ),
        base.transport.clone(),
        Arc::new(Flaky(std::sync::atomic::AtomicUsize::new(0))),
        PipelineConfig::default(),
    );
    let a = channel("a", "pid-a");
    let text = pipeline.local_playlist(&a, segment_url).await.unwrap();
    let newest = ids(&text).pop().unwrap();
    assert!(pipeline.segment(&a, &newest).await.is_err());
    assert!(pipeline.segment(&a, &newest).await.is_ok());
}

fn seg(sequence: i64) -> SegmentRef {
    SegmentRef {
        id: segment_id("p", sequence),
        url: format!("https://u/{sequence}.ts"),
        duration: 2.0,
        sequence,
    }
}

#[test]
fn predecessors_are_contiguous_and_bounded() {
    let history: Vec<_> = (1..=20).map(seg).collect();
    let sequences =
        |list: Vec<SegmentRef>| list.iter().map(|s| s.sequence).collect::<Vec<_>>();
    assert_eq!(
        sequences(contiguous_predecessors(&history, 20, 12)),
        (9..=19).collect::<Vec<_>>()
    );
    assert_eq!(
        sequences(contiguous_predecessors(&history, 3, 12)),
        vec![1, 2]
    );
    assert!(contiguous_predecessors(&history, 1, 12).is_empty());

    let mut gapped: Vec<_> = (1..=5).map(seg).collect();
    gapped.extend((8..=10).map(seg));
    assert_eq!(
        sequences(contiguous_predecessors(&gapped, 10, 12)),
        vec![8, 9]
    );
    assert_eq!(sequences(missing_predecessors(&gapped, 4, 9)), vec![4, 5]);
    assert_eq!(sequences(missing_predecessors(&gapped, 8, 10)), vec![8, 9]);
    assert!(missing_predecessors(&gapped, 6, 8).is_empty());
}

#[test]
fn fifo_forgets_the_oldest_entries() {
    let mut fifo = Fifo::new(2);
    fifo.insert("a".into(), 1);
    fifo.insert("b".into(), 2);
    fifo.insert("a".into(), 10);
    assert_eq!(fifo.get("a"), Some(&10));
    fifo.insert("c".into(), 3);
    assert_eq!(fifo.get("a"), None);
    assert_eq!(fifo.get("b"), Some(&2));
    assert_eq!(fifo.get("c"), Some(&3));
}

#[test]
fn query_strings_are_stripped_from_error_urls() {
    assert_eq!(
        strip_query("https://h/p.m3u8?sig=secret"),
        "https://h/p.m3u8"
    );
}
