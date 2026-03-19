use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use rand::RngCore;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

pub const SPEEDTEST_DEST: &str = "@SpeedTest";
pub const SPEEDTEST_ADDR: &str = "@SpeedTest:0";
pub const CHUNK_SIZE: usize = 64 * 1024;

const TYPE_DOWNLOAD: u8 = 0x01;
const TYPE_UPLOAD: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferSummary {
    pub elapsed: Duration,
    pub bytes: u64,
}

pub struct Client<S> {
    conn: S,
}

impl<S> Client<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(conn: S) -> Self {
        Self { conn }
    }

    pub async fn download<F>(
        &mut self,
        data_size: u32,
        duration: Duration,
        mut on_bytes: F,
    ) -> io::Result<TransferSummary>
    where
        F: FnMut(u64),
    {
        let req_size = if duration.is_zero() {
            data_size
        } else {
            u32::MAX
        };
        write_download_request(&mut self.conn, req_size).await?;
        let (ok, message) = read_download_response(&mut self.conn).await?;
        if !ok {
            return Err(io::Error::other(format!(
                "server rejected download request: {message}"
            )));
        }

        let start = Instant::now();
        let deadline = (!duration.is_zero()).then_some(tokio::time::Instant::now() + duration);
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        let mut remaining = data_size;
        let mut total_bytes = 0_u64;

        loop {
            let read_cap = if duration.is_zero() {
                if remaining == 0 {
                    break;
                }
                remaining.min(CHUNK_SIZE as u32) as usize
            } else {
                CHUNK_SIZE
            };

            let result = if let Some(deadline) = deadline {
                match tokio::time::timeout_at(deadline, self.conn.read(&mut buffer[..read_cap]))
                    .await
                {
                    Ok(result) => result,
                    Err(_) => break,
                }
            } else {
                self.conn.read(&mut buffer[..read_cap]).await
            };

            let size = result?;
            if size == 0 {
                if duration.is_zero() && remaining > 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "download stream closed early",
                    ));
                }
                break;
            }

            total_bytes += size as u64;
            on_bytes(size as u64);
            if duration.is_zero() {
                remaining -= size as u32;
            }
        }

        Ok(TransferSummary {
            elapsed: start.elapsed(),
            bytes: total_bytes,
        })
    }

    pub async fn upload<F>(
        &mut self,
        data_size: u32,
        duration: Duration,
        mut on_bytes: F,
    ) -> io::Result<TransferSummary>
    where
        F: FnMut(u64),
    {
        let req_size = if duration.is_zero() {
            data_size
        } else {
            u32::MAX
        };
        write_upload_request(&mut self.conn, req_size).await?;
        let (ok, message) = read_upload_response(&mut self.conn).await?;
        if !ok {
            return Err(io::Error::other(format!(
                "server rejected upload request: {message}"
            )));
        }

        let deadline = (!duration.is_zero()).then_some(tokio::time::Instant::now() + duration);
        let buffer = vec![0_u8; CHUNK_SIZE];
        let mut remaining = data_size;

        loop {
            let write_cap = if duration.is_zero() {
                if remaining == 0 {
                    break;
                }
                remaining.min(CHUNK_SIZE as u32) as usize
            } else {
                CHUNK_SIZE
            };

            let result = if let Some(deadline) = deadline {
                match tokio::time::timeout_at(deadline, self.conn.write(&buffer[..write_cap])).await
                {
                    Ok(result) => result,
                    Err(_) => break,
                }
            } else {
                self.conn.write(&buffer[..write_cap]).await
            };

            let size = result?;
            if size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "upload stream wrote zero bytes",
                ));
            }

            on_bytes(size as u64);
            if duration.is_zero() {
                remaining -= size as u32;
            }
        }

        if duration.is_zero() {
            let summary = read_upload_summary(&mut self.conn).await?;
            Ok(TransferSummary {
                elapsed: summary.0,
                bytes: summary.1 as u64,
            })
        } else {
            self.conn.shutdown().await?;
            let summary = read_upload_summary(&mut self.conn).await?;
            Ok(TransferSummary {
                elapsed: summary.0,
                bytes: summary.1 as u64,
            })
        }
    }
}

pub fn spawn_server_conn() -> DuplexStream {
    let (client_conn, server_conn) = tokio::io::duplex(CHUNK_SIZE * 2);
    tokio::spawn(async move {
        let _ = serve_conn(server_conn).await;
    });
    client_conn
}

pub async fn serve_conn<S>(mut conn: S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut typ = [0_u8; 1];
    conn.read_exact(&mut typ).await?;
    match typ[0] {
        TYPE_DOWNLOAD => handle_download(&mut conn).await,
        TYPE_UPLOAD => handle_upload(&mut conn).await,
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown speedtest request type {other}"),
        )),
    }
}

