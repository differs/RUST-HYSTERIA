use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Client, CoreError, CoreResult};

pub const HEALTH_CHECK_DEST: &str = "@HealthCheck:0";
const HEALTH_CHECK_REQUEST: &[u8] = b"ping";
const HEALTH_CHECK_RESPONSE: &[u8] = b"pong";

pub async fn run_client_health_check(client: &Client) -> CoreResult<()> {
    let mut stream = client
        .tcp(HEALTH_CHECK_DEST)
        .await?;
    stream
        .write_all(HEALTH_CHECK_REQUEST)
        .await
        .map_err(|err| CoreError::Transport(err.to_string()))?;
    stream
        .shutdown()
        .await
        .map_err(|err| CoreError::Transport(err.to_string()))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|err| CoreError::Transport(err.to_string()))?;
    if response != HEALTH_CHECK_RESPONSE {
        return Err(CoreError::Dial("unexpected healthcheck response".into()));
    }
    Ok(())
}
