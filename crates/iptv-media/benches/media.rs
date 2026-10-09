#![allow(missing_docs)]
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use iptv_media::{
    build_channel_list, decrypt_and_remux, parse_media_playlist, playable_window,
    render_local_playlist, segment_id,
    testkit::{synthetic_segment, PassThroughCipher, StreamSpec, XorCipher},
    ChannelListStyle, MuxState, PlaylistEntry, SegmentRef, VideoState, WindowPolicy,
};

fn ts_benches(c: &mut Criterion) {
    let input = synthetic_segment(&StreamSpec::TWO_MIB);
    let mut group = c.benchmark_group("ts");
    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("remux_pass_through_2mib", |b| {
        b.iter(|| {
            let mut cipher = PassThroughCipher::default();
            decrypt_and_remux(
                &mut cipher,
                &mut VideoState::default(),
                &mut MuxState::default(),
                black_box(&input),
            )
        });
    });
    group.bench_function("remux_xor_2mib", |b| {
        b.iter(|| {
            let mut cipher = XorCipher::default();
            decrypt_and_remux(
                &mut cipher,
                &mut VideoState::default(),
                &mut MuxState::default(),
                black_box(&input),
            )
        });
    });
    group.finish();
}

fn playlist_benches(c: &mut Criterion) {
    let mut text = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1000\n");
    for i in 0..60 {
        text.push_str(&format!(
            "#EXTINF:2.000,\nseg_{i}.ts?token=abcdef0123456789\n"
        ));
    }
    c.bench_function("hls/parse_upstream_playlist", |b| {
        b.iter(|| {
            parse_media_playlist(
                black_box(&text),
                "https://cdn.example.com/live/index.m3u8",
            )
        });
    });

    let history: Vec<SegmentRef> = (0..36)
        .map(|i| SegmentRef {
            id: segment_id("600001859", 1000 + i),
            url: format!("https://cdn.example.com/seg_{i}.ts"),
            duration: 2.0,
            sequence: 1000 + i,
        })
        .collect();
    let policy = WindowPolicy {
        window: 12,
        holdback: 1,
    };
    c.bench_function("hls/window_and_render", |b| {
        b.iter(|| {
            let window = playable_window(black_box(&history), policy);
            render_local_playlist(window, |s| {
                format!("http://relay:8787/segment/cctv1/{}.ts", s.id)
            })
        });
    });

    let entries: Vec<PlaylistEntry> = (0..200)
        .map(|i| PlaylistEntry {
            slug: format!("channel{i}"),
            name: format!("Channel {i} 综合"),
            logo: format!("https://cdn.example.com/logo/{i}.png"),
            group: "央视".to_string(),
        })
        .collect();
    let style = ChannelListStyle {
        epg_url: "https://epg.example.com/t.xml",
        notice_name: "Notice",
        notice_logo: "https://cdn.example.com/n.jpg",
        notice_url: "https://cdn.example.com/n.m3u8",
    };
    c.bench_function("playlist/m3u_build_200", |b| {
        b.iter(|| {
            build_channel_list(black_box(&entries), &style, |slug| {
                format!("http://relay:8787/live/{slug}.m3u8")
            })
        });
    });
}

criterion_group!(benches, ts_benches, playlist_benches);
criterion_main!(benches);