async fn handle_download<S>(conn: &mut S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let requested = read_download_request(conn).await?;
    write_download_response(conn, true, "OK").await?;

    let mut chunk = vec![0_u8; CHUNK_SIZE];
    rand::rng().fill_bytes(&mut chunk);

    if requested == u32::MAX {
        loop {
            if let Err(err) = conn.write_all(&chunk).await {
                return if is_peer_closed(&err) {
                    Ok(())
                } else {
                    Err(err)
                };
            }
        }
    }

    let mut remaining = requested as usize;
    while remaining > 0 {
        let size = remaining.min(CHUNK_SIZE);
        conn.write_all(&chunk[..size]).await?;
        remaining -= size;
    }
    Ok(())
}

async fn handle_upload<S>(conn: &mut S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let requested = read_upload_request(conn).await?;
    write_upload_response(conn, true, "OK").await?;

    let mut buffer = vec![0_u8; CHUNK_SIZE];
    let start = Instant::now();

    if requested == u32::MAX {
        let mut total_bytes = 0_u64;
        loop {
            match conn.read(&mut buffer).await {
                Ok(0) => {
                    return write_upload_summary(
                        conn,
                        start.elapsed(),
                        total_bytes.min(u32::MAX as u64) as u32,
                    )
                    .await;
                }
                Ok(read) => {
                    total_bytes = total_bytes.saturating_add(read as u64);
                }
                Err(err) => {
                    return if is_peer_closed(&err) {
                        Ok(())
                    } else {
                        Err(err)
                    };
                }
            }
        }
    }

    let mut remaining = requested as usize;
    while remaining > 0 {
        let size = remaining.min(CHUNK_SIZE);
        let read = conn.read(&mut buffer[..size]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "upload stream closed early",
            ));
        }
        remaining -= read;
    }

    write_upload_summary(conn, start.elapsed(), requested).await
}

fn is_peer_closed(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

pub async fn read_download_request<R>(reader: &mut R) -> io::Result<u32>
where
    R: AsyncRead + Unpin,
{
    reader.read_u32().await
}

pub async fn write_download_request<W>(writer: &mut W, length: u32) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = [0_u8; 5];
    buf[0] = TYPE_DOWNLOAD;
    buf[1..].copy_from_slice(&length.to_be_bytes());
    writer.write_all(&buf).await
}

pub async fn read_download_response<R>(reader: &mut R) -> io::Result<(bool, String)>
where
    R: AsyncRead + Unpin,
{
    let status = reader.read_u8().await?;
    let msg_len = reader.read_u16().await? as usize;
    let mut msg = vec![0_u8; msg_len];
    reader.read_exact(&mut msg).await?;
    Ok((status == 0, String::from_utf8_lossy(&msg).into_owned()))
}

pub async fn write_download_response<W>(writer: &mut W, ok: bool, message: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_status_message(writer, ok, message).await
}

pub async fn read_upload_request<R>(reader: &mut R) -> io::Result<u32>
where
    R: AsyncRead + Unpin,
{
    reader.read_u32().await
}

pub async fn write_upload_request<W>(writer: &mut W, length: u32) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = [0_u8; 5];
    buf[0] = TYPE_UPLOAD;
    buf[1..].copy_from_slice(&length.to_be_bytes());
    writer.write_all(&buf).await
}

pub async fn read_upload_response<R>(reader: &mut R) -> io::Result<(bool, String)>
where
    R: AsyncRead + Unpin,
{
    let status = reader.read_u8().await?;
    let msg_len = reader.read_u16().await? as usize;
    let mut msg = vec![0_u8; msg_len];
    reader.read_exact(&mut msg).await?;
    Ok((status == 0, String::from_utf8_lossy(&msg).into_owned()))
}

pub async fn write_upload_response<W>(writer: &mut W, ok: bool, message: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_status_message(writer, ok, message).await
}

pub async fn read_upload_summary<R>(reader: &mut R) -> io::Result<(Duration, u32)>
where
    R: AsyncRead + Unpin,
{
    let millis = reader.read_u32().await?;
    let bytes = reader.read_u32().await?;
    Ok((Duration::from_millis(millis as u64), bytes))
}

pub async fn write_upload_summary<W>(
    writer: &mut W,
    duration: Duration,
    bytes: u32,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buf = [0_u8; 8];
    let millis = duration.as_millis().min(u32::MAX as u128) as u32;
    buf[..4].copy_from_slice(&millis.to_be_bytes());
    buf[4..].copy_from_slice(&bytes.to_be_bytes());
    writer.write_all(&buf).await
}

async fn write_status_message<W>(writer: &mut W, ok: bool, message: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let message_bytes = Arc::<[u8]>::from(message.as_bytes());
    if message_bytes.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "speedtest message too long",
        ));
    }

    let mut buf = Vec::with_capacity(3 + message_bytes.len());
    buf.push(if ok { 0 } else { 1 });
    buf.extend_from_slice(&(message_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(&message_bytes);
    writer.write_all(&buf).await
}
