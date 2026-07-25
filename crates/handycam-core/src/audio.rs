use thiserror::Error;

/// Standard USB Audio endpoint exposed by the DCR-HC24.
pub const AUDIO_ENDPOINT: u8 = 0x83;
pub const AUDIO_CHANNELS: u8 = 2;
pub const AUDIO_SAMPLE_RATE_HZ: u32 = 16_000;
pub const AUDIO_BYTES_PER_SAMPLE: usize = 2;
pub const AUDIO_BYTES_PER_SAMPLE_FRAME: usize = AUDIO_CHANNELS as usize * AUDIO_BYTES_PER_SAMPLE;
pub const AUDIO_SAMPLE_FRAMES_PER_USB_FRAME: usize = 16;
pub const AUDIO_BYTES_PER_USB_FRAME: usize =
    AUDIO_SAMPLE_FRAMES_PER_USB_FRAME * AUDIO_BYTES_PER_SAMPLE_FRAME;

/// A PCM chunk positioned on the monotonically counted device-audio clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioChunk {
    /// Interleaved stereo signed PCM16LE.
    pub pcm: Vec<u8>,
    /// Sample-frame position of the first stereo sample pair.
    pub start_sample_frame: u64,
    pub sample_frames: u32,
}

impl AudioChunk {
    pub fn start_timestamp_us(&self) -> u64 {
        self.start_sample_frame * 1_000_000 / u64::from(AUDIO_SAMPLE_RATE_HZ)
    }
}

/// Count complete endpoint packets into a stable audio timeline.
///
/// USB frame numbers should be retained by the platform transport as the
/// preferred A/V correlation signal. This counter is the portable fallback:
/// at 16 kHz, one millisecond is exactly 16 stereo sample frames.
#[derive(Clone, Debug, Default)]
pub struct AudioClock {
    next_sample_frame: u64,
}

impl AudioClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.next_sample_frame = 0;
    }

    pub fn next_sample_frame(&self) -> u64 {
        self.next_sample_frame
    }

    pub fn push_pcm(&mut self, pcm: &[u8]) -> Result<AudioChunk, AudioPacketError> {
        if pcm.len() % AUDIO_BYTES_PER_SAMPLE_FRAME != 0 {
            return Err(AudioPacketError::MisalignedLength(pcm.len()));
        }
        let sample_frames = pcm.len() / AUDIO_BYTES_PER_SAMPLE_FRAME;
        let sample_frames =
            u32::try_from(sample_frames).map_err(|_| AudioPacketError::TooLarge(pcm.len()))?;
        let chunk = AudioChunk {
            pcm: pcm.to_vec(),
            start_sample_frame: self.next_sample_frame,
            sample_frames,
        };
        self.next_sample_frame += u64::from(sample_frames);
        Ok(chunk)
    }
}

/// Expected audio span corresponding to a Sony video timestamp interval.
pub const fn audio_sample_frames_for_video_delta(delta_ms: u16) -> u32 {
    delta_ms as u32 * (AUDIO_SAMPLE_RATE_HZ / 1_000)
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum AudioPacketError {
    #[error("PCM byte length {0} is not aligned to one stereo PCM16 sample frame")]
    MisalignedLength(usize),
    #[error("PCM byte length {0} cannot be represented as one audio chunk")]
    TooLarge(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_usb_frame_is_exactly_one_millisecond() {
        let mut clock = AudioClock::new();
        let first = clock.push_pcm(&[0; AUDIO_BYTES_PER_USB_FRAME]).unwrap();
        let second = clock.push_pcm(&[0; AUDIO_BYTES_PER_USB_FRAME]).unwrap();
        assert_eq!(first.start_sample_frame, 0);
        assert_eq!(first.sample_frames, 16);
        assert_eq!(second.start_sample_frame, 16);
        assert_eq!(second.start_timestamp_us(), 1_000);
    }

    #[test]
    fn one_pal_video_frame_corresponds_to_640_audio_frames() {
        assert_eq!(audio_sample_frames_for_video_delta(40), 640);
    }

    #[test]
    fn rejects_partial_stereo_sample_frames() {
        assert_eq!(
            AudioClock::new().push_pcm(&[0; 3]).unwrap_err(),
            AudioPacketError::MisalignedLength(3)
        );
    }
}
