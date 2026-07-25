use std::fmt;

use thiserror::Error;

use crate::{SCALE_FACTOR_INDEX, ScaleFactor};

const RECORD_MODE_PLAN: &str = include_str!("../assets/record-mode-init.tsv");

/// One replayable operation in Sony's record-mode initialization sequence.
#[derive(Clone, PartialEq, Eq)]
pub enum InitOp {
    ControlIn {
        delay_us: u64,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    },
    ControlOut {
        delay_us: u64,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
    },
    SetInterface {
        delay_us: u64,
        interface: u8,
        alternate: u8,
    },
}

impl InitOp {
    pub fn delay_us(&self) -> u64 {
        match self {
            Self::ControlIn { delay_us, .. }
            | Self::ControlOut { delay_us, .. }
            | Self::SetInterface { delay_us, .. } => *delay_us,
        }
    }
}

impl fmt::Debug for InitOp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlIn {
                delay_us,
                request_type,
                request,
                value,
                index,
                length,
            } => formatter
                .debug_struct("ControlIn")
                .field("delay_us", delay_us)
                .field("request_type", &format_args!("{request_type:#04x}"))
                .field("request", &format_args!("{request:#04x}"))
                .field("value", &format_args!("{value:#06x}"))
                .field("index", &format_args!("{index:#06x}"))
                .field("length", length)
                .finish(),
            Self::ControlOut {
                delay_us,
                request_type,
                request,
                value,
                index,
                data,
            } => formatter
                .debug_struct("ControlOut")
                .field("delay_us", delay_us)
                .field("request_type", &format_args!("{request_type:#04x}"))
                .field("request", &format_args!("{request:#04x}"))
                .field("value", &format_args!("{value:#06x}"))
                .field("index", &format_args!("{index:#06x}"))
                .field("length", &data.len())
                .finish(),
            Self::SetInterface {
                delay_us,
                interface,
                alternate,
            } => formatter
                .debug_struct("SetInterface")
                .field("delay_us", delay_us)
                .field("interface", interface)
                .field("alternate", alternate)
                .finish(),
        }
    }
}

#[derive(Debug, Error)]
pub enum InitPlanError {
    #[error("line {line}: expected 8 tab-separated fields, got {count}")]
    FieldCount { line: usize, count: usize },
    #[error("line {line}: invalid {field}: {value}")]
    InvalidNumber {
        line: usize,
        field: &'static str,
        value: String,
    },
    #[error("line {line}: invalid hexadecimal payload: {source}")]
    InvalidHex {
        line: usize,
        #[source]
        source: hex::FromHexError,
    },
    #[error("line {line}: payload length {actual} does not match wLength {expected}")]
    PayloadLength {
        line: usize,
        expected: usize,
        actual: usize,
    },
    #[error("line {line}: unsupported request type={request_type:#04x} request={request:#04x}")]
    Unsupported {
        line: usize,
        request_type: u8,
        request: u8,
    },
}

fn parse_u64(line: usize, field: &'static str, value: &str) -> Result<u64, InitPlanError> {
    let parsed = if let Some(value) = value.strip_prefix("0x") {
        u64::from_str_radix(value, 16)
    } else {
        value.parse()
    };
    parsed.map_err(|_| InitPlanError::InvalidNumber {
        line,
        field,
        value: value.to_owned(),
    })
}

fn parse_bounded(
    line: usize,
    field: &'static str,
    value: &str,
    maximum: u64,
) -> Result<u64, InitPlanError> {
    let parsed = parse_u64(line, field, value)?;
    if parsed > maximum {
        return Err(InitPlanError::InvalidNumber {
            line,
            field,
            value: value.to_owned(),
        });
    }
    Ok(parsed)
}

