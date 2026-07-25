//! Matroska/EBML element IDs, stored as their raw big-endian byte sequences
//! (the ID's own length-marker bits are part of the constant, unlike a size
//! vint). Names and values match the Matroska specification.

pub const EBML: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
pub const EBML_VERSION: [u8; 2] = [0x42, 0x86];
pub const EBML_READ_VERSION: [u8; 2] = [0x42, 0xF7];
pub const EBML_MAX_ID_LENGTH: [u8; 2] = [0x42, 0xF2];
pub const EBML_MAX_SIZE_LENGTH: [u8; 2] = [0x42, 0xF3];
pub const DOC_TYPE: [u8; 2] = [0x42, 0x82];
pub const DOC_TYPE_VERSION: [u8; 2] = [0x42, 0x87];
pub const DOC_TYPE_READ_VERSION: [u8; 2] = [0x42, 0x85];

pub const SEGMENT: [u8; 4] = [0x18, 0x53, 0x80, 0x67];

pub const SEGMENT_INFO: [u8; 4] = [0x15, 0x49, 0xA9, 0x66];
pub const TIMESTAMP_SCALE: [u8; 3] = [0x2A, 0xD7, 0xB1];
pub const MUXING_APP: [u8; 2] = [0x4D, 0x80];
pub const WRITING_APP: [u8; 2] = [0x57, 0x41];

pub const TRACKS: [u8; 4] = [0x16, 0x54, 0xAE, 0x6B];
pub const TRACK_ENTRY: [u8; 1] = [0xAE];
pub const TRACK_NUMBER: [u8; 1] = [0xD7];
pub const TRACK_UID: [u8; 2] = [0x73, 0xC5];
pub const TRACK_TYPE: [u8; 1] = [0x83];
pub const CODEC_ID: [u8; 1] = [0x86];
pub const VIDEO: [u8; 1] = [0xE0];
pub const PIXEL_WIDTH: [u8; 1] = [0xB0];
pub const PIXEL_HEIGHT: [u8; 1] = [0xBA];
pub const AUDIO: [u8; 1] = [0xE1];
pub const SAMPLING_FREQUENCY: [u8; 1] = [0xB5];
pub const CHANNELS: [u8; 1] = [0x9F];
pub const BIT_DEPTH: [u8; 2] = [0x62, 0x64];

pub const CLUSTER: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
pub const TIMESTAMP: [u8; 1] = [0xE7];
pub const SIMPLE_BLOCK: [u8; 1] = [0xA3];
