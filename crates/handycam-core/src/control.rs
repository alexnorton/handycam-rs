use thiserror::Error;

/// Index used by Sony's 64-byte camera command mailbox.
pub const COMMAND_BLOCK_INDEX: u16 = 0x0300;
/// Size of the camera command mailbox.
pub const COMMAND_BLOCK_SIZE: usize = 64;
/// Byte offset of the big-endian command word in the mailbox.
pub const COMMAND_WORD_OFFSET: usize = 60;

/// Register used by the DCR-HC24's simple JPEG scale-factor path.
pub const SCALE_FACTOR_INDEX: u16 = 0x007c;

/// One-byte Sony register write used by semantic device-manager sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterValue {
    pub index: u16,
    pub value: u8,
}

/// A vendor request `0x88` register or buffer write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterWrite {
    pub index: u16,
    pub data: Vec<u8>,
}

impl From<RegisterValue> for RegisterWrite {
    fn from(register: RegisterValue) -> Self {
        Self {
            index: register.index,
            data: vec![register.value],
        }
    }
}

pub const POWER_OFF_REGISTERS: [RegisterValue; 2] = [
    RegisterValue {
        index: 0x35,
        value: 0x06,
    },
    RegisterValue {
        index: 0x35,
        value: 0x00,
    },
];

pub const AUDIO_ON_REGISTERS: [RegisterValue; 4] = [
    RegisterValue {
        index: 0x23,
        value: 0x00,
    },
    RegisterValue {
        index: 0x24,
        value: 0x68,
    },
    RegisterValue {
        index: 0x22,
        value: 0x0c,
    },
    RegisterValue {
        index: 0x35,
        value: 0x0f,
    },
];

pub const POWER_ON_REGISTERS: [RegisterValue; 4] = [
    RegisterValue {
        index: 0x35,
        value: 0x06,
    },
    RegisterValue {
        index: 0x36,
        value: 0x01,
    },
    RegisterValue {
        index: 0x36,
        value: 0x03,
    },
    RegisterValue {
        index: 0x35,
        value: 0x0f,
    },
];

/// Required delay between `POWER_ON_REGISTERS[1]` and `[2]`.
pub const POWER_ON_STAGE_DELAY_US: u64 = 10_000;

const DEVICE_INIT_TEMPLATE: [RegisterValue; 21] = [
    RegisterValue {
        index: 0x00,
        value: 0x00,
    },
    RegisterValue {
        index: 0x50,
        value: 0xc0,
    },
    RegisterValue {
        index: 0x55,
        value: 0x80,
    },
    RegisterValue {
        index: 0x61,
        value: 0x20,
    },
    RegisterValue {
        index: 0x7a,
        value: 0x0f,
    },
    RegisterValue {
        index: 0x06,
        value: 0x80,
    },
    RegisterValue {
        index: 0x07,
        value: 0x02,
    },
    RegisterValue {
        index: 0x0a,
        value: 0xdf,
    },
    RegisterValue {
        index: 0x0f,
        value: 0x00,
    },
    RegisterValue {
        index: 0x20,
        value: 0x41,
    },
    RegisterValue {
        index: 0x21,
        value: 0x60,
    },
    RegisterValue {
        index: 0x53,
        value: 0x80,
    },
    RegisterValue {
        index: 0x54,
        value: 0x02,
    },
    RegisterValue {
        index: 0x5e,
        value: 0x03,
    },
    RegisterValue {
        index: 0x78,
        value: 0x60,
    },
    RegisterValue {
        index: 0x79,
        value: 0x09,
    },
    RegisterValue {
        index: 0x7c,
        value: 0x10,
    },
    RegisterValue {
        index: 0x37,
        value: 0x00,
    },
    RegisterValue {
        index: 0x51,
        value: 0x00,
    },
    RegisterValue {
        index: 0x76,
        value: 0x00,
    },
    RegisterValue {
        index: 0x23,
        value: 0x00,
    },
];

/// Generate the 21-register `DevmanInit` phase observed on the DCR-HC24.
///
/// Two values are populated from device state by Sony's driver. The known
/// record-mode trace uses `mode_61=0x3f` and `init_complete=0x03`.
pub fn device_init_registers(
    mode_61: u8,
    init_complete: u8,
) -> [RegisterValue; DEVICE_INIT_TEMPLATE.len()] {
    let mut registers = DEVICE_INIT_TEMPLATE;
    registers[3].value = mode_61;
    registers[18].value = init_complete;
    registers
}

/// Construct the command mailbox written by Sony's `CustomPropCommandWrite`.
pub fn command_block(command: u32) -> [u8; COMMAND_BLOCK_SIZE] {
    let mut block = [0_u8; COMMAND_BLOCK_SIZE];
    block[COMMAND_WORD_OFFSET..].copy_from_slice(&command.to_be_bytes());
    block
}

pub fn command_write(command: u32) -> RegisterWrite {
    RegisterWrite {
        index: COMMAND_BLOCK_INDEX,
        data: command_block(command).to_vec(),
    }
}

/// Tape transport operations recovered from Sony's Picture Package controls.
///
/// These are the command-byte values sent through the driver's custom
/// property and then embedded in a sequenced command-mailbox word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportCommand {
    Play,
    Pause,
    Stop,
    FastForward,
    Rewind,
}

impl TransportCommand {
    pub const fn command_byte(self) -> u8 {
        match self {
            Self::Play => 0x1a,
            Self::Pause => 0x19,
            Self::Stop => 0x18,
            Self::FastForward => 0x1c,
            Self::Rewind => 0x1b,
        }
    }
}

