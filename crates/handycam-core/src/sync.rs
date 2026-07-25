//! Cross-stream presentation-timestamp model.
//!
//! Video and audio each carry their own precise, jitter-free internal clock
//! (the camera's unwrapped millisecond timestamp for video, a sample-frame
//! count for audio), but nothing ties the two together. [`Synchronizer`]
//! anchors both onto one shared timeline by recording each stream's first
//! observed packet as an explicit synchronization event, then converting
//! every later packet's own internal clock into nanoseconds relative to that
//! shared origin. See `docs/next-steps-av-sync.md` for the motivating design.

use crate::audio::{AUDIO_SAMPLE_RATE_HZ, AudioChunk};
use crate::stream::CompressedFrame;

const NANOS_PER_MS: u64 = 1_000_000;
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Nominal 25 fps video frame interval.
const NOMINAL_VIDEO_DELTA_MS: u16 = 40;
/// Matches the existing 39-41ms acceptance band already used to flag
/// anomalous camera timestamp deltas.
const VIDEO_DELTA_TOLERANCE_MS: u16 = 1;

/// An opaque monotonic instant on the caller's host clock, in nanoseconds
/// since an arbitrary but caller-consistent reference (for example
/// `Instant::elapsed().as_nanos()` on the CLI side). Kept caller-supplied so
/// this module stays free of platform time dependencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostNanos(pub u64);

impl HostNanos {
    fn saturating_diff(self, earlier: HostNanos) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Whether an audio chunk's timestamp came from a hardware capture
/// timestamp or a monotonic read-time estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampQuality {
    Hardware,
    MonotonicFallback,
}

/// A detected break in an otherwise-contiguous stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discontinuity {
    VideoGap {
        expected_delta_ms: u16,
        actual_delta_ms: u16,
    },
    AudioGap {
        expected_sample_frame: u64,
        actual_sample_frame: u64,
    },
}

/// A decoded video frame placed on the shared presentation timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSyncFrame {
    pub frame: CompressedFrame,
    pub pts_ns: u64,
    pub duration_ns: u64,
    pub discontinuity: Option<Discontinuity>,
}

/// A captured audio chunk placed on the shared presentation timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSyncChunk {
    pub chunk: AudioChunk,
    pub pts_ns: u64,
    pub duration_ns: u64,
    pub quality: TimestampQuality,
    pub discontinuity: Option<Discontinuity>,
}

fn sample_frames_to_nanos(sample_frames: u64) -> u64 {
    sample_frames * NANOS_PER_SEC / u64::from(AUDIO_SAMPLE_RATE_HZ)
}

fn video_gap(delta_ms: u16) -> Option<Discontinuity> {
    let low = NOMINAL_VIDEO_DELTA_MS.saturating_sub(VIDEO_DELTA_TOLERANCE_MS);
    let high = NOMINAL_VIDEO_DELTA_MS + VIDEO_DELTA_TOLERANCE_MS;
    if (low..=high).contains(&delta_ms) {
        None
    } else {
        Some(Discontinuity::VideoGap {
            expected_delta_ms: NOMINAL_VIDEO_DELTA_MS,
            actual_delta_ms: delta_ms,
        })
    }
}

/// Anchors independently-clocked video and audio streams onto one shared
/// presentation timeline.
#[derive(Debug, Default)]
pub struct Synchronizer {
    global_t0: Option<HostNanos>,
    video_origin: Option<HostNanos>,
    audio_origin: Option<HostNanos>,
    expected_next_sample_frame: Option<u64>,
}

