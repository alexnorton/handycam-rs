use thiserror::Error;

use crate::{FrameConversionError, reconstruct_jpeg};

const DEFAULT_MAX_RECORD_SIZE: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Boundary,
    Video,
}

#[derive(Clone, Copy, Debug)]
pub struct EndpointPacket<'a> {
    pub endpoint: Endpoint,
    pub data: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressedFrame {
    pub sequence: u64,
    pub timestamp_11bit_ms: u16,
    pub timestamp_unwrapped_ms: u64,
    pub timestamp_delta_to_next_ms: u16,
    pub flags: u8,
    pub jpeg: Vec<u8>,
}

struct PendingRecord {
    timestamp: u16,
    unwrapped_timestamp: u64,
    flags: u8,
    entropy: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum StreamDecodeError {
    #[error("Sony record exceeded configured maximum of {maximum} bytes")]
    RecordTooLarge { maximum: usize },
    #[error(transparent)]
    FrameConversion(#[from] FrameConversionError),
}

/// Stateful cross-endpoint Sony frame assembler.
pub struct StreamDecoder {
    boundary_pending: bool,
    current: Option<PendingRecord>,
    discard_first_record: bool,
    next_sequence: u64,
    max_record_size: usize,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            boundary_pending: false,
            current: None,
            discard_first_record: true,
            next_sequence: 0,
            max_record_size: DEFAULT_MAX_RECORD_SIZE,
        }
    }

    pub fn with_max_record_size(maximum: usize) -> Self {
        Self {
            max_record_size: maximum,
            ..Self::new()
        }
    }

    pub fn reset(&mut self) {
        self.boundary_pending = false;
        self.current = None;
        self.discard_first_record = true;
        self.next_sequence = 0;
    }

    pub fn push_packet(
        &mut self,
        packet: EndpointPacket<'_>,
    ) -> Result<Option<CompressedFrame>, StreamDecodeError> {
        match packet.endpoint {
            Endpoint::Boundary => {
                if packet.data.first().is_some_and(|byte| byte & 0x08 != 0) {
                    self.boundary_pending = true;
                }
                Ok(None)
            }
            Endpoint::Video => self.push_video(packet.data),
        }
    }

