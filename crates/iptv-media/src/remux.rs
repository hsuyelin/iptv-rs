use serde::Serialize;

use crate::{
    cipher::PayloadCipher,
    error::MediaError,
    ts::{
        at,
        mux::{mux_events_to_ts, EventKind, MediaEvent, MuxState},
        nal::{count_byte_diff, find_nals, nal_type},
        packet::{parse_packets, Packet},
        pes::{collect_pes, Pes},
        psi::{parse_pat, parse_pmt, StreamInfo, STREAM_AAC, STREAM_H264},
    },
};

const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000,
    11_025, 8_000, 7_350,
];

/// Counters describing one remuxed segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RemuxStats {
    /// Input video PID.
    pub input_video_pid: u16,
    /// Input audio PID.
    pub input_audio_pid: u16,
    /// Number of video PES units.
    pub video_pes_count: usize,
    /// Number of audio PES units.
    pub audio_pes_count: usize,
    /// Video access units written.
    pub video_sample_count: usize,
    /// Audio frames written.
    pub audio_sample_count: usize,
    /// NAL units seen.
    pub nal_count: usize,
    /// NAL units passed to the cipher for decoding.
    pub decoded_nals: usize,
    /// NAL units whose bytes changed.
    pub changed_nals: usize,
    /// Total changed bytes.
    pub changed_bytes: usize,
    /// NAL units that became shorter.
    pub shorter_nals: usize,
    /// SPS units used for their cipher side effect.
    pub sps_side_effects: usize,
    /// Bytes in the output stream.
    pub output_bytes: usize,
}

/// Video parameter state carried across segments of one channel.
#[derive(Debug, Default, Clone)]
pub struct VideoState {
    live_sps_enabled: bool,
    last_sps: Option<Vec<u8>>,
    last_pps: Option<Vec<u8>>,
}

/// Decrypts the video of a TS segment and remuxes it into a fresh TS.
///
/// # Errors
/// Returns [`MediaError`] when the input is not a well-formed H.264/AAC transport
/// stream or when `cipher` fails.
pub fn decrypt_and_remux<C: PayloadCipher>(
    cipher: &mut C,
    video_state: &mut VideoState,
    mux_state: &mut MuxState,
    input: &[u8],
) -> Result<(Vec<u8>, RemuxStats), MediaError> {
    let packets = parse_packets(input)?;
    let pmt_pid = find_pmt_pid(input, &packets)?;
    let streams = find_streams(input, &packets, pmt_pid)?;
    let video_pid = stream_pid(&streams, STREAM_H264).ok_or(MediaError::NoVideoStream)?;
    let audio_pid = stream_pid(&streams, STREAM_AAC).ok_or(MediaError::NoAudioStream)?;

    let video_pes = collect_pes(input, &packets, video_pid);
    let audio_pes = collect_pes(input, &packets, audio_pid);
    let mut stats = RemuxStats {
        input_video_pid: video_pid,
        input_audio_pid: audio_pid,
        video_pes_count: video_pes.len(),
        audio_pes_count: audio_pes.len(),
        ..RemuxStats::default()
    };

    let mut events = Vec::with_capacity(video_pes.len() + audio_pes.len() * 2);
    let mut arena = Vec::with_capacity(input.len());
    let mut body = Vec::new();
    for (index, pes) in video_pes.iter().enumerate() {
        let mut sink = VideoSink {
            arena: &mut arena,
            body: &mut body,
            stats: &mut stats,
        };
        if let Some(event) =
            decrypt_video_pes(cipher, video_state, pes, index, &mut sink)?
        {
            events.push(event);
        }
    }
    for pes in &audio_pes {
        append_audio_events(pes, &mut events, &mut arena, &mut stats);
    }
    if !events.iter().any(|event| event.kind == EventKind::Video) {
        return Err(MediaError::NoVideoSamples);
    }
    if !events.iter().any(|event| event.kind == EventKind::Audio) {
        return Err(MediaError::NoAudioSamples);
    }
    let output = mux_events_to_ts(&mut events, &arena, mux_state);
    stats.output_bytes = output.len();
    Ok((output, stats))
}

fn stream_pid(streams: &[StreamInfo], stream_type: u8) -> Option<u16> {
    streams
        .iter()
        .find(|stream| stream.stream_type == stream_type)
        .map(|stream| stream.pid)
}

fn find_pmt_pid(input: &[u8], packets: &[Packet]) -> Result<u16, MediaError> {
    packets
        .iter()
        .filter(|packet| packet.pid == 0 && packet.pusi && packet.has_payload())
        .find_map(|packet| parse_pat(packet.payload(input)))
        .ok_or(MediaError::NoPmt)
}

fn find_streams(
    input: &[u8],
    packets: &[Packet],
    pmt_pid: u16,
) -> Result<Vec<StreamInfo>, MediaError> {
    packets
        .iter()
        .filter(|packet| packet.pid == pmt_pid && packet.pusi && packet.has_payload())
        .map(|packet| parse_pmt(packet.payload(input)))
        .find(|streams| !streams.is_empty())
        .ok_or(MediaError::NoStreams)
}

