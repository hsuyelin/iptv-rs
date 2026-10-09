//! Synthetic streams and cipher stand-ins for tests and benchmarks.

use crate::{
    cipher::{CipherError, PayloadCipher},
    ts::mux::{mux_events_to_ts, EventKind, MediaEvent, MuxState},
};

/// Shape of a generated segment.
#[derive(Debug, Clone, Copy)]
pub struct StreamSpec {
    /// Number of video access units.
    pub video_frames: usize,
    /// A keyframe (with SPS and PPS) every this many frames; at least 1.
    pub keyframe_every: usize,
    /// Size in bytes of each slice NAL body.
    pub nal_bytes: usize,
    /// ADTS frames attached to each video frame.
    pub audio_frames: usize,
    /// Seed for the byte generator.
    pub seed: u64,
}

impl StreamSpec {
    /// A small stream of 12 frames.
    pub const SMALL: Self = Self {
        video_frames: 12,
        keyframe_every: 6,
        nal_bytes: 700,
        audio_frames: 2,
        seed: 7,
    };

    /// A segment of roughly 2 MiB, the size used by the benchmarks.
    pub const TWO_MIB: Self = Self {
        video_frames: 120,
        keyframe_every: 30,
        nal_bytes: 16_000,
        audio_frames: 3,
        seed: 11,
    };
}

struct Lcg(u64);

impl Lcg {
    /// Next byte in `0x10..=0xff`, so generated data never contains a start code.
    fn byte(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let value = (self.0 >> 33) as u8;
        0x10 | (value & 0xef) | (value >> 4 & 0x0f)
    }

    fn fill(&mut self, out: &mut Vec<u8>, len: usize) {
        out.extend((0..len).map(|_| self.byte()));
    }
}

fn annex_b(out: &mut Vec<u8>, header: u8, rest: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1, header]);
    out.extend_from_slice(rest);
}

fn adts_frame(rng: &mut Lcg, body_len: usize) -> Vec<u8> {
    let length = body_len + 7;
    let mut frame = vec![
        0xff,
        0xf1,
        0x50,
        0x80 | ((length >> 11) & 0x03) as u8,
        ((length >> 3) & 0xff) as u8,
        (((length & 0x07) << 5) | 0x1f) as u8,
        0xfc,
    ];
    rng.fill(&mut frame, body_len);
    frame
}

/// Builds a valid H.264/AAC transport stream.
///
/// The SPS carries the marker the remuxer uses to turn decoding on, so a cipher sees
/// every slice NAL after the first SPS.
pub fn synthetic_segment(spec: &StreamSpec) -> Vec<u8> {
    let mut rng = Lcg(spec.seed);
    let mut arena: Vec<u8> = Vec::new();
    let mut events = Vec::new();
    let every = spec.keyframe_every.max(1);
    for frame in 0..spec.video_frames {
        let dts = i64::try_from(frame).unwrap_or(0) * 3_600 + 90_000;
        let start = arena.len();
        let keyframe = frame % every == 0;
        if keyframe {
            let mut sps = vec![0x64, 0x01];
            rng.fill(&mut sps, 10);
            annex_b(&mut arena, 0x67, &sps);
            let mut pps = Vec::new();
            rng.fill(&mut pps, 6);
            annex_b(&mut arena, 0x68, &pps);
        }
        let mut slice = Vec::new();
        rng.fill(&mut slice, spec.nal_bytes);
        annex_b(&mut arena, if keyframe { 0x65 } else { 0x41 }, &slice);
        events.push(MediaEvent {
            kind: EventKind::Video,
            dts90: dts,
            pts90: dts + 7_200,
            start,
            len: arena.len() - start,
            keyframe,
        });
        let audio_start = arena.len();
        for _ in 0..spec.audio_frames {
            let frame_bytes = adts_frame(&mut rng, 180);
            arena.extend_from_slice(&frame_bytes);
        }
        events.push(MediaEvent {
            kind: EventKind::Audio,
            dts90: dts + 7_200,
            pts90: dts + 7_200,
            start: audio_start,
            len: arena.len() - audio_start,
            keyframe: false,
        });
    }
    let mut state = MuxState::default();
    mux_events_to_ts(&mut events, &arena, &mut state)
}

/// Returns the input NAL unchanged.
#[derive(Debug, Default)]
pub struct PassThroughCipher {
    /// Number of `tick` calls.
    pub ticks: usize,
    /// Number of `decode` calls.
    pub decodes: usize,
}

impl PayloadCipher for PassThroughCipher {
    fn tick(&mut self) -> Result<(), CipherError> {
        self.ticks += 1;
        Ok(())
    }

    fn decode(&mut self, nal: &[u8]) -> Result<Vec<u8>, CipherError> {
        self.decodes += 1;
        Ok(nal.to_vec())
    }
}

/// Length-preserving cipher that flips bits after the first two bytes of each NAL.
#[derive(Debug, Default)]
pub struct XorCipher {
    /// Number of `decode` calls.
    pub decodes: usize,
}

impl PayloadCipher for XorCipher {
    fn tick(&mut self) -> Result<(), CipherError> {
        Ok(())
    }

    fn decode(&mut self, nal: &[u8]) -> Result<Vec<u8>, CipherError> {
        self.decodes += 1;
        Ok(nal
            .iter()
            .enumerate()
            .map(|(i, &b)| if i < 2 { b } else { b ^ 0x5a })
            .collect())
    }
}

/// Fails on the `fail_at`-th `decode` call (1-based) and passes through otherwise.
#[derive(Debug)]
pub struct FailingCipher {
    /// 1-based index of the failing call.
    pub fail_at: usize,
    calls: usize,
}

impl FailingCipher {
    /// Creates a cipher that fails on the given call.
    pub fn new(fail_at: usize) -> Self {
        Self { fail_at, calls: 0 }
    }
}

impl PayloadCipher for FailingCipher {
    fn tick(&mut self) -> Result<(), CipherError> {
        Ok(())
    }

    fn decode(&mut self, nal: &[u8]) -> Result<Vec<u8>, CipherError> {
        self.calls += 1;
        if self.calls == self.fail_at {
            return Err(CipherError::new("injected failure"));
        }
        Ok(nal.to_vec())
    }
}

/// Returns a cipher output longer than its input.
#[derive(Debug, Default)]
pub struct GrowingCipher;

impl PayloadCipher for GrowingCipher {
    fn tick(&mut self) -> Result<(), CipherError> {
        Ok(())
    }

    fn decode(&mut self, nal: &[u8]) -> Result<Vec<u8>, CipherError> {
        let mut grown = nal.to_vec();
        grown.push(0x11);
        Ok(grown)
    }
}

/// FNV-1a hash used to pin output bytes in characterization tests.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