    fn push_video(&mut self, data: &[u8]) -> Result<Option<CompressedFrame>, StreamDecodeError> {
        let is_header = data.len() >= 8 && data[..6] == [0xff; 6];
        if is_header && self.boundary_pending {
            self.boundary_pending = false;
            let timestamp = (u16::from(data[6] & 0x07) << 8) | u16::from(data[7]);
            let flags = data[6] & 0xf8;
            let finished = self.current.take();

            let next_unwrapped = finished.as_ref().map_or(0, |record| {
                record.unwrapped_timestamp
                    + u64::from(timestamp.wrapping_sub(record.timestamp) & 0x07ff)
            });
            self.current = Some(PendingRecord {
                timestamp,
                unwrapped_timestamp: if self.discard_first_record {
                    0
                } else {
                    next_unwrapped
                },
                flags,
                entropy: data[8..].to_vec(),
            });

            if let Some(record) = finished {
                if self.discard_first_record {
                    self.discard_first_record = false;
                    if let Some(current) = self.current.as_mut() {
                        current.unwrapped_timestamp = 0;
                    }
                    return Ok(None);
                }
                let delta = timestamp.wrapping_sub(record.timestamp) & 0x07ff;
                let jpeg = reconstruct_jpeg(&record.entropy)?;
                let frame = CompressedFrame {
                    sequence: self.next_sequence,
                    timestamp_11bit_ms: record.timestamp,
                    timestamp_unwrapped_ms: record.unwrapped_timestamp,
                    timestamp_delta_to_next_ms: delta,
                    flags: record.flags,
                    jpeg,
                };
                self.next_sequence += 1;
                return Ok(Some(frame));
            }
            return Ok(None);
        }

        if let Some(record) = self.current.as_mut() {
            if record.entropy.len() + data.len() > self.max_record_size {
                self.current = None;
                self.boundary_pending = false;
                self.discard_first_record = true;
                return Err(StreamDecodeError::RecordTooLarge {
                    maximum: self.max_record_size,
                });
            }
            record.entropy.extend_from_slice(data);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::Value;
    use sha2::{Digest, Sha256};

    use super::*;

    fn boundary() -> EndpointPacket<'static> {
        EndpointPacket {
            endpoint: Endpoint::Boundary,
            data: &[0x08, 0, 0, 0, 0],
        }
    }

    fn header(timestamp: u16) -> Vec<u8> {
        vec![
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            ((timestamp >> 8) as u8) & 0x07,
            timestamp as u8,
        ]
    }

    #[test]
    fn header_requires_a_boundary_and_packet_start() {
        let mut decoder = StreamDecoder::new();
        assert!(
            decoder
                .push_packet(EndpointPacket {
                    endpoint: Endpoint::Video,
                    data: &header(100),
                })
                .unwrap()
                .is_none()
        );
        decoder.push_packet(boundary()).unwrap();
        let prefixed = [&[0_u8][..], &header(100)].concat();
        decoder
            .push_packet(EndpointPacket {
                endpoint: Endpoint::Video,
                data: &prefixed,
            })
            .unwrap();
        decoder.push_packet(boundary()).unwrap();
        decoder
            .push_packet(EndpointPacket {
                endpoint: Endpoint::Video,
                data: &header(100),
            })
            .unwrap();
        assert!(decoder.current.is_some());
    }

    #[test]
    fn timestamp_wrap_uses_2048_modulus() {
        assert_eq!(20_u16.wrapping_sub(2028) & 0x07ff, 40);
    }

    #[test]
    fn oversized_record_resets_decoder() {
        let mut decoder = StreamDecoder::with_max_record_size(4);
        decoder.push_packet(boundary()).unwrap();
        decoder
            .push_packet(EndpointPacket {
                endpoint: Endpoint::Video,
                data: &header(0),
            })
            .unwrap();
        let error = decoder
            .push_packet(EndpointPacket {
                endpoint: Endpoint::Video,
                data: &[1, 2, 3, 4, 5],
            })
            .unwrap_err();
        assert!(matches!(
            error,
            StreamDecodeError::RecordTooLarge { maximum: 4 }
        ));
        assert!(decoder.current.is_none());
    }

    #[test]
    fn curated_camera_record_passes_through_framing_state_machine() {
        let mut record = hex::decode(
            include_str!("../tests/fixtures/record-0002.hex")
                .split_whitespace()
                .collect::<String>(),
        )
        .unwrap();
        let mut decoder = StreamDecoder::new();
        let mut frames = Vec::new();

        for timestamp in [100_u16, 140, 180] {
            record[6] = ((timestamp >> 8) as u8) & 0x07;
            record[7] = timestamp as u8;
            decoder.push_packet(boundary()).unwrap();
            for (index, chunk) in record.chunks(768).enumerate() {
                if let Some(frame) = decoder
                    .push_packet(EndpointPacket {
                        endpoint: Endpoint::Video,
                        data: chunk,
                    })
                    .unwrap()
                {
                    frames.push(frame);
                }
                assert!(index != 0 || chunk.starts_with(&[0xff; 6]));
            }
        }

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp_11bit_ms, 140);
        assert_eq!(frames[0].timestamp_delta_to_next_ms, 40);
        assert_eq!(
            format!("{:x}", Sha256::digest(&frames[0].jpeg)),
            "728f40b709530270e42b158d122a1b01cdd38630ebfc638ed4baf08ef1d4ec86"
        );
    }

    #[test]
    fn replays_complete_direct_linux_capture() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../reverse-engineering/captures/live-linux-init-first");
        if !root.exists() {
            // The complete research corpus is intentionally ignored; the
            // curated camera-record test above is always available.
            return;
        }
        let ep81 = fs::read(root.join("ep81.bin")).unwrap();
        let ep82 = fs::read(root.join("ep82.bin")).unwrap();
        let metadata = fs::read_to_string(root.join("iso-packets.jsonl")).unwrap();
        let mut decoder = StreamDecoder::new();
        let mut frames = Vec::new();

        for line in metadata.lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let length = row["length"].as_u64().unwrap() as usize;
            if length == 0 {
                continue;
            }
            let offset = row["data_offset"].as_u64().unwrap() as usize;
            let (endpoint, source) = match row["endpoint"].as_str().unwrap() {
                "0x81" => (Endpoint::Boundary, &ep81),
                "0x82" => (Endpoint::Video, &ep82),
                other => panic!("unexpected endpoint {other}"),
            };
            if let Some(frame) = decoder
                .push_packet(EndpointPacket {
                    endpoint,
                    data: &source[offset..offset + length],
                })
                .unwrap()
            {
                frames.push(frame);
            }
        }

        assert_eq!(frames.len(), 123);
        assert!(
            frames
                .iter()
                .all(|frame| frame.timestamp_delta_to_next_ms == 40)
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&frames[0].jpeg)),
            "728f40b709530270e42b158d122a1b01cdd38630ebfc638ed4baf08ef1d4ec86"
        );
    }
}
