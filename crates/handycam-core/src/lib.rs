//! Platform-neutral protocol core for Sony `054c:00c0` Handycam video.
//!
//! A transport must pass individual USB isochronous packets to
//! [`StreamDecoder::push_packet`] in completion order. In particular, endpoint
//! `0x82` packets must not be concatenated before header detection.

#![forbid(unsafe_code)]

mod audio;
mod control;
mod init;
mod jpeg;
mod media;
mod stream;
mod sync;

pub use audio::{
    AUDIO_BYTES_PER_SAMPLE, AUDIO_BYTES_PER_SAMPLE_FRAME, AUDIO_BYTES_PER_USB_FRAME,
    AUDIO_CHANNELS, AUDIO_ENDPOINT, AUDIO_SAMPLE_FRAMES_PER_USB_FRAME, AUDIO_SAMPLE_RATE_HZ,
    AudioChunk, AudioClock, AudioPacketError, audio_sample_frames_for_video_delta,
};
pub use control::{
    AUDIO_ON_REGISTERS, COMMAND_BLOCK_INDEX, COMMAND_BLOCK_SIZE, COMMAND_WORD_OFFSET,
    POWER_OFF_REGISTERS, POWER_ON_REGISTERS, POWER_ON_STAGE_DELAY_US, RegisterValue, RegisterWrite,
    SCALE_FACTOR_INDEX, ScaleFactor, ScaleFactorError, TransportCommand, TransportCommandEncoder,
    command_block, command_write, device_init_registers, scan_ack_token,
};
pub use init::{
    InitOp, InitPlanError, override_record_mode_scale_factor, parse_init_plan,
    record_mode_init_plan,
};
pub use jpeg::{FrameConversionError, OutputFormat, jpeg_to_yuyv, reconstruct_jpeg};
pub use media::{
    AudioFormat, AudioFormatError, MonotonicTimestamp, PcmEncoding, TimedAudioChunk,
    TimedMediaEvent, TimedMediaSink, TimedVideoFrame, TimestampAccuracy,
};
pub use stream::{CompressedFrame, Endpoint, EndpointPacket, StreamDecodeError, StreamDecoder};
pub use sync::{AvSynchronizer, MediaTiming, SynchronizationError};

/// Sony's USB vendor ID.
pub const VENDOR_ID: u16 = 0x054c;
/// Product ID verified with the DCR-HC24.
pub const PRODUCT_ID: u16 = 0x00c0;
/// Vendor video/control interface.
pub const VIDEO_INTERFACE: u8 = 0;
/// Verified streaming alternate setting.
pub const VIDEO_ALT: u8 = 5;
/// Camera frame dimensions.
pub const WIDTH: u32 = 320;
/// Camera frame dimensions.
pub const HEIGHT: u32 = 240;
/// Verified frame rate.
pub const FPS: u32 = 25;
