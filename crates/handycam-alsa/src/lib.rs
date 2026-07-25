//! Timestamped ALSA capture for the Sony Handycam's standard USB Audio Class
//! interface, feeding `handycam_core::AudioClock` with either a real
//! hardware capture timestamp or a monotonic read-time fallback.
//!
//! This crate is intentionally Linux-only (mirrors `handycam-libusb`'s
//! choice of `rusb`): `cpal` was considered but deliberately hides
//! platform-specific features like ALSA's hardware timestamps and explicit
//! xrun recovery to stay cross-platform, so a direct binding is the right
//! layer here.

#![forbid(unsafe_code)]

mod clock;

use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM, TstampType};
use alsa::{Direction, Error as AlsaError, ValueOr};
use handycam_core::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ, AudioChunk, AudioClock, AudioPacketError,
};
use thiserror::Error;

/// Requested ALSA period length. The device already delivers audio in 1ms
/// USB-frame-sized pieces internally; this just controls how many of those
/// this backend batches into one read.
const PERIOD_TIME_US: u32 = 20_000;

/// Whether an audio chunk's timestamp came from an ALSA hardware capture
/// timestamp or a monotonic host read-time estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampQuality {
    Hardware,
    MonotonicFallback,
}

/// One event produced by [`AlsaCapture::read`].
#[derive(Debug)]
pub enum AudioCaptureEvent {
    Chunk {
        chunk: AudioChunk,
        captured_at: Instant,
        quality: TimestampQuality,
    },
    /// An ALSA buffer overrun (xrun) was detected and recovered from.
    /// `frames_lost` is a wall-clock estimate, not an exact hardware count.
    Overrun { frames_lost: u32 },
}

#[derive(Debug, Error)]
pub enum AlsaCaptureError {
    #[error("ALSA error: {0}")]
    Alsa(#[from] AlsaError),
    #[error(transparent)]
    Audio(#[from] AudioPacketError),
}

/// Ties one ALSA hardware timestamp reading to the `Instant` observed at
/// (approximately) the same moment, so later hardware timestamps -- which
/// arrive as ALSA's own raw monotonic-clock reading, not a `std::time::Instant`
/// -- can be expressed as an `Instant` by applying the same delta to the
/// calibration point. This needs no unsafe code and no assumption about
/// `Instant`'s internal representation: it only ever adds a `Duration`
/// computed from two ALSA readings onto an `Instant` known to be
/// contemporaneous with the first one.
struct ClockCalibration {
    instant: Instant,
    alsa_monotonic: Duration,
}

pub struct AlsaCapture {
    pcm: PCM,
    clock: AudioClock,
    period_frames: usize,
    hardware_timestamps_enabled: bool,
    calibration: Option<ClockCalibration>,
    last_frame_boundary_at: Instant,
}

impl AlsaCapture {
    /// Opens `device` (for example `hw:1,0`) for blocking stereo PCM16LE
    /// capture at [`AUDIO_SAMPLE_RATE_HZ`].
    pub fn open(device: &str) -> Result<Self, AlsaCaptureError> {
        let pcm = PCM::new(device, Direction::Capture, false)?;
        let period_frames = {
            let hw_params = HwParams::any(&pcm)?;
            hw_params.set_access(Access::RWInterleaved)?;
            hw_params.set_format(Format::s16())?;
            hw_params.set_channels(u32::from(AUDIO_CHANNELS))?;
            hw_params.set_rate(AUDIO_SAMPLE_RATE_HZ, ValueOr::Nearest)?;
            hw_params.set_period_time_near(PERIOD_TIME_US, ValueOr::Nearest)?;
            pcm.hw_params(&hw_params)?;
            hw_params.get_period_size().unwrap_or(0)
        };

        let hardware_timestamps_enabled = {
            let sw_params = pcm.sw_params_current()?;
            let enabled = sw_params.set_tstamp_mode(true).is_ok()
                && sw_params.set_tstamp_type(TstampType::Monotonic).is_ok();
            if enabled {
                pcm.sw_params(&sw_params)?;
            }
            enabled
        };

        pcm.prepare()?;
        let now = Instant::now();
        Ok(Self {
            pcm,
            clock: AudioClock::new(),
            period_frames: (period_frames.max(1)) as usize,
            hardware_timestamps_enabled,
            calibration: None,
            last_frame_boundary_at: now,
        })
    }

    /// Waits up to `timeout` for the device to have a full period of audio
    /// ready, then reads it. Returns `Ok(None)` on timeout so the caller's
    /// polling loop can check its own shutdown flag between reads instead of
    /// blocking on this call indefinitely -- there is no safe cross-thread
    /// way to interrupt a blocked ALSA read directly.
    pub fn read(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<AudioCaptureEvent>, AlsaCaptureError> {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        if !self.pcm.wait(Some(timeout_ms))? {
            return Ok(None);
        }

        let mut buffer = vec![0_i16; self.period_frames * usize::from(AUDIO_CHANNELS)];
        match self
            .pcm
            .io_checked::<i16>()
            .and_then(|io| io.readi(&mut buffer))
        {
            Ok(frames_read) => {
                buffer.truncate(frames_read * usize::from(AUDIO_CHANNELS));
                let pcm_bytes: Vec<u8> = buffer
                    .iter()
                    .flat_map(|sample| sample.to_le_bytes())
                    .collect();
                let (captured_at, quality) = self.capture_timestamp();
                self.last_frame_boundary_at = captured_at;
                let chunk = self.clock.push_pcm(&pcm_bytes)?;
                Ok(Some(AudioCaptureEvent::Chunk {
                    chunk,
                    captured_at,
                    quality,
                }))
            }
            Err(error) => self.recover_from_read_error(error).map(Some),
        }
    }

    fn capture_timestamp(&mut self) -> (Instant, TimestampQuality) {
        if self.hardware_timestamps_enabled {
            if let Ok(status) = self.pcm.status() {
                if let Some(sample) = clock::timespec_duration(status.get_htstamp()) {
                    let calibration = self.calibration.get_or_insert_with(|| ClockCalibration {
                        instant: Instant::now(),
                        alsa_monotonic: sample,
                    });
                    if let Some(delta) = sample.checked_sub(calibration.alsa_monotonic) {
                        return (calibration.instant + delta, TimestampQuality::Hardware);
                    }
                }
            }
        }
        (Instant::now(), TimestampQuality::MonotonicFallback)
    }

    fn recover_from_read_error(
        &mut self,
        error: AlsaError,
    ) -> Result<AudioCaptureEvent, AlsaCaptureError> {
        if error.errno() != libc::EPIPE {
            return Err(AlsaCaptureError::Alsa(error));
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_frame_boundary_at);
        let frames_lost = clock::estimate_frames_lost(elapsed, AUDIO_SAMPLE_RATE_HZ);
        self.pcm.recover(error.errno(), true)?;
        self.clock
            .resync(self.clock.next_sample_frame() + u64::from(frames_lost));
        self.last_frame_boundary_at = now;
        Ok(AudioCaptureEvent::Overrun { frames_lost })
    }
}