struct VideoSink<'a> {
    arena: &'a mut Vec<u8>,
    body: &'a mut Vec<u8>,
    stats: &'a mut RemuxStats,
}

fn push_annex_b(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

fn cipher_error(
    stage: &'static str,
    index: usize,
) -> impl FnOnce(crate::cipher::CipherError) -> MediaError {
    move |source| MediaError::Cipher {
        stage,
        index,
        source,
    }
}

fn decrypt_video_pes<C: PayloadCipher>(
    cipher: &mut C,
    state: &mut VideoState,
    pes: &Pes,
    index: usize,
    sink: &mut VideoSink<'_>,
) -> Result<Option<MediaEvent>, MediaError> {
    let Some(pts) = pes.pts else {
        return Ok(None);
    };
    let dts = pes.dts.unwrap_or(pts);
    let payload = pes.payload();
    let nals = find_nals(payload, 0);
    if nals.is_empty() {
        return Ok(None);
    }

    sink.body.clear();
    let mut keyframe = false;
    for range in nals {
        let Some(data) = payload.get(range.start..range.end) else {
            continue;
        };
        sink.stats.nal_count += 1;
        cipher.tick().map_err(cipher_error("tick", index))?;
        let kind = nal_type(data);
        match kind {
            1 | 5 => {
                if state.live_sps_enabled {
                    sink.stats.decoded_nals += 1;
                    let decoded =
                        cipher.decode(data).map_err(cipher_error("decode", index))?;
                    let diff = count_byte_diff(data, &decoded);
                    if diff > 0 {
                        sink.stats.changed_nals += 1;
                        sink.stats.changed_bytes += diff;
                    }
                    if decoded.len() < data.len() {
                        sink.stats.shorter_nals += 1;
                    }
                    if decoded.len() > data.len() {
                        return Err(MediaError::CipherGrew {
                            before: data.len(),
                            after: decoded.len(),
                            nal_type: kind,
                        });
                    }
                    push_annex_b(sink.body, &decoded);
                } else {
                    push_annex_b(sink.body, data);
                }
                if kind == 5 {
                    keyframe = true;
                }
            }
            7 => {
                let mut sps = data.to_vec();
                if sps.len() > 2 {
                    if !state.live_sps_enabled {
                        let marker = at(&sps, 2) & 0x03;
                        state.live_sps_enabled = marker == 1 || marker == 2;
                    }
                    cipher
                        .decode(data)
                        .map_err(cipher_error("decode-sps", index))?;
                    if let Some(slot) = sps.get_mut(2) {
                        *slot = 0;
                    }
                    sink.stats.sps_side_effects += 1;
                }
                push_annex_b(sink.body, &sps);
                state.last_sps = Some(sps);
            }
            8 => {
                push_annex_b(sink.body, data);
                state.last_pps = Some(data.to_vec());
            }
            _ => push_annex_b(sink.body, data),
        }
    }

    let start = sink.arena.len();
    sink.arena.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xf0]);
    if keyframe {
        if let Some(sps) = &state.last_sps {
            push_annex_b(sink.arena, sps);
        }
        if let Some(pps) = &state.last_pps {
            push_annex_b(sink.arena, pps);
        }
    }
    sink.arena.extend_from_slice(sink.body);
    sink.stats.video_sample_count += 1;
    Ok(Some(MediaEvent {
        kind: EventKind::Video,
        dts90: dts,
        pts90: pts,
        start,
        len: sink.arena.len() - start,
        keyframe,
    }))
}

fn append_audio_events(
    pes: &Pes,
    events: &mut Vec<MediaEvent>,
    arena: &mut Vec<u8>,
    stats: &mut RemuxStats,
) {
    let Some(base_pts) = pes.pts else {
        return;
    };
    let payload = pes.payload();
    let mut offset = 0usize;
    let mut index = 0i64;
    while offset + 7 <= payload.len() {
        if at(payload, offset) != 0xff || (at(payload, offset + 1) & 0xf0) != 0xf0 {
            offset += 1;
            continue;
        }
        let rate_index = usize::from((at(payload, offset + 2) >> 2) & 0x0f);
        let sample_rate = AAC_SAMPLE_RATES.get(rate_index).copied().unwrap_or(44_100);
        let frame_length = (usize::from(at(payload, offset + 3) & 0x03) << 11)
            | (usize::from(at(payload, offset + 4)) << 3)
            | (usize::from(at(payload, offset + 5)) >> 5);
        if frame_length < 7 || offset + frame_length > payload.len() {
            break;
        }
        let Some(frame) = payload.get(offset..offset + frame_length) else {
            break;
        };
        let delta = (index * 1024 * 90_000) / i64::from(sample_rate);
        let start = arena.len();
        arena.extend_from_slice(frame);
        events.push(MediaEvent {
            kind: EventKind::Audio,
            dts90: base_pts + delta,
            pts90: base_pts + delta,
            start,
            len: frame_length,
            keyframe: false,
        });
        stats.audio_sample_count += 1;
        index += 1;
        offset += frame_length;
    }
}
