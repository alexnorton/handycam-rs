//! A minimal streaming Matroska (`.mkv`) writer for exactly the shape this
//! project needs: one MJPEG video track and one PCM audio track, written as
//! `SimpleBlock`s with explicit presentation timestamps.
//!
//! This deliberately does not depend on a general-purpose muxing crate.
//! Rust "matroska" crates on crates.io are read-only demuxers, and the
//! write-capable options wrap libwebm C++ over FFI, which would reintroduce
//! unsafe/non-Rust code the project has otherwise confined to
//! `handycam-libusb`'s narrow libusb transfer lifecycle. The EBML element
//! set required to produce a valid, playable file is small enough to write
//! directly.
//!
//! `Segment` and `Cluster` are written with EBML's "unknown size" marker, so
//! nothing is ever rewritten once its true length becomes known and only
//! [`std::io::Write`] is required -- no `Seek`, matching the existing
//! pipe-friendly `stream --output -` design. There is no `Cues` element in
//! this first version, so seeking in the produced file relies on a player's
//! own linear/keyframe scan; this is a documented limitation, not a decoding
//! defect.

#![forbid(unsafe_code)]

mod ebml;
mod ids;

use std::io::{self, Write};

use thiserror::Error;

use ebml::{UNKNOWN_SIZE, element, encode_uint, encode_vint, master};

/// One tick of the file's timestamp scale is 1ms; Matroska timestamps
/// (Cluster `Timestamp` and Block relative timecodes) are expressed in
/// ticks. Audio and video presentation timestamps in this project already
/// land on whole milliseconds in the common case (16kHz audio delivers in
/// 1ms USB-frame-sized chunks; camera timestamps are themselves integer
/// milliseconds), so this loses no meaningful precision.
const TIMESTAMP_SCALE_NS: u64 = 1_000_000;

/// Cluster span budget in ticks (1 tick = 1ms here), chosen well under the
/// SimpleBlock relative-timecode field's `i16` range so a cluster never
/// needs to close early because of an overflow.
const MAX_CLUSTER_SPAN_TICKS: u64 = 1_000;

const VIDEO_TRACK_NUMBER: u64 = 1;
const AUDIO_TRACK_NUMBER: u64 = 2;

const MUXING_APP: &str = concat!("handycam-mkv/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTrackConfig {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioTrackConfig {
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
}

#[derive(Debug, Error)]
pub enum MuxError {
    #[error("I/O error writing Matroska output: {0}")]
    Io(#[from] io::Error),
    #[error(
        "relative block timecode {relative_ticks} does not fit the 16-bit Matroska field; the interleaved streams have drifted too far apart"
    )]
    TimecodeOverflow { relative_ticks: i64 },
}

/// Writes a streaming Matroska container with one video and one audio
/// track. Frames/chunks may be interleaved in any order as long as each
/// stream's own timestamps are non-decreasing.
pub struct MatroskaWriter<W: Write> {
    writer: W,
    cluster_open: bool,
    cluster_start_ticks: u64,
}

impl<W: Write> MatroskaWriter<W> {
    pub fn create(
        mut writer: W,
        video: VideoTrackConfig,
        audio: AudioTrackConfig,
    ) -> Result<Self, MuxError> {
        writer.write_all(&ebml_header())?;
        writer.write_all(&ids::SEGMENT)?;
        writer.write_all(&UNKNOWN_SIZE)?;
        writer.write_all(&segment_info())?;
        writer.write_all(&tracks(&video, &audio))?;
        Ok(Self {
            writer,
            cluster_open: false,
            cluster_start_ticks: 0,
        })
    }

    pub fn write_video_frame(&mut self, pts_ns: u64, jpeg: &[u8]) -> Result<(), MuxError> {
        self.write_block(VIDEO_TRACK_NUMBER, pts_ns, true, jpeg)
    }

    pub fn write_audio_chunk(&mut self, pts_ns: u64, pcm: &[u8]) -> Result<(), MuxError> {
        self.write_block(AUDIO_TRACK_NUMBER, pts_ns, true, pcm)
    }

    pub fn finish(mut self) -> Result<W, MuxError> {
        self.writer.flush()?;
        Ok(self.writer)
    }