/// Parse a TSV initialization plan using the deliberately narrow supported
/// operation set.
pub fn parse_init_plan(text: &str) -> Result<Vec<InitOp>, InitPlanError> {
    let mut operations = Vec::new();
    for (offset, row) in text.lines().enumerate() {
        let line = offset + 1;
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = row.split('\t').collect();
        if fields.len() != 8 {
            return Err(InitPlanError::FieldCount {
                line,
                count: fields.len(),
            });
        }
        let delay_us = parse_bounded(line, "delay_us", fields[0], 10_000_000)?;
        let request_type = parse_bounded(line, "bmRequestType", fields[1], u8::MAX.into())? as u8;
        let request = parse_bounded(line, "bRequest", fields[2], u8::MAX.into())? as u8;
        let value = parse_bounded(line, "wValue", fields[3], u16::MAX.into())? as u16;
        let index = parse_bounded(line, "wIndex", fields[4], u16::MAX.into())? as u16;
        let length = parse_bounded(line, "wLength", fields[5], 64)? as usize;

        let operation = match (request_type, request) {
            (0x01, 0x0b) if index == 0 && length == 0 => InitOp::SetInterface {
                delay_us,
                interface: index as u8,
                alternate: value as u8,
            },
            (0xc0, 0x88) => InitOp::ControlIn {
                delay_us,
                request_type,
                request,
                value,
                index,
                length: length as u16,
            },
            (0x40, 0x88) => {
                let data = hex::decode(fields[6])
                    .map_err(|source| InitPlanError::InvalidHex { line, source })?;
                if data.len() != length {
                    return Err(InitPlanError::PayloadLength {
                        line,
                        expected: length,
                        actual: data.len(),
                    });
                }
                InitOp::ControlOut {
                    delay_us,
                    request_type,
                    request,
                    value,
                    index,
                    data,
                }
            }
            _ => {
                return Err(InitPlanError::Unsupported {
                    line,
                    request_type,
                    request,
                });
            }
        };
        operations.push(operation);
    }
    Ok(operations)
}

/// Return the verified DCR-HC24 live record-mode plan embedded in the crate.
pub fn record_mode_init_plan() -> Result<Vec<InitOp>, InitPlanError> {
    parse_init_plan(RECORD_MODE_PLAN)
}

/// Override the configured scale-factor write in a parsed record-mode plan.
///
/// The plan first writes the device-reset value `0x10`, then later writes the
/// configured value. Searching backwards changes only that configured write.
pub fn override_record_mode_scale_factor(
    operations: &mut [InitOp],
    scale_factor: ScaleFactor,
) -> bool {
    for operation in operations.iter_mut().rev() {
        if let InitOp::ControlOut { index, data, .. } = operation
            && *index == SCALE_FACTOR_INDEX
            && data.len() == 1
        {
            data[0] = scale_factor.get();
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_plan_matches_recorded_statistics() {
        let operations = record_mode_init_plan().unwrap();
        assert_eq!(operations.len(), 223);
        assert_eq!(
            operations.iter().map(InitOp::delay_us).sum::<u64>(),
            2_563_101
        );

        let mut reads = 0;
        let mut writes = 0;
        let mut alternates = Vec::new();
        for operation in &operations {
            match operation {
                InitOp::ControlIn { .. } => reads += 1,
                InitOp::ControlOut { .. } => writes += 1,
                InitOp::SetInterface { alternate, .. } => alternates.push(*alternate),
            }
        }
        assert_eq!((writes, reads), (140, 80));
        assert_eq!(alternates, [0, 7, 5]);
    }

    #[test]
    fn quality_override_preserves_reset_and_changes_configured_write() {
        let mut operations = record_mode_init_plan().unwrap();
        assert!(override_record_mode_scale_factor(
            &mut operations,
            ScaleFactor::new(80).unwrap()
        ));
        let values = operations
            .iter()
            .filter_map(|operation| match operation {
                InitOp::ControlOut { index, data, .. }
                    if *index == SCALE_FACTOR_INDEX && data.len() == 1 =>
                {
                    Some(data[0])
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(values, [0x10, 0x50]);
    }
}
