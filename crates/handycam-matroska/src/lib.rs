//! Minimal streaming Matroska writer for MJPEG video and PCM audio.

use std::io::{self, Seek, SeekFrom, Write};

use handycam_core::{
    AudioFormat, HEIGHT, MediaTiming, PcmEncoding, TimedAudioChunk, TimedVideoFrame, WIDTH,
};
use thiserror::Error;

const VIDEO_TRACK: u8 = 1;
const AUDIO_TRACK: u8 = 2;

#[derive(Debug, Error)]
pub enum MatroskaError {
    #[error("Matroska output failed: {0}")]
    Io(#[from] io::Error),
    #[error("audio event format changed during capture")]
    AudioFormatChanged,
    #[error("PCM payload has {actual} bytes; expected {expected} from its format and sample count")]
    InvalidPcmLength { expected: usize, actual: usize },
}

/// Incremental seekable-file writer using one millisecond Matroska timecode ticks.
///
/// Each packet starts a cluster. This has modest overhead but permits events
/// from independently scheduled capture backends without buffering or seeking.
pub struct MatroskaWriter<W: Write + Seek> {
    output: W,
    audio_format: AudioFormat,
    duration_offset: u64,
    maximum_end_us: u64,
}

impl<W: Write + Seek> MatroskaWriter<W> {
    pub fn new(mut output: W, audio_format: AudioFormat) -> Result<Self, MatroskaError> {
        let duration_offset = write_header(&mut output, audio_format)?;
        Ok(Self {
            output,
            audio_format,
            duration_offset,
            maximum_end_us: 0,
        })
    }

    pub fn write_video(
        &mut self,
        event: &TimedVideoFrame,
        timing: MediaTiming,
    ) -> Result<(), MatroskaError> {
        write_cluster(
            &mut self.output,
            timing.pts_us / 1_000,
            VIDEO_TRACK,
            &event.frame.jpeg,
        )?;
        self.maximum_end_us = self
            .maximum_end_us
            .max(timing.pts_us.saturating_add(timing.duration_us));
        Ok(())
    }

    pub fn write_audio(
        &mut self,
        event: &TimedAudioChunk,
        timing: MediaTiming,
    ) -> Result<(), MatroskaError> {
        if event.format != self.audio_format {
            return Err(MatroskaError::AudioFormatChanged);
        }
        let bytes_per_sample = match event.format.encoding() {
            PcmEncoding::Signed16LittleEndian => 2,
        };
        let expected = usize::try_from(event.chunk.sample_frames)
            .unwrap_or(usize::MAX)
            .saturating_mul(event.format.channels().get() as usize)
            .saturating_mul(bytes_per_sample);
        if event.chunk.pcm.len() != expected {
            return Err(MatroskaError::InvalidPcmLength {
                expected,
                actual: event.chunk.pcm.len(),
            });
        }
        write_cluster(
            &mut self.output,
            timing.pts_us / 1_000,
            AUDIO_TRACK,
            &event.chunk.pcm,
        )?;
        self.maximum_end_us = self
            .maximum_end_us
            .max(timing.pts_us.saturating_add(timing.duration_us));
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), MatroskaError> {
        self.write_duration()?;
        self.output.flush()?;
        Ok(())
    }

    pub fn into_inner(mut self) -> Result<W, MatroskaError> {
        self.flush()?;
        Ok(self.output)
    }

