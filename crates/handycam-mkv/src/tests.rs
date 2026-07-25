use super::*;

fn track_configs() -> (VideoTrackConfig, AudioTrackConfig) {
    (
        VideoTrackConfig {
            width: 4,
            height: 2,
        },
        AudioTrackConfig {
            sample_rate: 16_000,
            channels: 2,
            bit_depth: 16,
        },
    )
}

fn simple_block(track_number: u64, relative_ticks: i16, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_vint(track_number));
    body.extend_from_slice(&relative_ticks.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(payload);
    element(&ids::SIMPLE_BLOCK, &body)
}

fn cluster(start_ticks: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&ids::CLUSTER);
    out.extend_from_slice(&UNKNOWN_SIZE);
    out.extend_from_slice(&element(&ids::TIMESTAMP, &encode_uint(start_ticks)));
    for block in blocks {
        out.extend_from_slice(block);
    }
    out
}

#[test]
fn produces_the_expected_byte_stream_for_a_small_two_cluster_fixture() {
    let (video, audio) = track_configs();
    let jpeg1 = [0xAA, 0xBB];
    let pcm = [0x01, 0x02, 0x03, 0x04];
    let jpeg2 = [0xCC];
    let jpeg3 = [0xDD, 0xEE, 0xFF];

    let mut writer = MatroskaWriter::create(Vec::new(), video, audio).unwrap();
    writer.write_video_frame(0, &jpeg1).unwrap();
    writer.write_audio_chunk(5_000_000, &pcm).unwrap();
    writer.write_video_frame(40_000_000, &jpeg2).unwrap();
    // 2000ms is well past the 1-second cluster span budget, so this must
    // start a new cluster rather than keep growing the first one.
    writer.write_video_frame(2_000_000_000, &jpeg3).unwrap();
    let output = writer.finish().unwrap();

    let mut expected = Vec::new();
    expected.extend_from_slice(&ebml_header());
    expected.extend_from_slice(&ids::SEGMENT);
    expected.extend_from_slice(&UNKNOWN_SIZE);
    expected.extend_from_slice(&segment_info());
    expected.extend_from_slice(&tracks(&video, &audio));
    expected.extend_from_slice(&cluster(
        0,
        &[
            simple_block(VIDEO_TRACK_NUMBER, 0, true, &jpeg1),
            simple_block(AUDIO_TRACK_NUMBER, 5, true, &pcm),
            simple_block(VIDEO_TRACK_NUMBER, 40, true, &jpeg2),
        ],
    ));
    expected.extend_from_slice(&cluster(
        2_000,
        &[simple_block(VIDEO_TRACK_NUMBER, 0, true, &jpeg3)],
    ));

    assert_eq!(output, expected);
}

#[test]
fn output_starts_with_the_ebml_magic_number_and_advertises_matroska() {
    let (video, audio) = track_configs();
    let mut writer = MatroskaWriter::create(Vec::new(), video, audio).unwrap();
    writer.write_video_frame(0, &[0xFF]).unwrap();
    let output = writer.finish().unwrap();

    assert_eq!(&output[..4], &ids::EBML);
    assert!(
        output
            .windows(b"matroska".len())
            .any(|window| window == b"matroska")
    );
    assert!(
        output
            .windows(b"V_MJPEG".len())
            .any(|window| window == b"V_MJPEG")
    );
    assert!(
        output
            .windows(b"A_PCM/INT/LIT".len())
            .any(|window| window == b"A_PCM/INT/LIT")
    );
}

#[test]
fn a_slightly_earlier_block_from_the_other_stream_reuses_the_cluster_with_a_negative_relative_timecode()
 {
    let (video, audio) = track_configs();
    let mut writer = MatroskaWriter::create(Vec::new(), video, audio).unwrap();
    // Video opens the cluster at 1000 ticks; an audio chunk that arrived
    // fractionally earlier on the shared timeline (990 ticks) is still
    // squarely within the same cluster's window and must be encoded with a
    // negative relative timecode rather than forcing a new cluster.
    writer.write_video_frame(1_000_000_000, &[0x01]).unwrap();
    writer.write_audio_chunk(990_000_000, &[0x02]).unwrap();
    let output = writer.finish().unwrap();

    let expected_cluster = cluster(
        1_000,
        &[
            simple_block(VIDEO_TRACK_NUMBER, 0, true, &[0x01]),
            simple_block(AUDIO_TRACK_NUMBER, -10, true, &[0x02]),
        ],
    );
    assert!(
        output
            .windows(expected_cluster.len())
            .any(|window| window == expected_cluster)
    );
}

#[test]
fn a_relative_timecode_too_far_from_the_cluster_start_is_rejected() {
    let (video, audio) = track_configs();
    let mut writer = MatroskaWriter::create(Vec::new(), video, audio).unwrap();
    // Opens a cluster at 100_000 ticks; ensure_cluster only rolls a new
    // cluster on a large *forward* jump, so a block far enough *behind* it
    // stays in the same cluster and must be rejected once its relative
    // timecode no longer fits the signed 16-bit field.
    writer.write_video_frame(100_000_000_000, &[0x01]).unwrap();
    let error = writer.write_audio_chunk(0, &[0x02]).unwrap_err();
    assert!(matches!(
        error,
        MuxError::TimecodeOverflow {
            relative_ticks: -100_000
        }
    ));
}
