//! Characterization and robustness tests. The pinned digests were produced by the
//! original single-crate implementation with the same `XorCipher` stand-in.

use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::{
    decrypt_and_remux,
    testkit::{
        fnv1a64, synthetic_segment, FailingCipher, GrowingCipher, PassThroughCipher,
        StreamSpec, XorCipher,
    },
    MediaError, MuxState, RemuxStats, VideoState,
};

const PACKET: usize = 188;

fn remux_with<C: crate::PayloadCipher>(
    cipher: &mut C,
    input: &[u8],
) -> Result<(Vec<u8>, RemuxStats), MediaError> {
    decrypt_and_remux(
        cipher,
        &mut VideoState::default(),
        &mut MuxState::default(),
        input,
    )
}

#[test]
fn remux_matches_original_output_small() {
    let input = synthetic_segment(&StreamSpec::SMALL);
    let (out, stats) = remux_with(&mut XorCipher::default(), &input).unwrap();
    assert_eq!((fnv1a64(&out), out.len()), (0x2330_6539_9371_0560, 20_680));
    assert_eq!(stats.video_sample_count, 12);
    assert_eq!(stats.audio_sample_count, 24);
    assert_eq!(stats.decoded_nals, 12);
    assert_eq!(stats.sps_side_effects, 2);
    assert_eq!(stats.output_bytes, out.len());
}

#[test]
fn remux_matches_original_output_two_mib() {
    let input = synthetic_segment(&StreamSpec::TWO_MIB);
    let (out, stats) = remux_with(&mut XorCipher::default(), &input).unwrap();
    assert_eq!(
        (fnv1a64(&out), out.len()),
        (0x35aa_f79f_a594_37cc, 2_121_016)
    );
    assert_eq!(stats.changed_bytes, 1_919_880);
}

#[test]
fn pass_through_round_trip_keeps_sample_counts() {
    let input = synthetic_segment(&StreamSpec::SMALL);
    let mut cipher = PassThroughCipher::default();
    let (out, stats) = remux_with(&mut cipher, &input).unwrap();
    assert_eq!(out.len() % PACKET, 0);
    assert!(out
        .as_chunks::<PACKET>()
        .0
        .iter()
        .all(|packet| packet[0] == 0x47));
    // The remuxed stream parses again with identical sample counts.
    let (again, second) = remux_with(&mut PassThroughCipher::default(), &out).unwrap();
    assert_eq!(second.video_sample_count, stats.video_sample_count);
    assert_eq!(second.audio_sample_count, stats.audio_sample_count);
    assert_eq!(again.len(), out.len());
    assert_eq!(cipher.ticks, stats.nal_count);
}

#[test]
fn continuity_counters_continue_across_segments() {
    let input = synthetic_segment(&StreamSpec::SMALL);
    let mut video = VideoState::default();
    let mut mux = MuxState::default();
    let mut cipher = PassThroughCipher::default();
    let (first, _) =
        decrypt_and_remux(&mut cipher, &mut video, &mut mux, &input).unwrap();
    let last_counter = |data: &[u8], pid: u16| {
        data.as_chunks::<PACKET>()
            .0
            .iter()
            .filter(|p| ((u16::from(p[1] & 0x1f) << 8) | u16::from(p[2])) == pid)
            .map(|p| p[3] & 0x0f)
            .next_back()
    };
    let expected_video = (last_counter(&first, 0x100).unwrap() + 1) & 0x0f;
    let expected_audio = (last_counter(&first, 0x101).unwrap() + 1) & 0x0f;
    let (second, _) =
        decrypt_and_remux(&mut cipher, &mut video, &mut mux, &input).unwrap();
    let first_counter = |data: &[u8], pid: u16| {
        data.as_chunks::<PACKET>()
            .0
            .iter()
            .find(|p| ((u16::from(p[1] & 0x1f) << 8) | u16::from(p[2])) == pid)
            .map(|p| p[3] & 0x0f)
    };
    assert_eq!(first_counter(&second, 0x100), Some(expected_video));
    assert_eq!(first_counter(&second, 0x101), Some(expected_audio));
    assert_eq!(
        mux.next_counter(0x100),
        (last_counter(&second, 0x100).unwrap() + 1) & 0x0f
    );
}

#[test]
fn output_is_ordered_by_decode_time_after_first_video() {
    let input = synthetic_segment(&StreamSpec::SMALL);
    let (out, _) = remux_with(&mut PassThroughCipher::default(), &input).unwrap();
    let pes = crate::ts::pes::collect_pes(
        &out,
        &crate::ts::packet::parse_packets(&out).unwrap(),
        0x101,
    );
    let times: Vec<i64> = pes.iter().filter_map(|p| p.pts).collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn empty_and_misaligned_inputs_are_typed_errors() {
    assert!(matches!(
        remux_with(&mut XorCipher::default(), &[]),
        Err(MediaError::Empty)
    ));
    let mut input = synthetic_segment(&StreamSpec::SMALL);
    input.extend_from_slice(&[0x47; 17]);
    assert!(matches!(
        remux_with(&mut XorCipher::default(), &input),
        Err(MediaError::Misaligned { len }) if len % PACKET == 17
    ));
}

#[test]
fn stream_without_pmt_or_streams_is_rejected() {
    let mut input = vec![0xffu8; PACKET * 3];
    for packet in input.as_chunks_mut::<PACKET>().0.iter_mut() {
        packet[0] = 0x47;
        packet[1] = 0x1f;
        packet[2] = 0xff;
        packet[3] = 0x10;
    }
    assert!(matches!(
        remux_with(&mut XorCipher::default(), &input),
        Err(MediaError::NoPmt)
    ));
}

#[test]
fn cipher_failure_names_stage_and_pes_index() {
    let input = synthetic_segment(&StreamSpec::SMALL);
    let error = remux_with(&mut FailingCipher::new(3), &input).unwrap_err();
    match error {
        MediaError::Cipher { stage, index, .. } => {
            assert_eq!(stage, "decode");
            // Decode #1 is the SPS and #2 the IDR slice of PES 0, so #3 is PES 1.
            assert_eq!(index, 1);
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn growing_cipher_output_is_rejected() {
    let input = synthetic_segment(&StreamSpec::SMALL);
    assert!(matches!(
        remux_with(&mut GrowingCipher, &input),
        Err(MediaError::CipherGrew { before, after, .. }) if after == before + 1
    ));
}

#[test]
fn garbage_never_panics() {
    let mut rng = StdRng::seed_from_u64(0xdead_beef);
    for _ in 0..200 {
        let len = rng.gen_range(0..=64 * 1024);
        let mut data: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
        // Half of the cases get valid sync bytes to reach the deeper parsers.
        if rng.gen_bool(0.5) {
            for packet in data.chunks_mut(PACKET) {
                packet[0] = 0x47;
            }
            data.truncate(data.len() / PACKET * PACKET);
        }
        let _ = remux_with(&mut XorCipher::default(), &data);
    }
}

#[test]
fn corrupted_valid_stream_never_panics() {
    let base = synthetic_segment(&StreamSpec::SMALL);
    let mut rng = StdRng::seed_from_u64(42);
    for _ in 0..300 {
        let mut data = base.clone();
        for _ in 0..rng.gen_range(1..40) {
            let index = rng.gen_range(0..data.len());
            // Keep sync bytes intact so parsing proceeds past the packet layer.
            if index % PACKET != 0 {
                data[index] = rng.gen();
            }
        }
        let _ = remux_with(&mut XorCipher::default(), &data);
    }
}