/// Stateful encoder used by Sony's user-mode `S2UCapture` command layer.
///
/// The sequence nibble is incremented before each command and duplicated in
/// the resulting word:
///
/// ```text
/// 00 n1 cc n0
/// ```
///
/// `cc` is [`TransportCommand::command_byte`] and `n` wraps modulo 16.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportCommandEncoder {
    sequence: u8,
}

impl TransportCommandEncoder {
    pub const fn with_sequence(sequence: u8) -> Self {
        Self {
            sequence: sequence & 0x0f,
        }
    }

    pub const fn sequence(self) -> u8 {
        self.sequence
    }

    pub fn encode(&mut self, command: TransportCommand) -> u32 {
        self.sequence = self.sequence.wrapping_add(1) & 0x0f;
        let sequence = u32::from(self.sequence);
        (sequence << 20) | (1 << 16) | (u32::from(command.command_byte()) << 8) | (sequence << 4)
    }

    pub fn command_write(&mut self, command: TransportCommand) -> RegisterWrite {
        command_write(self.encode(command))
    }
}

/// Return the status-byte acknowledgement token for the observed scan family.
///
/// Scan commands have the form `00 n9 10 n1` in the final four bytes of an
/// otherwise zeroed mailbox. The camera acknowledges them by copying `n1`
/// into byte zero of the status block at index `0x0340`.
pub fn scan_ack_token(block: &[u8]) -> Option<u8> {
    if block.len() != COMMAND_BLOCK_SIZE
        || block[..COMMAND_WORD_OFFSET].iter().any(|byte| *byte != 0)
    {
        return None;
    }
    let command = &block[COMMAND_WORD_OFFSET..];
    if command[0] == 0
        && command[1] & 0x0f == 0x09
        && command[2] == 0x10
        && command[3] == (command[1] & 0xf0) | 0x01
    {
        Some(command[3])
    } else {
        None
    }
}

/// JPEG scale factor accepted by the Sony driver configuration.
///
/// Larger values produce more quantization and therefore lower image quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScaleFactor(u8);

impl ScaleFactor {
    pub const MIN: u8 = 4;
    pub const MAX: u8 = 128;
    pub const DRIVER_DEFAULT: Self = Self(36);

    pub fn new(value: u8) -> Result<Self, ScaleFactorError> {
        if !(Self::MIN..=Self::MAX).contains(&value) {
            return Err(ScaleFactorError(value));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    /// Produce the direct register write used by this camera revision.
    pub fn register_write(self) -> RegisterWrite {
        RegisterWrite {
            index: SCALE_FACTOR_INDEX,
            data: vec![self.0],
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("Sony JPEG scale factor {0} is outside the supported range 4..=128")]
pub struct ScaleFactorError(pub u8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_command_is_big_endian_at_end_of_zeroed_mailbox() {
        let block = command_block(0x0069_1061);
        assert_eq!(&block[..COMMAND_WORD_OFFSET], &[0; COMMAND_WORD_OFFSET]);
        assert_eq!(&block[COMMAND_WORD_OFFSET..], &[0x00, 0x69, 0x10, 0x61]);
    }

    #[test]
    fn scan_command_exposes_its_status_acknowledgement_token() {
        assert_eq!(scan_ack_token(&command_block(0x0079_1071)), Some(0x71));
        assert_eq!(scan_ack_token(&command_block(0x00a9_10a1)), Some(0xa1));
        assert_eq!(scan_ack_token(&command_block(0x0018_8010)), None);
        let mut nonzero_prefix = command_block(0x0079_1071);
        nonzero_prefix[4] = 1;
        assert_eq!(scan_ack_token(&nonzero_prefix), None);
    }

    #[test]
    fn transport_commands_match_sony_picture_package() {
        assert_eq!(TransportCommand::Play.command_byte(), 0x1a);
        assert_eq!(TransportCommand::Pause.command_byte(), 0x19);
        assert_eq!(TransportCommand::Stop.command_byte(), 0x18);
        assert_eq!(TransportCommand::FastForward.command_byte(), 0x1c);
        assert_eq!(TransportCommand::Rewind.command_byte(), 0x1b);
    }

    #[test]
    fn transport_encoder_duplicates_and_wraps_sequence_nibble() {
        let mut encoder = TransportCommandEncoder::default();
        assert_eq!(encoder.encode(TransportCommand::Play), 0x0011_1a10);
        assert_eq!(
            encoder.command_write(TransportCommand::Pause),
            command_write(0x0021_1920)
        );

        let mut wrapping = TransportCommandEncoder::with_sequence(0x0f);
        assert_eq!(wrapping.encode(TransportCommand::Stop), 0x0001_1800);
        assert_eq!(wrapping.sequence(), 0);
    }

    #[test]
    fn scale_factor_bounds_and_register_write_are_explicit() {
        assert!(ScaleFactor::new(3).is_err());
        assert_eq!(
            ScaleFactor::new(20).unwrap().register_write(),
            RegisterWrite {
                index: 0x007c,
                data: vec![20],
            }
        );
        assert!(ScaleFactor::new(129).is_err());
    }

    #[test]
    fn semantic_device_manager_sequences_match_the_trace() {
        assert_eq!(
            POWER_OFF_REGISTERS[0],
            RegisterValue {
                index: 0x35,
                value: 0x06,
            }
        );
        assert_eq!(
            AUDIO_ON_REGISTERS.map(|register| register.index),
            [0x23, 0x24, 0x22, 0x35]
        );
        assert_eq!(POWER_ON_STAGE_DELAY_US, 10_000);

        let init = device_init_registers(0x3f, 0x03);
        assert_eq!(init.len(), 21);
        assert_eq!(
            init[3],
            RegisterValue {
                index: 0x61,
                value: 0x3f,
            }
        );
        assert_eq!(
            init[18],
            RegisterValue {
                index: 0x51,
                value: 0x03,
            }
        );
    }
}
