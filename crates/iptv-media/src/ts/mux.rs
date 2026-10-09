use crate::ts::{
    packet::{PACKET_LEN, SYNC_BYTE},
    pes::encode_timestamp,
};

pub(crate) const OUT_VIDEO_PID: u16 = 0x0100;
pub(crate) const OUT_AUDIO_PID: u16 = 0x0101;
pub(crate) const OUT_PMT_PID: u16 = 0x1000;

const VIDEO_STREAM_ID: u8 = 0xe0;
const AUDIO_STREAM_ID: u8 = 0xc0;

/// Continuity counters carried across segments of one channel.
#[derive(Debug, Default, Clone)]
pub struct MuxState {
    continuity: Vec<(u16, u8)>,
}

impl MuxState {
    /// Returns the counter value the next packet on `pid` will carry.
    pub fn next_counter(&self, pid: u16) -> u8 {
        self.continuity
            .iter()
            .find(|(candidate, _)| *candidate == pid)
            .map_or(0, |(_, value)| *value & 0x0f)
    }

    fn take_counter(&mut self, pid: u16) -> u8 {
        if let Some((_, value)) = self.continuity.iter_mut().find(|(p, _)| *p == pid) {
            let current = *value & 0x0f;
            *value = (current + 1) & 0x0f;
            return current;
        }
        self.continuity.push((pid, 1));
        0
    }
}

/// Kind of elementary stream an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    Video,
    Audio,
}

/// One access unit or audio frame, stored in an arena.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MediaEvent {
    pub kind: EventKind,
    pub dts90: i64,
    pub pts90: i64,
    pub start: usize,
    pub len: usize,
    pub keyframe: bool,
}

/// A 188-byte packet under construction. Writes outside the packet are ignored.
struct PacketBuf([u8; PACKET_LEN]);

impl PacketBuf {
    fn new() -> Self {
        Self([0xff; PACKET_LEN])
    }

    fn put(&mut self, at: usize, value: u8) {
        if let Some(slot) = self.0.get_mut(at) {
            *slot = value;
        }
    }

    fn put_slice(&mut self, at: usize, bytes: &[u8]) {
        if let Some(target) = self.0.get_mut(at..at + bytes.len()) {
            target.copy_from_slice(bytes);
        }
    }

    fn header(&mut self, pid: u16, first: bool, afc: u8, counter: u8) {
        self.put(0, SYNC_BYTE);
        self.put(
            1,
            if first { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1f),
        );
        self.put(2, pid as u8);
        self.put(3, (afc << 4) | counter);
    }
}

/// Muxes `events` into a fresh transport stream with a PAT and PMT up front.
pub(crate) fn mux_events_to_ts(
    events: &mut Vec<MediaEvent>,
    arena: &[u8],
    state: &mut MuxState,
) -> Vec<u8> {
    let estimate =
        arena.len() / 184 * PACKET_LEN + events.len() * PACKET_LEN + 4 * PACKET_LEN;
    let mut out = Vec::with_capacity(estimate);
    let mut scratch = Vec::new();
    write_psi(&mut out, 0x0000, &pat_section(), state);
    write_psi(&mut out, OUT_PMT_PID, &pmt_section(), state);

    if let Some(index) = events
        .iter()
        .position(|event| event.kind == EventKind::Video)
    {
        let first_video = events.remove(index);
        write_event(&mut out, &mut scratch, arena, &first_video, state);
    }
    events.sort_by(|a, b| {
        a.dts90.cmp(&b.dts90).then_with(|| match (a.kind, b.kind) {
            (EventKind::Video, EventKind::Audio) => std::cmp::Ordering::Less,
            (EventKind::Audio, EventKind::Video) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        })
    });
    for event in events.iter() {
        write_event(&mut out, &mut scratch, arena, event, state);
    }
    out
}

