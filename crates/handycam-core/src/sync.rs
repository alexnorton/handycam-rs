//! Common A/V timeline construction.
//!
//! Arrival timestamps are supplied by a platform backend in microseconds from
//! one monotonic clock. Media positions continue to come from the camera and
//! audio sample clocks, so scheduling jitter does not become timestamp jitter.

use crate::{MonotonicTimestamp, TimedAudioChunk, TimedMediaEvent, TimedVideoFrame};
use thiserror::Error;

/// Timing attached to one encoded video frame or PCM block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaTiming {
    /// Presentation timestamp relative to the explicit stream-start event.
    pub pts_us: u64,
    pub duration_us: u64,
    /// Difference between the observed monotonic time and clock-derived PTS.
    /// A backend can use this to diagnose buffering and clock drift.
    pub observed_drift_us: i64,
    /// True when the media clock did not advance by the expected amount.
    pub discontinuity: bool,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SynchronizationError {
    #[error("media observation {observed} precedes stream start {stream_start}")]
    ObservationBeforeStreamStart {
        stream_start: MonotonicTimestamp,
        observed: MonotonicTimestamp,
    },
}

#[derive(Clone, Copy, Debug)]
struct ClockAnchor {
    media_position_us: u64,
    pts_us: u64,
}

/// Maps independent camera and audio clocks onto a shared monotonic timeline.
#[derive(Clone, Debug)]
pub struct AvSynchronizer {
    stream_start: MonotonicTimestamp,
    video_anchor: Option<ClockAnchor>,
    audio_anchor: Option<ClockAnchor>,
    last_video_position_us: Option<u64>,
    last_audio_end_us: Option<u64>,
}

impl AvSynchronizer {
    /// Start a timeline at an explicit platform-monotonic synchronization event.
    pub fn new(stream_start: MonotonicTimestamp) -> Self {
        Self {
            stream_start,
            video_anchor: None,
            audio_anchor: None,
            last_video_position_us: None,
            last_audio_end_us: None,
        }
    }

    /// Position a camera frame using its unwrapped Sony millisecond timestamp.
    pub fn video_timing(
        &mut self,
        event: &TimedVideoFrame,
    ) -> Result<MediaTiming, SynchronizationError> {
        let position_us = event.frame.timestamp_unwrapped_ms.saturating_mul(1_000);
        let duration_us = u64::from(event.frame.timestamp_delta_to_next_ms).saturating_mul(1_000);
        let observed_pts_us = self.observed_pts(event.observed_at)?;
        let anchor = *self.video_anchor.get_or_insert(ClockAnchor {
            media_position_us: position_us,
            pts_us: observed_pts_us,
        });
        let pts_us = anchored_pts(anchor, position_us);
        let discontinuity = event.source_discontinuity
            || self
                .last_video_position_us
                .is_some_and(|last| position_us != last);
        self.last_video_position_us = Some(position_us.saturating_add(duration_us));
        Ok(MediaTiming {
            pts_us,
            duration_us,
            observed_drift_us: signed_difference(observed_pts_us, pts_us),
            discontinuity,
        })
    }

    /// Position a PCM block using its first sample-frame and sample count.
    pub fn audio_timing(
        &mut self,
        event: &TimedAudioChunk,
    ) -> Result<MediaTiming, SynchronizationError> {
        let sample_rate_hz = event.format.sample_rate_hz();
        let position_us = samples_to_us(event.chunk.start_sample_frame, sample_rate_hz.get());
        let duration_us = samples_to_us(u64::from(event.chunk.sample_frames), sample_rate_hz.get());
        let observed_pts_us = self.observed_pts(event.observed_start)?;
        let anchor = *self.audio_anchor.get_or_insert(ClockAnchor {
            media_position_us: position_us,
            pts_us: observed_pts_us,
        });
        let pts_us = anchored_pts(anchor, position_us);
        let discontinuity = event.source_discontinuity
            || self
                .last_audio_end_us
                .is_some_and(|last| position_us != last);
        self.last_audio_end_us = Some(position_us.saturating_add(duration_us));
        Ok(MediaTiming {
            pts_us,
            duration_us,
            observed_drift_us: signed_difference(observed_pts_us, pts_us),
            discontinuity,
        })
    }

    pub fn event_timing(
        &mut self,
        event: &TimedMediaEvent,
    ) -> Result<MediaTiming, SynchronizationError> {
        match event {
            TimedMediaEvent::Video(frame) => self.video_timing(frame),
            TimedMediaEvent::Audio(chunk) => self.audio_timing(chunk),
        }
    }

    fn observed_pts(&self, observed: MonotonicTimestamp) -> Result<u64, SynchronizationError> {
        observed.duration_since(self.stream_start).ok_or(
            SynchronizationError::ObservationBeforeStreamStart {
                stream_start: self.stream_start,
                observed,
            },
        )
    }
}