impl Synchronizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Establishes (or, if an earlier packet than any seen so far arrives,
    /// corrects) the shared `t = 0` reference. In the common case where
    /// events are observed in real-time order, this is fixed by whichever
    /// stream's very first packet arrives first and never changes again.
    fn ensure_global_t0(&mut self, at: HostNanos) -> HostNanos {
        match self.global_t0 {
            Some(t0) if t0 <= at => t0,
            _ => {
                self.global_t0 = Some(at);
                at
            }
        }
    }

    pub fn push_video(&mut self, frame: CompressedFrame, received_at: HostNanos) -> VideoSyncFrame {
        let global_t0 = self.ensure_global_t0(received_at);
        let origin = *self.video_origin.get_or_insert(received_at);
        let offset_ns = origin.saturating_diff(global_t0);
        let pts_ns = offset_ns + frame.timestamp_unwrapped_ms * NANOS_PER_MS;
        let duration_ns = u64::from(frame.timestamp_delta_to_next_ms) * NANOS_PER_MS;
        let discontinuity = video_gap(frame.timestamp_delta_to_next_ms);
        VideoSyncFrame {
            frame,
            pts_ns,
            duration_ns,
            discontinuity,
        }
    }

    pub fn push_audio(
        &mut self,
        chunk: AudioChunk,
        captured_at: HostNanos,
        quality: TimestampQuality,
    ) -> AudioSyncChunk {
        let global_t0 = self.ensure_global_t0(captured_at);
        let origin = *self.audio_origin.get_or_insert(captured_at);
        let offset_ns = origin.saturating_diff(global_t0);
        let pts_ns = offset_ns + sample_frames_to_nanos(chunk.start_sample_frame);
        let duration_ns = sample_frames_to_nanos(u64::from(chunk.sample_frames));
        let discontinuity = self.audio_gap(&chunk);
        AudioSyncChunk {
            chunk,
            pts_ns,
            duration_ns,
            quality,
            discontinuity,
        }
    }

    fn audio_gap(&mut self, chunk: &AudioChunk) -> Option<Discontinuity> {
        let gap = self
            .expected_next_sample_frame
            .filter(|&expected| expected != chunk.start_sample_frame)
            .map(|expected| Discontinuity::AudioGap {
                expected_sample_frame: expected,
                actual_sample_frame: chunk.start_sample_frame,
            });
        self.expected_next_sample_frame =
            Some(chunk.start_sample_frame + u64::from(chunk.sample_frames));
        gap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioClock;

    fn frame(
        sequence: u64,
        timestamp_11bit_ms: u16,
        unwrapped_ms: u64,
        delta_ms: u16,
    ) -> CompressedFrame {
        CompressedFrame {
            sequence,
            timestamp_11bit_ms,
            timestamp_unwrapped_ms: unwrapped_ms,
            timestamp_delta_to_next_ms: delta_ms,
            flags: 0,
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
        }
    }

    #[test]
    fn first_frame_of_each_stream_defines_its_own_offset_from_shared_origin() {
        let mut sync = Synchronizer::new();
        let video = sync.push_video(frame(0, 0, 0, 40), HostNanos(1_000_000_000));
        assert_eq!(video.pts_ns, 0);

        let mut clock = AudioClock::new();
        let chunk = clock.push_pcm(&[0; 32]).unwrap();
        // Audio started 250ms after the shared origin (the first video packet).
        let audio = sync.push_audio(chunk, HostNanos(1_250_000_000), TimestampQuality::Hardware);
        assert_eq!(audio.pts_ns, 250_000_000);
    }

    #[test]
    fn later_video_frames_advance_by_the_unwrapped_camera_clock() {
        let mut sync = Synchronizer::new();
        sync.push_video(frame(0, 100, 0, 40), HostNanos(0));
        let second = sync.push_video(frame(1, 140, 40, 40), HostNanos(40_000_000));
        // pts tracks the camera's own clock, not host-observed arrival time.
        assert_eq!(second.pts_ns, 40 * NANOS_PER_MS);
        assert!(second.discontinuity.is_none());
    }

    #[test]
    fn rollover_crossing_keeps_advancing_the_unwrapped_clock() {
        // 2028ms wraps to 20ms across the 2048ms modulus; the unwrapped clock
        // (already computed by StreamDecoder) must keep climbing regardless.
        let mut sync = Synchronizer::new();
        sync.push_video(frame(0, 2028, 39_988, 40), HostNanos(0));
        let wrapped = sync.push_video(frame(1, 20, 40_028, 40), HostNanos(40_000_000));
        assert_eq!(wrapped.pts_ns, 40_028 * NANOS_PER_MS);
        assert!(wrapped.discontinuity.is_none());
    }

    #[test]
    fn dropped_video_frame_is_reported_as_a_gap() {
        let mut sync = Synchronizer::new();
        sync.push_video(frame(0, 0, 0, 40), HostNanos(0));
        let after_drop = sync.push_video(frame(1, 40, 40, 80), HostNanos(40_000_000));
        assert_eq!(
            after_drop.discontinuity,
            Some(Discontinuity::VideoGap {
                expected_delta_ms: 40,
                actual_delta_ms: 80,
            })
        );
    }

    #[test]
    fn audio_xrun_resync_is_reported_as_a_gap() {
        let mut sync = Synchronizer::new();
        let mut clock = AudioClock::new();
        let first = clock.push_pcm(&[0; 32]).unwrap();
        let first_sync = sync.push_audio(first, HostNanos(0), TimestampQuality::Hardware);
        assert!(first_sync.discontinuity.is_none());

        clock.resync(10_000);
        let after_xrun = clock.push_pcm(&[0; 32]).unwrap();
        let gap = sync.push_audio(
            after_xrun,
            HostNanos(1_000_000_000),
            TimestampQuality::Hardware,
        );
        assert_eq!(
            gap.discontinuity,
            Some(Discontinuity::AudioGap {
                expected_sample_frame: 8,
                actual_sample_frame: 10_000,
            })
        );
    }

    #[test]
    fn contiguous_audio_chunks_report_no_gap() {
        let mut sync = Synchronizer::new();
        let mut clock = AudioClock::new();
        let first = clock.push_pcm(&[0; 32]).unwrap();
        sync.push_audio(first, HostNanos(0), TimestampQuality::Hardware);
        let second = clock.push_pcm(&[0; 32]).unwrap();
        let second_sync = sync.push_audio(second, HostNanos(1_000_000), TimestampQuality::Hardware);
        assert!(second_sync.discontinuity.is_none());
    }

    #[test]
    fn monotonic_fallback_quality_is_preserved_on_the_sync_chunk() {
        let mut sync = Synchronizer::new();
        let mut clock = AudioClock::new();
        let chunk = clock.push_pcm(&[0; 32]).unwrap();
        let synced = sync.push_audio(chunk, HostNanos(0), TimestampQuality::MonotonicFallback);
        assert_eq!(synced.quality, TimestampQuality::MonotonicFallback);
    }
}
