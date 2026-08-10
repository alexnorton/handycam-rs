//! Portable timed-media contracts shared by native and browser backends.

use std::fmt;
use std::num::NonZeroU32;

use thiserror::Error;

use crate::{AudioChunk, CompressedFrame};

/// A timestamp in a backend-defined monotonic clock domain.
///
/// All events in one capture session must use the same domain. The numerical
/// epoch has no meaning outside that session and need not be wall-clock time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicTimestamp(u64);

impl MonotonicTimestamp {
    pub const ZERO: Self = Self(0);

    pub const fn from_micros(microseconds: u64) -> Self {
        Self(microseconds)
    }

    pub const fn as_micros(self) -> u64 {
        self.0
    }

    pub const fn duration_since(self, earlier: Self) -> Option<u64> {
        self.0.checked_sub(earlier.0)
    }
}

impl fmt::Display for MonotonicTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}us", self.0)
    }
}

/// Encoding of the sample payload in a timed audio event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcmEncoding {
    Signed16LittleEndian,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampAccuracy {
    /// Timestamp came from a device or capture API sample-position clock.
    Hardware,
    /// Timestamp was inferred from monotonic delivery time and payload duration.
    Estimated,
}

/// Validated, platform-neutral PCM stream description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    sample_rate_hz: NonZeroU32,
    channels: NonZeroU32,
    encoding: PcmEncoding,
}

impl AudioFormat {
    pub fn pcm_s16le(sample_rate_hz: u32, channels: u32) -> Result<Self, AudioFormatError> {
        Ok(Self {
            sample_rate_hz: NonZeroU32::new(sample_rate_hz)
                .ok_or(AudioFormatError::ZeroSampleRate)?,
            channels: NonZeroU32::new(channels).ok_or(AudioFormatError::ZeroChannels)?,
            encoding: PcmEncoding::Signed16LittleEndian,
        })
    }

    pub const fn sample_rate_hz(self) -> NonZeroU32 {
        self.sample_rate_hz
    }

    pub const fn channels(self) -> NonZeroU32 {
        self.channels
    }

    pub const fn encoding(self) -> PcmEncoding {
        self.encoding
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum AudioFormatError {
    #[error("audio sample rate must be non-zero")]
    ZeroSampleRate,
    #[error("audio channel count must be non-zero")]
    ZeroChannels,
}

/// A decoded camera frame paired with the backend's observation time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedVideoFrame {
    pub observed_at: MonotonicTimestamp,
    pub frame: CompressedFrame,
    /// Set when the backend knows transport data was lost before this frame.
    pub source_discontinuity: bool,
}

/// A PCM block paired with the capture timestamp of its first sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedAudioChunk {
    pub observed_start: MonotonicTimestamp,
    pub timestamp_accuracy: TimestampAccuracy,
    pub format: AudioFormat,
    pub chunk: AudioChunk,
    /// Set for an overrun, device reset, or other backend-detected data loss.
    pub source_discontinuity: bool,
}

/// Type-erased media event accepted by a shared capture pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimedMediaEvent {
    Video(TimedVideoFrame),
    Audio(TimedAudioChunk),
}

/// Push boundary implemented by muxers, analyzers, or event queues.
///
/// Capture backends drive this trait using their natural execution model. A
/// native backend may call it from a polling loop; a WebUSB implementation may
/// call it after an asynchronous transfer without the core requiring `async`.
pub trait TimedMediaSink {
    type Error;

    fn push(&mut self, event: TimedMediaEvent) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_explicitly_relative_to_one_clock_domain() {
        let start = MonotonicTimestamp::from_micros(1_000);
        let event = MonotonicTimestamp::from_micros(1_750);
        assert_eq!(event.duration_since(start), Some(750));
        assert_eq!(start.duration_since(event), None);
    }

    #[test]
    fn audio_format_rejects_invalid_clock_and_channel_counts() {
        assert_eq!(
            AudioFormat::pcm_s16le(0, 2),
            Err(AudioFormatError::ZeroSampleRate)
        );
        assert_eq!(
            AudioFormat::pcm_s16le(16_000, 0),
            Err(AudioFormatError::ZeroChannels)
        );
    }

    #[test]
    fn backend_events_can_flow_through_a_platform_neutral_sink() {
        #[derive(Default)]
        struct Collector(Vec<TimedMediaEvent>);

        impl TimedMediaSink for Collector {
            type Error = std::convert::Infallible;

            fn push(&mut self, event: TimedMediaEvent) -> Result<(), Self::Error> {
                self.0.push(event);
                Ok(())
            }
        }

        let event = TimedMediaEvent::Audio(TimedAudioChunk {
            observed_start: MonotonicTimestamp::from_micros(4_000),
            timestamp_accuracy: TimestampAccuracy::Hardware,
            format: AudioFormat::pcm_s16le(48_000, 2).unwrap(),
            chunk: AudioChunk {
                pcm: vec![0; 192],
                start_sample_frame: 0,
                sample_frames: 48,
            },
            source_discontinuity: false,
        });
        let mut collector = Collector::default();
        collector.push(event.clone()).unwrap();
        assert_eq!(collector.0, vec![event]);
    }
}