    fn write_block(
        &mut self,
        track_number: u64,
        pts_ns: u64,
        keyframe: bool,
        payload: &[u8],
    ) -> Result<(), MuxError> {
        let ticks = pts_ns / TIMESTAMP_SCALE_NS;
        self.ensure_cluster(ticks)?;

        // Block relative timecodes are signed: a block from an interleaved
        // second stream can legitimately land slightly before the cluster's
        // own `Timestamp`, so only reject a magnitude the 16-bit field
        // cannot hold, not merely a negative one.
        let relative_ticks = ticks as i64 - self.cluster_start_ticks as i64;
        let relative_ticks_i16 = i16::try_from(relative_ticks)
            .map_err(|_| MuxError::TimecodeOverflow { relative_ticks })?;

        let mut body = Vec::with_capacity(4 + payload.len());
        body.extend_from_slice(&encode_vint(track_number));
        body.extend_from_slice(&relative_ticks_i16.to_be_bytes());
        body.push(if keyframe { 0x80 } else { 0x00 });
        body.extend_from_slice(payload);
        self.writer.write_all(&element(&ids::SIMPLE_BLOCK, &body))?;
        Ok(())
    }

    fn ensure_cluster(&mut self, ticks: u64) -> Result<(), MuxError> {
        let needs_new_cluster = !self.cluster_open
            || ticks.saturating_sub(self.cluster_start_ticks) > MAX_CLUSTER_SPAN_TICKS;
        if needs_new_cluster {
            self.writer.write_all(&ids::CLUSTER)?;
            self.writer.write_all(&UNKNOWN_SIZE)?;
            self.writer
                .write_all(&element(&ids::TIMESTAMP, &encode_uint(ticks)))?;
            self.cluster_open = true;
            self.cluster_start_ticks = ticks;
        }
        Ok(())
    }
}

fn ebml_header() -> Vec<u8> {
    master(
        &ids::EBML,
        &[
            element(&ids::EBML_VERSION, &encode_uint(1)),
            element(&ids::EBML_READ_VERSION, &encode_uint(1)),
            element(&ids::EBML_MAX_ID_LENGTH, &encode_uint(4)),
            element(&ids::EBML_MAX_SIZE_LENGTH, &encode_uint(8)),
            element(&ids::DOC_TYPE, b"matroska"),
            element(&ids::DOC_TYPE_VERSION, &encode_uint(4)),
            element(&ids::DOC_TYPE_READ_VERSION, &encode_uint(2)),
        ],
    )
}

fn segment_info() -> Vec<u8> {
    master(
        &ids::SEGMENT_INFO,
        &[
            element(&ids::TIMESTAMP_SCALE, &encode_uint(TIMESTAMP_SCALE_NS)),
            element(&ids::MUXING_APP, MUXING_APP.as_bytes()),
            element(&ids::WRITING_APP, MUXING_APP.as_bytes()),
        ],
    )
}

fn tracks(video: &VideoTrackConfig, audio: &AudioTrackConfig) -> Vec<u8> {
    let video_entry = master(
        &ids::TRACK_ENTRY,
        &[
            element(&ids::TRACK_NUMBER, &encode_uint(VIDEO_TRACK_NUMBER)),
            element(&ids::TRACK_UID, &encode_uint(VIDEO_TRACK_NUMBER)),
            element(&ids::TRACK_TYPE, &encode_uint(1)),
            element(&ids::CODEC_ID, b"V_MJPEG"),
            master(
                &ids::VIDEO,
                &[
                    element(&ids::PIXEL_WIDTH, &encode_uint(u64::from(video.width))),
                    element(&ids::PIXEL_HEIGHT, &encode_uint(u64::from(video.height))),
                ],
            ),
        ],
    );
    let audio_entry = master(
        &ids::TRACK_ENTRY,
        &[
            element(&ids::TRACK_NUMBER, &encode_uint(AUDIO_TRACK_NUMBER)),
            element(&ids::TRACK_UID, &encode_uint(AUDIO_TRACK_NUMBER)),
            element(&ids::TRACK_TYPE, &encode_uint(2)),
            element(&ids::CODEC_ID, b"A_PCM/INT/LIT"),
            master(
                &ids::AUDIO,
                &[
                    element(
                        &ids::SAMPLING_FREQUENCY,
                        &(audio.sample_rate as f32).to_be_bytes(),
                    ),
                    element(&ids::CHANNELS, &encode_uint(u64::from(audio.channels))),
                    element(&ids::BIT_DEPTH, &encode_uint(u64::from(audio.bit_depth))),
                ],
            ),
        ],
    );
    master(&ids::TRACKS, &[video_entry, audio_entry])
}

#[cfg(test)]
mod tests;
