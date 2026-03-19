use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    CoreError, CoreResult, ProtocolError,
    protocol::{
        FRAME_TYPE_TCP_REQUEST, MAX_ADDRESS_LENGTH, MAX_MESSAGE_LENGTH, MAX_PADDING_LENGTH,
        write_tcp_request as encode_tcp_request, write_tcp_response as encode_tcp_response,
    },
};

pub async fn read_varint<R>(reader: &mut R) -> CoreResult<u64>
where
    R: AsyncRead + Unpin,
{
    let mut first = [0_u8; 1];
    reader.read_exact(&mut first).await?;
    let needed = 1usize << (first[0] >> 6);
    let mut rest = [0_u8; 7];
    if needed > 1 {
        reader.read_exact(&mut rest[..needed - 1]).await?;
    }

    let mut value = (first[0] & 0x3f) as u64;
    for byte in &rest[..needed - 1] {
        value = (value << 8) | u64::from(*byte);
    }
    Ok(value)
}

pub async fn read_framed_tcp_request<R>(reader: &mut R) -> CoreResult<String>
where
    R: AsyncRead + Unpin,
{
    let frame_type = read_varint(reader).await?;
    if frame_type != FRAME_TYPE_TCP_REQUEST {
        return Err(CoreError::UnexpectedFrameType(frame_type));
    }
    read_tcp_request(reader).await
}

pub async fn read_tcp_request<R>(reader: &mut R) -> CoreResult<String>
where
    R: AsyncRead + Unpin,
{
    let addr_len = read_varint(reader).await? as usize;
    if addr_len == 0 || addr_len > MAX_ADDRESS_LENGTH {
        return Err(ProtocolError::InvalidAddressLength.into());
    }

    let mut addr_buf = vec![0_u8; addr_len];
    reader.read_exact(&mut addr_buf).await?;

    let padding_len = read_varint(reader).await? as usize;
    if padding_len > MAX_PADDING_LENGTH {
        return Err(ProtocolError::InvalidPaddingLength.into());
    }
    discard_exact(reader, padding_len).await?;

    Ok(String::from_utf8_lossy(&addr_buf).into_owned())
}

pub async fn write_tcp_request<W>(writer: &mut W, addr: &str) -> CoreResult<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::new();
    encode_tcp_request(&mut buf, addr)?;
    writer.write_all(&buf).await?;
    Ok(())
}

pub async fn read_tcp_response<R>(reader: &mut R) -> CoreResult<(bool, String)>
where
    R: AsyncRead + Unpin,
{
    let mut status = [0_u8; 1];
    reader.read_exact(&mut status).await?;

    let msg_len = read_varint(reader).await? as usize;
    if msg_len > MAX_MESSAGE_LENGTH {
        return Err(ProtocolError::InvalidMessageLength.into());
    }

    let mut message = vec![0_u8; msg_len];
    if msg_len > 0 {
        reader.read_exact(&mut message).await?;
    }

    let padding_len = read_varint(reader).await? as usize;
    if padding_len > MAX_PADDING_LENGTH {
        return Err(ProtocolError::InvalidPaddingLength.into());
    }
    discard_exact(reader, padding_len).await?;

    Ok((
        status[0] == 0,
        String::from_utf8_lossy(&message).into_owned(),
    ))
}

pub async fn write_tcp_response<W>(writer: &mut W, ok: bool, message: &str) -> CoreResult<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::new();
    encode_tcp_response(&mut buf, ok, message)?;
    writer.write_all(&buf).await?;
    Ok(())
}

async fn discard_exact<R>(reader: &mut R, len: usize) -> CoreResult<()>
where
    R: AsyncRead + Unpin,
{
    let mut remaining = len;
    let mut buf = [0_u8; 1024];
    while remaining > 0 {
        let chunk = remaining.min(buf.len());
        reader.read_exact(&mut buf[..chunk]).await?;
        remaining -= chunk;
    }
    Ok(())
}