fn anchored_pts(anchor: ClockAnchor, position_us: u64) -> u64 {
    if position_us >= anchor.media_position_us {
        anchor
            .pts_us
            .saturating_add(position_us - anchor.media_position_us)
    } else {
        anchor
            .pts_us
            .saturating_sub(anchor.media_position_us - position_us)
    }
}

fn samples_to_us(sample_frames: u64, sample_rate_hz: u32) -> u64 {
    sample_frames.saturating_mul(1_000_000) / u64::from(sample_rate_hz)
}

fn signed_difference(left: u64, right: u64) -> i64 {
    i128::from(left)
        .saturating_sub(i128::from(right))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioChunk, AudioFormat, CompressedFrame, TimestampAccuracy};

    fn video(timestamp_ms: u64, duration_ms: u16, observed_us: u64) -> TimedVideoFrame {
        TimedVideoFrame {
            observed_at: MonotonicTimestamp::from_micros(observed_us),
            frame: CompressedFrame {
                sequence: timestamp_ms / 40,
                timestamp_11bit_ms: (timestamp_ms & 0x07ff) as u16,
                timestamp_unwrapped_ms: timestamp_ms,
                timestamp_delta_to_next_ms: duration_ms,
                flags: 0,
                jpeg: Vec::new(),
            },
            source_discontinuity: false,
        }
    }

    fn audio(start: u64, count: u32, observed_us: u64) -> TimedAudioChunk {
        TimedAudioChunk {
            observed_start: MonotonicTimestamp::from_micros(observed_us),
            timestamp_accuracy: TimestampAccuracy::Hardware,
            format: AudioFormat::pcm_s16le(16_000, 2).unwrap(),
            chunk: AudioChunk {
                pcm: Vec::new(),
                start_sample_frame: start,
                sample_frames: count,
            },
            source_discontinuity: false,
        }
    }

    #[test]
    fn preserves_initial_audio_video_offset() {
        let mut sync = AvSynchronizer::new(MonotonicTimestamp::from_micros(1_000_000));
        let video_timing = sync.video_timing(&video(0, 40, 1_120_000)).unwrap();
        let audio_timing = sync.audio_timing(&audio(0, 640, 1_370_000)).unwrap();
        assert_eq!(video_timing.pts_us, 120_000);
        assert_eq!(audio_timing.pts_us, 370_000);
        assert_eq!(audio_timing.pts_us - video_timing.pts_us, 250_000);
    }

    #[test]
    fn media_clocks_remove_arrival_jitter_but_report_it_as_drift() {
        let mut sync = AvSynchronizer::new(MonotonicTimestamp::from_micros(10_000));
        assert_eq!(
            sync.video_timing(&video(0, 40, 20_000)).unwrap().pts_us,
            10_000
        );
        let second = sync.video_timing(&video(40, 40, 63_000)).unwrap();
        assert_eq!(second.pts_us, 50_000);
        assert_eq!(second.observed_drift_us, 3_000);
        assert!(!second.discontinuity);
    }

    #[test]
    fn detects_dropped_video_and_audio_ranges() {
        let mut sync = AvSynchronizer::new(MonotonicTimestamp::ZERO);
        sync.video_timing(&video(0, 40, 0)).unwrap();
        assert!(
            sync.video_timing(&video(80, 40, 80_000))
                .unwrap()
                .discontinuity
        );
        sync.audio_timing(&audio(0, 160, 0)).unwrap();
        assert!(
            sync.audio_timing(&audio(320, 160, 20_000))
                .unwrap()
                .discontinuity
        );
    }

    #[test]
    fn audio_sample_count_has_exact_pal_frame_duration() {
        let mut sync = AvSynchronizer::new(MonotonicTimestamp::ZERO);
        let timing = sync.audio_timing(&audio(0, 640, 0)).unwrap();
        assert_eq!(timing.duration_us, 40_000);
    }

    #[test]
    fn supports_non_sony_audio_clock_rates() {
        let mut event = audio(48_000, 480, 1_000_000);
        event.format = AudioFormat::pcm_s16le(48_000, 1).unwrap();
        let mut sync = AvSynchronizer::new(MonotonicTimestamp::ZERO);
        let timing = sync.audio_timing(&event).unwrap();
        assert_eq!(timing.duration_us, 10_000);
    }

    #[test]
    fn rejects_events_from_before_the_session_clock_origin() {
        let mut sync = AvSynchronizer::new(MonotonicTimestamp::from_micros(100));
        assert_eq!(
            sync.video_timing(&video(0, 40, 99)),
            Err(SynchronizationError::ObservationBeforeStreamStart {
                stream_start: MonotonicTimestamp::from_micros(100),
                observed: MonotonicTimestamp::from_micros(99),
            })
        );
    }
}
