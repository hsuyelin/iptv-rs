use std::{
    path::Path,
    process::Command as StdCommand,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::*;

/// A transcoder that counts, delays and can be told to fail, without any ffmpeg.
#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    running: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
    fail_first: AtomicBool,
}

impl Fake {
    fn slow(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::default()
        }
    }
}

impl Transcoder for Fake {
    fn transcode(&self, input: Bytes) -> BoxFuture<'_, Result<Bytes, CompatError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            if self.fail_first.swap(false, Ordering::SeqCst) {
                return Err(CompatError::BadOutput);
            }
            Ok(Bytes::from([b"compat:".as_slice(), &input].concat()))
        })
    }
}

fn compat_of(fake: &Arc<Fake>, parallel: usize, capacity: usize) -> Compat {
    let transcoder: Arc<dyn Transcoder> = fake.clone();
    Compat::new(transcoder, parallel, capacity)
}

/// The value after `flag` in an argument list.
fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

#[test]
fn the_command_line_asks_for_what_old_devices_can_play() {
    let args = ffmpeg_args(&Settings::default());
    assert_eq!(after(&args, "-c:v"), Some("libx264"));
    assert_eq!(after(&args, "-profile:v"), Some("main"));
    assert_eq!(after(&args, "-level"), Some("3.1"));
    assert_eq!(after(&args, "-bf"), Some("0"), "no B-frames");
    assert_eq!(
        after(&args, "-g"),
        Some("50"),
        "a keyframe every 2 s at 25 fps"
    );
    assert_eq!(after(&args, "-keyint_min"), Some("50"));
    assert_eq!(
        after(&args, "-sc_threshold"),
        Some("0"),
        "no keyframes at scene cuts"
    );
    assert_eq!(after(&args, "-pix_fmt"), Some("yuv420p"));
    assert_eq!(after(&args, "-vf"), Some("scale=-2:'min(720,ih)'"));
    assert_eq!(after(&args, "-b:v"), Some("2500k"));
    assert_eq!(after(&args, "-maxrate"), Some("3000k"));
    assert_eq!(after(&args, "-bufsize"), Some("5000k"));
}

#[test]
fn audio_and_timestamps_are_left_alone() {
    let args = ffmpeg_args(&Settings::default());
    assert_eq!(after(&args, "-c:a"), Some("copy"));
    assert!(args.iter().any(|arg| arg == "-copyts"));
    assert_eq!(after(&args, "-muxdelay"), Some("0"));
    assert_eq!(after(&args, "-fps_mode"), Some("passthrough"));
    assert_eq!(after(&args, "-i"), Some("pipe:0"));
    assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
}

#[test]
fn the_level_follows_the_height() {
    for (height, level) in [(360, "3.0"), (576, "3.0"), (720, "3.1"), (1080, "4.0")] {
        let args = ffmpeg_args(&Settings {
            height,
            ..Settings::default()
        });
        assert_eq!(after(&args, "-level"), Some(level), "height {height}");
    }
}

#[test]
fn odd_settings_do_not_make_a_broken_command() {
    let args = ffmpeg_args(&Settings {
        video_kbps: 0,
        threads: 0,
        ..Settings::default()
    });
    assert_eq!(after(&args, "-b:v"), Some("100k"));
    assert_eq!(after(&args, "-threads"), Some("1"));
}

#[tokio::test(start_paused = true)]
async fn a_segment_is_encoded_once_and_fetched_once() {
    let fake = Arc::new(Fake::default());
    let compat = compat_of(&fake, 2, 8);
    let fetched = AtomicUsize::new(0);
    for _ in 0..3 {
        let out = compat
            .segment("cctv1", "a", || async {
                fetched.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(Bytes::from_static(b"TS"))
            })
            .await
            .unwrap();
        assert_eq!(&out[..], b"compat:TS");
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fetched.load(Ordering::SeqCst),
        1,
        "a cached segment costs nothing upstream"
    );
}

