use std::io::{Read, Write};

use crate::{ProtocolError, protocol::padding, varint};

pub const FRAME_TYPE_TCP_REQUEST: u64 = 0x401;

pub const MAX_ADDRESS_LENGTH: usize = 2048;
pub const MAX_MESSAGE_LENGTH: usize = 2048;
pub const MAX_PADDING_LENGTH: usize = 4096;

pub const MAX_DATAGRAM_FRAME_SIZE: usize = 1200;
pub const MAX_UDP_SIZE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UDPMessage {
    pub session_id: u32,
    pub packet_id: u16,
    pub frag_id: u8,
    pub frag_count: u8,
    pub addr: String,
    pub data: Vec<u8>,
}

impl UDPMessage {
    pub fn header_size(&self) -> usize {
        4 + 2 + 1 + 1 + varint::len(self.addr.len() as u64) + self.addr.len()
    }

    pub fn size(&self) -> usize {
        self.header_size() + self.data.len()
    }

    pub fn serialize(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < self.size() {
            return None;
        }

        buf[..4].copy_from_slice(&self.session_id.to_be_bytes());
        buf[4..6].copy_from_slice(&self.packet_id.to_be_bytes());
        buf[6] = self.frag_id;
        buf[7] = self.frag_count;

        let mut index = 8;
        index += varint::write(&mut buf[index..], self.addr.len() as u64).ok()?;
        buf[index..index + self.addr.len()].copy_from_slice(self.addr.as_bytes());
        index += self.addr.len();
        buf[index..index + self.data.len()].copy_from_slice(&self.data);
        index += self.data.len();
        Some(index)
    }
}

pub fn parse_udp_message(buf: &[u8]) -> Result<UDPMessage, ProtocolError> {
    if buf.len() < 8 {
        return Err(ProtocolError::UnexpectedEof);
    }

    let session_id = u32::from_be_bytes(buf[0..4].try_into().expect("slice length checked"));
    let packet_id = u16::from_be_bytes(buf[4..6].try_into().expect("slice length checked"));
    let frag_id = buf[6];
    let frag_count = buf[7];

    let (addr_len, addr_len_size) = varint::read_slice(&buf[8..])?;
    if addr_len == 0 || addr_len > MAX_MESSAGE_LENGTH as u64 {
        return Err(ProtocolError::InvalidAddressLength);
    }
    let addr_len = addr_len as usize;
    let addr_start = 8 + addr_len_size;
    let addr_end = addr_start + addr_len;
    if buf.len() <= addr_end {
        return Err(ProtocolError::InvalidMessageLength);
    }

    let addr = String::from_utf8_lossy(&buf[addr_start..addr_end]).into_owned();
    let data = buf[addr_end..].to_vec();

    Ok(UDPMessage {
        session_id,
        packet_id,
        frag_id,
        frag_count,
        addr,
        data,
    })
}

pub fn read_tcp_request<R: Read>(reader: &mut R) -> Result<String, ProtocolError> {
    let addr_len = varint::read(reader)? as usize;
    if addr_len == 0 || addr_len > MAX_ADDRESS_LENGTH {
        return Err(ProtocolError::InvalidAddressLength);
    }

    let mut addr = vec![0_u8; addr_len];
    reader.read_exact(&mut addr)?;

    let padding_len = varint::read(reader)? as usize;
    if padding_len > MAX_PADDING_LENGTH {
        return Err(ProtocolError::InvalidPaddingLength);
    }

    if padding_len > 0 {
        let mut discard = vec![0_u8; padding_len];
        reader.read_exact(&mut discard)?;
    }

    Ok(String::from_utf8_lossy(&addr).into_owned())
}

pub fn write_tcp_request<W: Write>(writer: &mut W, addr: &str) -> Result<(), ProtocolError> {
    let padding = padding::tcp_request_padding();
    let addr_len = addr.len();
    let padding_len = padding.len();
    let size = varint::len(FRAME_TYPE_TCP_REQUEST)
        + varint::len(addr_len as u64)
        + addr_len
        + varint::len(padding_len as u64)
        + padding_len;
    let mut buf = vec![0_u8; size];

    let mut index = 0;
    index += varint::write(&mut buf[index..], FRAME_TYPE_TCP_REQUEST)?;
    index += varint::write(&mut buf[index..], addr_len as u64)?;
    buf[index..index + addr_len].copy_from_slice(addr.as_bytes());
    index += addr_len;
    index += varint::write(&mut buf[index..], padding_len as u64)?;
    buf[index..index + padding_len].copy_from_slice(padding.as_bytes());

    writer.write_all(&buf)?;
    Ok(())
}

pub fn read_tcp_response<R: Read>(reader: &mut R) -> Result<(bool, String), ProtocolError> {
    let mut status = [0_u8; 1];
    reader.read_exact(&mut status)?;

    let message_len = varint::read(reader)? as usize;
    if message_len > MAX_MESSAGE_LENGTH {
        return Err(ProtocolError::InvalidMessageLength);
    }

    let mut message = vec![0_u8; message_len];
    if message_len > 0 {
        reader.read_exact(&mut message)?;
    }

    let padding_len = varint::read(reader)? as usize;
    if padding_len > MAX_PADDING_LENGTH {
        return Err(ProtocolError::InvalidPaddingLength);
    }
    if padding_len > 0 {
        let mut discard = vec![0_u8; padding_len];
        reader.read_exact(&mut discard)?;
    }

    Ok((
        status[0] == 0,
        String::from_utf8_lossy(&message).into_owned(),
    ))
}

pub fn write_tcp_response<W: Write>(
    writer: &mut W,
    ok: bool,
    message: &str,
) -> Result<(), ProtocolError> {
    let padding = padding::tcp_response_padding();
    let message_len = message.len();
    let padding_len = padding.len();
    let size = 1
        + varint::len(message_len as u64)
        + message_len
        + varint::len(padding_len as u64)
        + padding_len;
    let mut buf = vec![0_u8; size];

    buf[0] = if ok { 0 } else { 1 };
    let mut index = 1;
    index += varint::write(&mut buf[index..], message_len as u64)?;
    buf[index..index + message_len].copy_from_slice(message.as_bytes());
    index += message_len;
    index += varint::write(&mut buf[index..], padding_len as u64)?;
    buf[index..index + padding_len].copy_from_slice(padding.as_bytes());

    writer.write_all(&buf)?;
    Ok(())
}