fn write_event(
    out: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    arena: &[u8],
    event: &MediaEvent,
    state: &mut MuxState,
) {
    let payload = arena
        .get(event.start..event.start + event.len)
        .unwrap_or(&[]);
    match event.kind {
        EventKind::Video => write_pes(
            out,
            scratch,
            PesTarget {
                pid: OUT_VIDEO_PID,
                stream_id: VIDEO_STREAM_ID,
            },
            payload,
            (event.pts90, event.dts90),
            Some((event.dts90, event.keyframe)),
            state,
        ),
        EventKind::Audio => write_pes(
            out,
            scratch,
            PesTarget {
                pid: OUT_AUDIO_PID,
                stream_id: AUDIO_STREAM_ID,
            },
            payload,
            (event.pts90, event.dts90),
            None,
            state,
        ),
    }
}

#[derive(Clone, Copy)]
struct PesTarget {
    pid: u16,
    stream_id: u8,
}

fn write_pes(
    out: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    target: PesTarget,
    payload: &[u8],
    (pts, dts): (i64, i64),
    pcr: Option<(i64, bool)>,
    state: &mut MuxState,
) {
    make_pes_into(scratch, target.stream_id, payload, pts, dts);
    let pes: &[u8] = scratch;
    let mut offset = 0usize;
    let mut first = true;
    while offset < pes.len() {
        let remaining = pes.len() - offset;
        let with_pcr = first && pcr.is_some();
        let mut payload_capacity = 184usize;
        let mut afc = 1u8;
        let mut adaptation_length = 0usize;
        let mut adaptation_flags = 0u8;
        if with_pcr {
            afc = 3;
            let random_access = pcr.is_some_and(|(_, random)| random);
            adaptation_flags = 0x10 | if random_access { 0x40 } else { 0 };
            payload_capacity = 176;
            adaptation_length = if remaining < payload_capacity {
                7 + (payload_capacity - remaining)
            } else {
                7
            };
        } else if remaining < 184 {
            afc = 3;
            adaptation_length = 183 - remaining;
            payload_capacity = remaining;
        }
        let payload_len = remaining.min(payload_capacity);
        let mut packet = PacketBuf::new();
        packet.header(target.pid, first, afc, state.take_counter(target.pid));
        let mut cursor = 4usize;
        if afc == 3 {
            packet.put(cursor, adaptation_length as u8);
            cursor += 1;
            if adaptation_length > 0 {
                packet.put(cursor, adaptation_flags);
                cursor += 1;
                if let (true, Some((pcr_value, _))) = (with_pcr, pcr) {
                    packet.put_slice(cursor, &pcr_bytes(pcr_value));
                }
                // Stuffing bytes are already 0xff from `PacketBuf::new`.
                cursor = PACKET_LEN - payload_len;
            }
        }
        if let Some(chunk) = pes.get(offset..offset + payload_len) {
            packet.put_slice(cursor, chunk);
        }
        out.extend_from_slice(&packet.0);
        offset += payload_len;
        first = false;
    }
}

fn make_pes_into(out: &mut Vec<u8>, stream_id: u8, payload: &[u8], pts: i64, dts: i64) {
    out.clear();
    let use_dts = pts != dts;
    let timestamp_len = if use_dts { 10 } else { 5 };
    out.reserve(9 + timestamp_len + payload.len());
    out.extend_from_slice(&[
        0,
        0,
        1,
        stream_id,
        0,
        0,
        0x80,
        if use_dts { 0xc0 } else { 0x80 },
        timestamp_len as u8,
    ]);
    if stream_id != VIDEO_STREAM_ID {
        let length =
            (payload.len() + 3 + timestamp_len).min(usize::from(u16::MAX)) as u16;
        let [hi, lo] = length.to_be_bytes();
        if let Some(slot) = out.get_mut(4) {
            *slot = hi;
        }
        if let Some(slot) = out.get_mut(5) {
            *slot = lo;
        }
    }
    if use_dts {
        out.extend_from_slice(&encode_timestamp(0x03, pts));
        out.extend_from_slice(&encode_timestamp(0x01, dts));
    } else {
        out.extend_from_slice(&encode_timestamp(0x02, pts));
    }
    out.extend_from_slice(payload);
}