#[tokio::test(start_paused = true)]
async fn viewers_asking_together_share_one_encode() {
    let fake = Arc::new(Fake::slow(Duration::from_millis(500)));
    let compat = compat_of(&fake, 4, 8);
    let ask = || async {
        compat
            .segment("cctv1", "a", || async {
                Ok::<_, ()>(Bytes::from_static(b"TS"))
            })
            .await
            .unwrap()
    };
    let (a, b, c, d) = tokio::join!(ask(), ask(), ask(), ask());
    assert!([a, b, c, d].iter().all(|out| &out[..] == b"compat:TS"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_encode_is_not_remembered() {
    let fake = Arc::new(Fake::default());
    fake.fail_first.store(true, Ordering::SeqCst);
    let compat = compat_of(&fake, 2, 8);
    let fetch = || async { Ok::<_, ()>(Bytes::from_static(b"TS")) };
    assert!(matches!(
        compat.segment("c", "k", fetch).await,
        Err(CompatFailure::Transcode(CompatError::BadOutput))
    ));
    let retry = compat.segment("c", "k", fetch).await.unwrap();
    assert_eq!(&retry[..], b"compat:TS");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn an_upstream_failure_passes_through_and_nothing_is_encoded() {
    let fake = Arc::new(Fake::default());
    let compat = compat_of(&fake, 2, 8);
    let result = compat
        .segment("c", "k", || async { Err::<Bytes, _>("upstream is down") })
        .await;
    assert!(matches!(
        result,
        Err(CompatFailure::Original("upstream is down"))
    ));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn the_oldest_segment_is_forgotten_first() {
    let fake = Arc::new(Fake::default());
    let compat = compat_of(&fake, 2, 2);
    let fetch = || async { Ok::<_, ()>(Bytes::from_static(b"TS")) };
    for key in ["a", "b", "c"] {
        compat.segment("c", key, fetch).await.unwrap();
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
    compat.segment("c", "c", fetch).await.unwrap();
    compat.segment("c", "b", fetch).await.unwrap();
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        3,
        "b and c are still remembered"
    );
    compat.segment("c", "a", fetch).await.unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 4, "a was forgotten");
}

#[tokio::test(start_paused = true)]
async fn no_more_encodes_run_at_once_than_allowed() {
    let fake = Arc::new(Fake::slow(Duration::from_millis(200)));
    let compat = compat_of(&fake, 2, 16);
    let fetch = || async { Ok::<_, ()>(Bytes::from_static(b"TS")) };
    let _ = tokio::join!(
        compat.segment("c", "1", fetch),
        compat.segment("c", "2", fetch),
        compat.segment("c", "3", fetch),
        compat.segment("c", "4", fetch),
        compat.segment("c", "5", fetch),
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 5);
    assert_eq!(fake.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_missing_program_is_reported_by_name() {
    let transcoder = FfmpegTranscoder::new("/no/such/ffmpeg".into(), Settings::default());
    let error = transcoder.check().await.unwrap_err();
    assert!(matches!(error, CompatError::Spawn { .. }));
    assert!(error.to_string().contains("/no/such/ffmpeg"));
    assert!(matches!(
        transcoder.transcode(Bytes::from_static(b"x")).await,
        Err(CompatError::Spawn { .. })
    ));
}

#[test]
fn only_the_end_of_a_long_error_is_kept() {
    assert_eq!(tail(b"  short  "), "short");
    let long = vec![b'x'; STDERR_KEEP * 3];
    assert_eq!(tail(&long).len(), STDERR_KEEP);
}

// ---- Continuity counters ---------------------------------------------------------------

const PAYLOAD: u8 = 0x10;
const ADAPTATION_ONLY: u8 = 0x20;

fn packet(pid: u16, flags: u8) -> Vec<u8> {
    let mut bytes = vec![0xffu8; 188];
    bytes[0] = 0x47;
    bytes[1] = ((pid >> 8) as u8) & 0x1f;
    bytes[2] = pid as u8;
    bytes[3] = flags;
    bytes
}

/// The continuity counters of `pid`, in order.
fn counters(ts: &[u8], pid: u16) -> Vec<u8> {
    ts.as_chunks::<188>()
        .0
        .iter()
        .filter(|p| (u16::from(p[1] & 0x1f) << 8 | u16::from(p[2])) == pid)
        .map(|p| p[3] & 0x0f)
        .collect()
}

fn stream(pid: u16, first: u8, count: u8) -> Vec<u8> {
    (0..count)
        .flat_map(|n| packet(pid, PAYLOAD | ((first + n) & 0x0f)))
        .collect()
}

#[test]
fn the_second_segment_carries_on_from_the_first() {
    let mut next = HashMap::new();
    let mut first = stream(0x100, 0, 3);
    let mut second = stream(0x100, 0, 2);
    carry_continuity(&mut first, &mut next);
    carry_continuity(&mut second, &mut next);
    assert_eq!(
        counters(&first, 0x100),
        [0, 1, 2],
        "the first segment is left as it came"
    );
    assert_eq!(counters(&second, 0x100), [3, 4]);
}

#[test]
fn a_pid_seen_for_the_first_time_keeps_its_number() {
    let mut next = HashMap::new();
    let mut ts = stream(0x101, 7, 3);
    carry_continuity(&mut ts, &mut next);
    assert_eq!(counters(&ts, 0x101), [7, 8, 9]);
}

#[test]
fn the_counter_wraps_after_fifteen() {
    let mut next = HashMap::new();
    let mut first = stream(0x100, 0, 14);
    let mut second = stream(0x100, 0, 4);
    carry_continuity(&mut first, &mut next);
    carry_continuity(&mut second, &mut next);
    assert_eq!(counters(&second, 0x100), [14, 15, 0, 1]);
}

#[test]
fn each_pid_counts_on_its_own() {
    let mut next = HashMap::new();
    let mut ts = [
        stream(0x100, 0, 2),
        stream(0x101, 0, 1),
        stream(0x100, 5, 1),
    ]
    .concat();
    carry_continuity(&mut ts, &mut next);
    assert_eq!(counters(&ts, 0x100), [0, 1, 2]);
    assert_eq!(counters(&ts, 0x101), [0]);
}

#[test]
fn a_packet_without_a_payload_repeats_the_number_before_it() {
    let mut next = HashMap::new();
    let mut ts = [
        packet(0x100, PAYLOAD),
        packet(0x100, ADAPTATION_ONLY | 0x0c),
        packet(0x100, PAYLOAD | 0x0d),
    ]
    .concat();
    carry_continuity(&mut ts, &mut next);
    assert_eq!(counters(&ts, 0x100), [0, 0, 1]);
}

#[test]
fn bytes_that_are_not_packets_are_left_alone() {
    let mut next = HashMap::new();
    let mut junk = b"not a transport stream, and not a multiple of 188 bytes".to_vec();
    let before = junk.clone();
    carry_continuity(&mut junk, &mut next);
    assert_eq!(junk, before);
    let mut lost_sync = packet(0x100, PAYLOAD | 5);
    lost_sync[0] = 0x00;
    let before = lost_sync.clone();
    carry_continuity(&mut lost_sync, &mut next);
    assert_eq!(lost_sync, before);
    assert!(next.is_empty());
    let mut partial = [stream(0x100, 0, 1), vec![0x47, 0x01]].concat();
    carry_continuity(&mut partial, &mut next);
    assert_eq!(&partial[188..], &[0x47, 0x01]);
}

/// Encodes into three packets that, like a fresh ffmpeg, count from zero.
struct FromZero;

impl Transcoder for FromZero {
    fn transcode(&self, _input: Bytes) -> BoxFuture<'_, Result<Bytes, CompatError>> {
        Box::pin(async { Ok(Bytes::from(stream(0x100, 0, 3))) })
    }
}

fn from_zero() -> Compat {
    Compat::new(Arc::new(FromZero), 2, 8)
}

async fn encoded(compat: &Compat, channel: &str, id: &str) -> Vec<u8> {
    let out = compat
        .segment(channel, id, || async {
            Ok::<_, ()>(Bytes::from_static(b"TS"))
        })
        .await
        .unwrap();
    out.to_vec()
}

#[tokio::test(start_paused = true)]
async fn consecutive_segments_of_a_channel_join_without_a_jump() {
    let compat = from_zero();
    let one = encoded(&compat, "cctv1", "a").await;
    let two = encoded(&compat, "cctv1", "b").await;
    let three = encoded(&compat, "cctv1", "c").await;
    assert_eq!(counters(&one, 0x100), [0, 1, 2]);
    assert_eq!(counters(&two, 0x100), [3, 4, 5]);
    assert_eq!(counters(&three, 0x100), [6, 7, 8]);
}

#[tokio::test(start_paused = true)]
async fn channels_do_not_share_counters() {
    let compat = from_zero();
    encoded(&compat, "cctv1", "a").await;
    let other = encoded(&compat, "cctv2", "a").await;
    assert_eq!(counters(&other, 0x100), [0, 1, 2]);
}

#[tokio::test(start_paused = true)]
async fn a_remembered_segment_is_not_renumbered_again() {
    let compat = from_zero();
    let first = encoded(&compat, "cctv1", "a").await;
    assert_eq!(encoded(&compat, "cctv1", "a").await, first);
    let next = encoded(&compat, "cctv1", "b").await;
    assert_eq!(
        counters(&next, 0x100),
        [3, 4, 5],
        "asking again did not use up numbers"
    );
}

// ---- With a real ffmpeg: the output is checked with ffprobe, frame by frame. -------------

fn has(program: &str) -> bool {
    StdCommand::new(program)
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Five seconds of 1080p25 High profile with B-frames and one keyframe, like the real
/// stream, at large timestamps like the real stream.
fn make_source(path: &Path) {
    let status = StdCommand::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=1920x1080:rate=25")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100"])
        .args(["-t", "5", "-c:v", "libx264", "-preset", "ultrafast"])
        .args([
            "-profile:v",
            "high",
            "-bf",
            "3",
            "-g",
            "125",
            "-pix_fmt",
            "yuv420p",
        ])
        .args(["-c:a", "aac", "-ac", "2", "-output_ts_offset", "91039.2"])
        .args(["-f", "mpegts"])
        .arg(path)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success());
}

fn probe(path: &Path, entries: &str, select: &str) -> serde_json::Value {
    let output = StdCommand::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            select,
            "-show_entries",
            entries,
        ])
        .args(["-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    serde_json::from_slice(&output.stdout).expect("ffprobe prints JSON")
}

fn times(value: &serde_json::Value, key: &str, field: &str) -> Vec<f64> {
    value[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item[field].as_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn ffmpeg_makes_the_stream_the_proposal_asks_for() {
    if !(has("ffmpeg") && has("ffprobe")) {
        eprintln!("skipped: ffmpeg and ffprobe are not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("in.ts");
    let output = dir.path().join("out.ts");
    make_source(&source);

    let transcoder = FfmpegTranscoder::new("ffmpeg".into(), Settings::default());
    transcoder.check().await.unwrap();
    let encoded = transcoder
        .transcode(Bytes::from(std::fs::read(&source).unwrap()))
        .await
        .unwrap();
    std::fs::write(&output, &encoded).unwrap();
    assert_eq!(encoded.len() % 188, 0, "whole transport stream packets");

    // The picture: 720p, Main profile, level 3.1, no B-frames.
    let stream = probe(&output, "stream=profile,level,height,width", "v");
    let video = &stream["streams"][0];
    assert_eq!(video["profile"], "Main");
    assert_eq!(video["level"], 31);
    assert_eq!(video["height"], 720);
    assert_eq!(video["width"], 1280);

    // Every frame is there, none is a B-frame, and a keyframe comes at least every 2 s.
    let frames = probe(&output, "frame=pict_type,key_frame,pts_time", "v");
    let all = frames["frames"].as_array().unwrap();
    assert_eq!(all.len(), 125, "no frame lost or repeated");
    assert!(all.iter().all(|frame| frame["pict_type"] != "B"));
    let keys: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame["key_frame"] == 1)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(keys.first(), Some(&0), "the segment starts on a keyframe");
    assert!(
        keys.windows(2).all(|pair| pair[1] - pair[0] <= 50),
        "{keys:?}"
    );
    assert!(125 - keys.last().copied().unwrap_or(0) <= 50);

    // Timestamps: even 40 ms steps, and close to where the source had them.
    let before = times(
        &probe(&source, "packet=pts_time", "v"),
        "packets",
        "pts_time",
    );
    let after_pts = times(
        &probe(&output, "packet=pts_time", "v"),
        "packets",
        "pts_time",
    );
    let first_before = before.iter().copied().fold(f64::MAX, f64::min);
    let first_after = after_pts.iter().copied().fold(f64::MAX, f64::min);
    assert!(
        (first_after - first_before).abs() < 0.1,
        "{first_before} vs {first_after}"
    );
    let mut sorted = after_pts.clone();
    sorted.sort_by(f64::total_cmp);
    assert!(sorted
        .windows(2)
        .all(|pair| ((pair[1] - pair[0]) - 0.04).abs() < 0.001));

    // Audio is the very same stream: same codec, same first timestamp, same count.
    let audio_before = probe(&source, "packet=pts_time:stream=codec_name", "a");
    let audio_after = probe(&output, "packet=pts_time:stream=codec_name", "a");
    assert_eq!(audio_after["streams"][0]["codec_name"], "aac");
    let a = times(&audio_before, "packets", "pts_time");
    let b = times(&audio_after, "packets", "pts_time");
    assert_eq!(a.len(), b.len());
    assert!((a[0] - b[0]).abs() < 0.001);
}

#[tokio::test]
async fn a_segment_ffmpeg_cannot_read_fails_cleanly() {
    if !has("ffmpeg") {
        eprintln!("skipped: ffmpeg is not installed");
        return;
    }
    let transcoder = FfmpegTranscoder::new("ffmpeg".into(), Settings::default());
    let error = transcoder
        .transcode(Bytes::from_static(b"this is not a transport stream"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, CompatError::Failed { .. } | CompatError::BadOutput),
        "{error}"
    );
}
