use std::{io, sync::Arc};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::limit::BandwidthLimiter;

const RELAY_BUFFER_SIZE: usize = 64 * 1024;

pub(crate) async fn copy_bidirectional_with_limit<A, B>(
    a: A,
    b: B,
    a_to_b_limiter: Option<Arc<BandwidthLimiter>>,
    b_to_a_limiter: Option<Arc<BandwidthLimiter>>,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_reader, mut a_writer) = tokio::io::split(a);
    let (mut b_reader, mut b_writer) = tokio::io::split(b);
    let mut a_to_b_buffer = [0_u8; RELAY_BUFFER_SIZE];
    let mut b_to_a_buffer = [0_u8; RELAY_BUFFER_SIZE];

    tokio::try_join!(
        copy_with_limit(
            &mut a_reader,
            &mut b_writer,
            &mut a_to_b_buffer,
            a_to_b_limiter,
        ),
        copy_with_limit(
            &mut b_reader,
            &mut a_writer,
            &mut b_to_a_buffer,
            b_to_a_limiter,
        ),
    )
}

async fn copy_with_limit<R, W>(
    reader: &mut R,
    writer: &mut W,
    buffer: &mut [u8],
    limiter: Option<Arc<BandwidthLimiter>>,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut total = 0_u64;
    loop {
        let read = reader.read(buffer).await?;
        if read == 0 {
            writer.shutdown().await?;
            return Ok(total);
        }

        let mut written = 0;
        while written < read {
            let chunk = if let Some(limiter) = limiter.as_ref() {
                limiter.wait_for_chunk(read - written).await
            } else {
                read - written
            };
            writer.write_all(&buffer[written..written + chunk]).await?;
            written += chunk;
        }
        total = total.saturating_add(read as u64);
    }
}