fn write_psi(out: &mut Vec<u8>, pid: u16, section: &[u8], state: &mut MuxState) {
    let mut packet = PacketBuf::new();
    packet.put(0, SYNC_BYTE);
    packet.put(1, 0x40 | ((pid >> 8) as u8 & 0x1f));
    packet.put(2, pid as u8);
    packet.put(3, 0x10 | state.take_counter(pid));
    packet.put(4, 0x00);
    packet.put_slice(5, section.get(..section.len().min(183)).unwrap_or(&[]));
    out.extend_from_slice(&packet.0);
}

fn pat_section() -> Vec<u8> {
    let body = [
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0x00,
        0x01,
        0xe0 | ((OUT_PMT_PID >> 8) as u8 & 0x1f),
        OUT_PMT_PID as u8,
    ];
    let section_length = body.len() + 4;
    section_with_crc(
        &[
            0x00,
            0xb0 | ((section_length >> 8) as u8 & 0x0f),
            section_length as u8,
        ],
        &body,
    )
}

fn pmt_section() -> Vec<u8> {
    let body = [
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0xe0 | ((OUT_VIDEO_PID >> 8) as u8 & 0x1f),
        OUT_VIDEO_PID as u8,
        0xf0,
        0x00,
        // H.264 video stream.
        0x1b,
        0xe0 | ((OUT_VIDEO_PID >> 8) as u8 & 0x1f),
        OUT_VIDEO_PID as u8,
        0xf0,
        0x00,
        // AAC audio stream.
        0x0f,
        0xe0 | ((OUT_AUDIO_PID >> 8) as u8 & 0x1f),
        OUT_AUDIO_PID as u8,
        0xf0,
        0x00,
    ];
    let section_length = body.len() + 4;
    section_with_crc(
        &[
            0x02,
            0xb0 | ((section_length >> 8) as u8 & 0x0f),
            section_length as u8,
        ],
        &body,
    )
}

fn section_with_crc(header: &[u8], body: &[u8]) -> Vec<u8> {
    let mut section = Vec::with_capacity(header.len() + body.len() + 4);
    section.extend_from_slice(header);
    section.extend_from_slice(body);
    let crc = crc32_mpeg(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

pub(crate) fn crc32_mpeg(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn pcr_bytes(timestamp: i64) -> [u8; 6] {
    let base = u64::try_from(timestamp.max(0)).unwrap_or(0) & ((1u64 << 33) - 1);
    [
        ((base >> 25) & 0xff) as u8,
        ((base >> 17) & 0xff) as u8,
        ((base >> 9) & 0xff) as u8,
        ((base >> 1) & 0xff) as u8,
        (((base & 1) as u8) << 7) | 0x7e,
        0x00,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_vector() {
        // CRC-32/MPEG-2 of "123456789".
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376_e6e7);
    }

    #[test]
    fn counters_wrap_per_pid() {
        let mut state = MuxState::default();
        for expected in (0..16).chain(0..2) {
            assert_eq!(state.take_counter(0x100), expected);
        }
        assert_eq!(state.next_counter(0x100), 2);
        assert_eq!(state.next_counter(0x101), 0);
    }

    #[test]
    fn pes_packets_are_aligned_and_sync() {
        let arena = vec![0xabu8; 1000];
        let mut events = vec![MediaEvent {
            kind: EventKind::Video,
            dts90: 90_000,
            pts90: 93_000,
            start: 0,
            len: 1000,
            keyframe: true,
        }];
        let mut state = MuxState::default();
        let out = mux_events_to_ts(&mut events, &arena, &mut state);
        assert_eq!(out.len() % PACKET_LEN, 0);
        assert!(out
            .as_chunks::<PACKET_LEN>()
            .0
            .iter()
            .all(|p| p[0] == SYNC_BYTE));
    }
}