    fn write_duration(&mut self) -> io::Result<()> {
        let end = self.output.seek(SeekFrom::End(0))?;
        self.output.seek(SeekFrom::Start(self.duration_offset))?;
        let duration_ms = self.maximum_end_us as f64 / 1_000.0;
        self.output.write_all(&duration_ms.to_be_bytes())?;
        self.output.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

fn write_header(output: &mut (impl Write + Seek), audio_format: AudioFormat) -> io::Result<u64> {
    let mut ebml = Vec::new();
    uint_element(&mut ebml, 0x4286, 1)?;
    uint_element(&mut ebml, 0x42f7, 1)?;
    uint_element(&mut ebml, 0x42f2, 4)?;
    uint_element(&mut ebml, 0x42f3, 8)?;
    string_element(&mut ebml, 0x4282, "matroska")?;
    uint_element(&mut ebml, 0x4287, 4)?;
    uint_element(&mut ebml, 0x4285, 2)?;
    element(output, 0x1a45dfa3, &ebml)?;

    write_id(output, 0x18538067)?;
    output.write_all(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])?;

    let mut info = Vec::new();
    uint_element(&mut info, 0x2ad7b1, 1_000_000)?;
    let duration_payload_in_info = info.len() as u64 + 3;
    float_element(&mut info, 0x4489, 0.0)?;
    string_element(&mut info, 0x4d80, "handycam")?;
    string_element(&mut info, 0x5741, "handycam")?;
    let info_start = output.stream_position()?;
    let info_header_size = 4 + size_width(info.len() as u64)? as u64;
    element(output, 0x1549a966, &info)?;
    let duration_offset = info_start + info_header_size + duration_payload_in_info;

    let mut tracks = Vec::new();
    let mut video = Vec::new();
    uint_element(&mut video, 0xd7, u64::from(VIDEO_TRACK))?;
    uint_element(&mut video, 0x73c5, u64::from(VIDEO_TRACK))?;
    uint_element(&mut video, 0x83, 1)?;
    string_element(&mut video, 0x86, "V_MJPEG")?;
    uint_element(&mut video, 0x23e383, 40_000_000)?;
    let mut video_settings = Vec::new();
    uint_element(&mut video_settings, 0xb0, u64::from(WIDTH))?;
    uint_element(&mut video_settings, 0xba, u64::from(HEIGHT))?;
    element(&mut video, 0xe0, &video_settings)?;
    element(&mut tracks, 0xae, &video)?;

    let mut audio = Vec::new();
    uint_element(&mut audio, 0xd7, u64::from(AUDIO_TRACK))?;
    uint_element(&mut audio, 0x73c5, u64::from(AUDIO_TRACK))?;
    uint_element(&mut audio, 0x83, 2)?;
    string_element(&mut audio, 0x86, "A_PCM/INT/LIT")?;
    let mut audio_settings = Vec::new();
    float_element(
        &mut audio_settings,
        0xb5,
        f64::from(audio_format.sample_rate_hz().get()),
    )?;
    uint_element(
        &mut audio_settings,
        0x9f,
        u64::from(audio_format.channels().get()),
    )?;
    uint_element(&mut audio_settings, 0x6264, 16)?;
    element(&mut audio, 0xe1, &audio_settings)?;
    element(&mut tracks, 0xae, &audio)?;
    element(output, 0x1654ae6b, &tracks)?;
    Ok(duration_offset)
}

fn write_cluster(output: &mut impl Write, pts_ms: u64, track: u8, data: &[u8]) -> io::Result<()> {
    let mut cluster = Vec::with_capacity(data.len() + 24);
    uint_element(&mut cluster, 0xe7, pts_ms)?;
    let mut block = Vec::with_capacity(data.len() + 4);
    block.push(0x80 | track);
    block.extend_from_slice(&0_i16.to_be_bytes());
    block.push(0x80);
    block.extend_from_slice(data);
    element(&mut cluster, 0xa3, &block)?;
    element(output, 0x1f43b675, &cluster)
}

fn uint_element(output: &mut impl Write, id: u32, value: u64) -> io::Result<()> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    element(output, id, &bytes[first..])
}

fn float_element(output: &mut impl Write, id: u32, value: f64) -> io::Result<()> {
    element(output, id, &value.to_be_bytes())
}

fn string_element(output: &mut impl Write, id: u32, value: &str) -> io::Result<()> {
    element(output, id, value.as_bytes())
}

fn element(output: &mut impl Write, id: u32, payload: &[u8]) -> io::Result<()> {
    write_id(output, id)?;
    write_size(output, payload.len() as u64)?;
    output.write_all(payload)
}

fn write_id(output: &mut impl Write, id: u32) -> io::Result<()> {
    let bytes = id.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(3);
    output.write_all(&bytes[first..])
}

fn write_size(output: &mut impl Write, size: u64) -> io::Result<()> {
    let width = size_width(size)?;
    let encoded = size | (1_u64 << (7 * width));
    let bytes = encoded.to_be_bytes();
    output.write_all(&bytes[8 - width..])
}

fn size_width(size: u64) -> io::Result<usize> {
    (1..=8)
        .find(|width| size < (1_u64 << (7 * width)) - 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "EBML element is too large"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::Command;

    use handycam_core::{
        AudioChunk, CompressedFrame, MonotonicTimestamp, TimedAudioChunk, TimedVideoFrame,
        TimestampAccuracy,
    };

    use super::*;

    #[test]
    fn writes_streaming_matroska_with_both_tracks() {
        let format = AudioFormat::pcm_s16le(16_000, 2).unwrap();
        let mut writer = MatroskaWriter::new(Cursor::new(Vec::new()), format).unwrap();
        writer
            .write_video(
                &TimedVideoFrame {
                    observed_at: MonotonicTimestamp::ZERO,
                    frame: CompressedFrame {
                        sequence: 0,
                        timestamp_11bit_ms: 0,
                        timestamp_unwrapped_ms: 0,
                        timestamp_delta_to_next_ms: 40,
                        flags: 0,
                        jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
                    },
                    source_discontinuity: false,
                },
                MediaTiming {
                    pts_us: 0,
                    duration_us: 40_000,
                    observed_drift_us: 0,
                    discontinuity: false,
                },
            )
            .unwrap();
        writer
            .write_audio(
                &TimedAudioChunk {
                    observed_start: MonotonicTimestamp::ZERO,
                    timestamp_accuracy: TimestampAccuracy::Hardware,
                    format,
                    chunk: AudioChunk {
                        pcm: vec![0; 2_560],
                        start_sample_frame: 0,
                        sample_frames: 640,
                    },
                    source_discontinuity: false,
                },
                MediaTiming {
                    pts_us: 0,
                    duration_us: 40_000,
                    observed_drift_us: 0,
                    discontinuity: false,
                },
            )
            .unwrap();
        let output = writer.into_inner().unwrap().into_inner();
        assert!(output.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        assert!(output.windows(7).any(|bytes| bytes == b"V_MJPEG"));
        assert!(output.windows(13).any(|bytes| bytes == b"A_PCM/INT/LIT"));
        assert!(
            output
                .windows(4)
                .any(|bytes| bytes == [0xff, 0xd8, 0xff, 0xd9])
        );

        let path =
            std::env::temp_dir().join(format!("handycam-matroska-test-{}.mkv", std::process::id()));
        std::fs::write(&path, output).unwrap();
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration:stream=codec_name,codec_type,sample_rate,channels,width,height",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output();
        let _ = std::fs::remove_file(path);
        let probe = match probe {
            Ok(probe) => probe,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => panic!("could not run ffprobe: {error}"),
        };
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let streams = String::from_utf8_lossy(&probe.stdout);
        assert!(streams.contains("mjpeg,video,320,240"), "{streams}");
        assert!(streams.contains("pcm_s16le,audio,16000,2"), "{streams}");
        assert!(streams.contains("0.040000"), "{streams}");
    }
}
