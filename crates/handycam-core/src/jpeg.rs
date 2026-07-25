use std::io::Cursor;

use hex_literal::hex;
use jpeg_decoder::{Decoder, PixelFormat};
use thiserror::Error;

use crate::{HEIGHT, WIDTH};

// Canonical JFIF header generated from the verified 320x240, baseline,
// quality-50, 4:2:0 format. It includes SOI through SOS, but no entropy or EOI.
const JPEG_HEADER: &[u8] = &hex!(
    "
    ffd8ffe000104a46494600010100000100010000ffdb004300100b0c0e0c0a10
    0e0d0e1211101318281a181616183123251d283a333d3c3933383740485c4e40
    4457453738506d51575f626768673e4d71797064785c656763ffdb0043011112
    121815182f1a1a2f634238426363636363636363636363636363636363636363
    636363636363636363636363636363636363636363636363636363636363ffc0
    00110800f0014003012200021101031101ffc4001f0000010501010101010100
    000000000000000102030405060708090a0bffc400b510000201030302040305
    0504040000017d01020300041105122131410613516107227114328191a10823
    42b1c11552d1f02433627282090a161718191a25262728292a3435363738393a
    434445464748494a535455565758595a636465666768696a737475767778797a
    838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7
    b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1
    f2f3f4f5f6f7f8f9faffc4001f01000301010101010101010100000000000001
    02030405060708090a0bffc400b5110002010204040304070504040001027700
    0102031104052131061241510761711322328108144291a1b1c109233352f015
    6272d10a162434e125f11718191a262728292a35363738393a43444546474849
    4a535455565758595a636465666768696a737475767778797a82838485868788
    898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4
    c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8f9
    faffda000c03010002110311003f00
    "
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Mjpeg,
    Yuyv,
}

#[derive(Debug, Error)]
pub enum FrameConversionError {
    #[error("Sony record contains no entropy payload")]
    EmptyEntropy,
    #[error("JPEG decode failed: {0}")]
    Decode(#[from] jpeg_decoder::Error),
    #[error("decoded JPEG has no image information")]
    MissingImageInfo,
    #[error("decoded JPEG dimensions are {actual:?}, expected {expected:?}")]
    Dimensions {
        actual: (u16, u16),
        expected: (u16, u16),
    },
    #[error("decoded JPEG pixel format is {0:?}, expected RGB24")]
    PixelFormat(PixelFormat),
}

/// Convert the camera's unescaped entropy scan into a complete baseline JPEG.
pub fn reconstruct_jpeg(entropy_with_padding: &[u8]) -> Result<Vec<u8>, FrameConversionError> {
    let entropy_end = entropy_with_padding
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |position| position + 1);
    let entropy = &entropy_with_padding[..entropy_end];
    if entropy.is_empty() {
        return Err(FrameConversionError::EmptyEntropy);
    }

    let stuffing = entropy.iter().filter(|byte| **byte == 0xff).count();
    let mut jpeg = Vec::with_capacity(JPEG_HEADER.len() + entropy.len() + stuffing + 2);
    jpeg.extend_from_slice(JPEG_HEADER);
    for byte in entropy {
        jpeg.push(*byte);
        if *byte == 0xff {
            jpeg.push(0x00);
        }
    }
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    Ok(jpeg)
}

/// Decode a reconstructed frame and convert it to packed, limited-range
/// BT.601 YUYV422.
pub fn jpeg_to_yuyv(jpeg: &[u8]) -> Result<Vec<u8>, FrameConversionError> {
    let mut decoder = Decoder::new(Cursor::new(jpeg));
    let rgb = decoder.decode()?;
    let info = decoder
        .info()
        .ok_or(FrameConversionError::MissingImageInfo)?;
    if (info.width, info.height) != (WIDTH as u16, HEIGHT as u16) {
        return Err(FrameConversionError::Dimensions {
            actual: (info.width, info.height),
            expected: (WIDTH as u16, HEIGHT as u16),
        });
    }
    if info.pixel_format != PixelFormat::RGB24 {
        return Err(FrameConversionError::PixelFormat(info.pixel_format));
    }

    let mut yuyv = Vec::with_capacity((WIDTH * HEIGHT * 2) as usize);
    for pair in rgb.chunks_exact(6) {
        let (y0, u0, v0) = rgb_to_yuv(pair[0], pair[1], pair[2]);
        let (y1, u1, v1) = rgb_to_yuv(pair[3], pair[4], pair[5]);
        yuyv.extend_from_slice(&[
            y0,
            (u0 as u16 + u1 as u16).div_ceil(2) as u8,
            y1,
            (v0 as u16 + v1 as u16).div_ceil(2) as u8,
        ]);
    }
    Ok(yuyv)
}

fn rgb_to_yuv(red: u8, green: u8, blue: u8) -> (u8, u8, u8) {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    let y = 16 + ((66 * red + 129 * green + 25 * blue + 128) >> 8);
    let u = 128 + ((-38 * red - 74 * green + 112 * blue + 128) >> 8);
    let v = 128 + ((112 * red - 94 * green - 18 * blue + 128) >> 8);
    (
        y.clamp(0, 255) as u8,
        u.clamp(0, 255) as u8,
        v.clamp(0, 255) as u8,
    )
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn record() -> Vec<u8> {
        hex::decode(
            include_str!("../tests/fixtures/record-0002.hex")
                .split_whitespace()
                .collect::<String>(),
        )
        .unwrap()
    }

    #[test]
    fn reconstruction_matches_python_oracle() {
        let record = record();
        let jpeg = reconstruct_jpeg(&record[8..]).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&jpeg)),
            "728f40b709530270e42b158d122a1b01cdd38630ebfc638ed4baf08ef1d4ec86"
        );
    }

    #[test]
    fn yuyv_has_expected_dimensions() {
        let record = record();
        let jpeg = reconstruct_jpeg(&record[8..]).unwrap();
        let yuyv = jpeg_to_yuyv(&jpeg).unwrap();
        assert_eq!(yuyv.len(), (WIDTH * HEIGHT * 2) as usize);
        assert_eq!(
            format!("{:x}", Sha256::digest(&yuyv)),
            "3decf60600836ae43511d5701d1ac8aedde46d1431a5853219bb735b69898101"
        );
    }
}
