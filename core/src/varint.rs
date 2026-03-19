use std::io::Read;

use crate::errors::ProtocolError;

pub const MAX_VARINT: u64 = 0x3fff_ffff_ffff_ffff;

pub fn len(value: u64) -> usize {
    assert!(value <= MAX_VARINT, "{value:#x} doesn't fit into 62 bits");
    match value {
        0..=63 => 1,
        64..=16_383 => 2,
        16_384..=1_073_741_823 => 4,
        _ => 8,
    }
}

pub fn write(buf: &mut [u8], value: u64) -> Result<usize, ProtocolError> {
    if value > MAX_VARINT {
        return Err(ProtocolError::VarintOverflow);
    }
    let needed = len(value);
    if buf.len() < needed {
        return Err(ProtocolError::UnexpectedEof);
    }

    match needed {
        1 => {
            buf[0] = value as u8;
        }
        2 => {
            buf[0] = ((value >> 8) as u8) | 0x40;
            buf[1] = value as u8;
        }
        4 => {
            buf[0] = ((value >> 24) as u8) | 0x80;
            buf[1] = (value >> 16) as u8;
            buf[2] = (value >> 8) as u8;
            buf[3] = value as u8;
        }
        8 => {
            buf[0] = ((value >> 56) as u8) | 0xc0;
            buf[1] = (value >> 48) as u8;
            buf[2] = (value >> 40) as u8;
            buf[3] = (value >> 32) as u8;
            buf[4] = (value >> 24) as u8;
            buf[5] = (value >> 16) as u8;
            buf[6] = (value >> 8) as u8;
            buf[7] = value as u8;
        }
        _ => unreachable!(),
    }

    Ok(needed)
}

pub fn read<R: Read>(reader: &mut R) -> Result<u64, ProtocolError> {
    let mut first = [0_u8; 1];
    reader.read_exact(&mut first)?;
    let needed = 1usize << (first[0] >> 6);
    let mut rest = [0_u8; 7];
    if needed > 1 {
        reader.read_exact(&mut rest[..needed - 1])?;
    }

    let mut value = (first[0] & 0x3f) as u64;
    for byte in &rest[..needed - 1] {
        value = (value << 8) | u64::from(*byte);
    }
    Ok(value)
}

pub fn read_slice(buf: &[u8]) -> Result<(u64, usize), ProtocolError> {
    let Some(first) = buf.first().copied() else {
        return Err(ProtocolError::UnexpectedEof);
    };
    let needed = 1usize << (first >> 6);
    if buf.len() < needed {
        return Err(ProtocolError::UnexpectedEof);
    }

    let mut value = (first & 0x3f) as u64;
    for byte in &buf[1..needed] {
        value = (value << 8) | u64::from(*byte);
    }
    Ok((value, needed))
}
