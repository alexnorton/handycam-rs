//! Minimal EBML primitives: the variable-length integer encodings and
//! element-serialization helpers Matroska is built from. Only what
//! [`crate::MatroskaWriter`] needs to write is implemented; there is no
//! reader here.

/// The single-byte "unknown size" marker used for the streaming `Segment`
/// and `Cluster` elements, so no element ever needs to be rewritten once its
/// true length is known.
pub const UNKNOWN_SIZE: [u8; 1] = [0xFF];

/// Encodes `value` as an EBML variable-length "Element Data Size", choosing
/// the shortest length that can represent it (matches the encoding also used
/// for EBML element IDs and Matroska's (Simple)Block track-number field,
/// which share the same leading-one-bit length marker).
pub fn encode_vint(value: u64) -> Vec<u8> {
    let len = vint_length(value);
    let mut bytes = vec![0_u8; len as usize];
    let mut remaining = value;
    for byte in bytes.iter_mut().rev() {
        *byte = (remaining & 0xFF) as u8;
        remaining >>= 8;
    }
    bytes[0] |= 1 << (8 - len);
    bytes
}

fn vint_length(value: u64) -> u32 {
    for len in 1..=8 {
        // `len` data bytes hold `7 * len` usable bits; the all-ones pattern
        // in those bits is reserved to mean "unknown size", so only values
        // strictly below it fit.
        let max = (1_u64 << (7 * len)) - 2;
        if value <= max {
            return len;
        }
    }
    8
}

/// Encodes an unsigned integer using Matroska's minimal big-endian encoding
/// (at least one byte, no unnecessary leading zero bytes).
pub fn encode_uint(value: u64) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let mut bytes = value.to_be_bytes().to_vec();
    while bytes.len() > 1 && bytes[0] == 0 {
        bytes.remove(0);
    }
    bytes
}

/// Serializes one element: its raw ID bytes, an encoded size, then `data`.
pub fn element(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(id.len() + 9 + data.len());
    out.extend_from_slice(id);
    out.extend_from_slice(&encode_vint(data.len() as u64));
    out.extend_from_slice(data);
    out
}

/// Serializes a master element whose children are already-serialized byte
/// sequences, concatenated in order.
pub fn master(id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let data: Vec<u8> = children.concat();
    element(id, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_vint_sets_the_top_bit() {
        assert_eq!(encode_vint(0), vec![0x80]);
        assert_eq!(encode_vint(5), vec![0x85]);
        assert_eq!(encode_vint(126), vec![0xFE]);
    }

    #[test]
    fn value_requiring_a_second_byte_shifts_the_marker() {
        // 127 is unrepresentable in a 1-byte vint (0x7F is the reserved
        // "unknown length" pattern for that length), so it needs 2 bytes.
        assert_eq!(encode_vint(127), vec![0x40, 0x7F]);
        // 16382 is the largest value a 2-byte vint can hold (16383 would be
        // the all-ones "unknown length" pattern reserved at that length).
        assert_eq!(encode_vint(16_382), vec![0x7F, 0xFE]);
    }

    #[test]
    fn encode_uint_drops_leading_zero_bytes_but_keeps_at_least_one() {
        assert_eq!(encode_uint(0), vec![0]);
        assert_eq!(encode_uint(255), vec![0xFF]);
        assert_eq!(encode_uint(256), vec![0x01, 0x00]);
        assert_eq!(encode_uint(1_000_000), vec![0x0F, 0x42, 0x40]);
    }

    #[test]
    fn element_wraps_id_size_and_data() {
        assert_eq!(element(&[0x86], b"V_MJPEG"), {
            let mut expected = vec![0x86, 0x87];
            expected.extend_from_slice(b"V_MJPEG");
            expected
        });
    }
}
