use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose};
use hysteria_core::{Client, TcpProxyStream};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy},
    net::{TcpListener, TcpStream},
};

use crate::config::HttpConfig;

const KEEP_ALIVE_TIMEOUT_SECS: u64 = 60;
const MAX_HTTP_HEADER_SIZE: usize = 64 * 1024;
const MAX_CHUNK_LINE_SIZE: usize = 8 * 1024;

pub async fn serve_http_proxy(config: HttpConfig, client: Client) -> Result<()> {
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind HTTP proxy listener {}", config.listen))?;
    println!(
        "HTTP proxy listening: {}",
        listener
            .local_addr()
            .context("failed to read HTTP proxy listen address")?
    );

    let auth = if !config.username.is_empty() && !config.password.is_empty() {
        Some(Arc::new(HttpAuth {
            username: config.username,
            password: config.password,
            realm: if config.realm.is_empty() {
                "Hysteria".to_string()
            } else {
                config.realm
            },
        }))
    } else {
        None
    };

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let client = client.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, client, auth).await {
                eprintln!("HTTP proxy connection {peer_addr} failed: {err:#}");
            }
        });
    }
}

#[derive(Debug)]
struct HttpAuth {
    username: String,
    password: String,
    realm: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    None,
    ContentLength(usize),
    Chunked,
    UntilEof,
}

#[derive(Debug)]
struct HttpRequestHead {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
}

#[derive(Debug)]
struct HttpResponseHead {
    version: String,
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
}

async fn handle_connection(
    mut stream: TcpStream,
    client: Client,
    auth: Option<Arc<HttpAuth>>,
) -> Result<()> {
    let mut client_buffer = Vec::with_capacity(2048);

    loop {
        let Some(request) = read_http_request(&mut stream, &mut client_buffer).await? else {
            return Ok(());
        };

        if let Some(auth) = auth.as_ref() {
            if !proxy_authorized(&request.headers, auth) {
                send_proxy_auth_required(&mut stream, &request.version, &auth.realm).await?;
                return Ok(());
            }
        }

        if request.method.eq_ignore_ascii_case("CONNECT") {
            return handle_connect(stream, client, request, client_buffer).await;
        }

        let keep_alive_requested = request_keep_alive(&request);
        let keep_alive = handle_http_request(
            &mut stream,
            &client,
            request,
            &mut client_buffer,
            keep_alive_requested,
        )
        .await?;
        if !keep_alive {
            return Ok(());
        }
    }
}

async fn read_http_request(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<Option<HttpRequestHead>> {
    let Some(header_bytes) = read_http_head(stream, buffer).await? else {
        return Ok(None);
    };

    let header_text =
        std::str::from_utf8(&header_bytes).context("HTTP request header is not valid UTF-8")?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP request line"))?;
    let mut request_parts = request_line.splitn(3, ' ');
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP method"))?
        .to_string();
    let target = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP request target"))?
        .to_string();
    let version = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP version"))?
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("malformed HTTP header line"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    Ok(Some(HttpRequestHead {
        method,
        target,
        version,
        headers,
    }))
}

async fn read_http_response(
    stream: &mut TcpProxyStream,
    buffer: &mut Vec<u8>,
) -> Result<Option<HttpResponseHead>> {
    let Some(header_bytes) = read_http_head(stream, buffer).await? else {
        return Ok(None);
    };

    let header_text =
        std::str::from_utf8(&header_bytes).context("HTTP response header is not valid UTF-8")?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP response status line"))?;
    let mut status_parts = status_line.splitn(3, ' ');
    let version = status_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP response version"))?
        .to_string();
    let status = status_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP response status"))?
        .parse::<u16>()
        .context("invalid HTTP response status")?;
    let reason = status_parts.next().unwrap_or("").trim().to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("malformed HTTP header line"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    Ok(Some(HttpResponseHead {
        version,
        status,
        reason,
        headers,
    }))
}

async fn read_http_head<R>(stream: &mut R, buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 2048];

    loop {
        if let Some(header_end) = find_header_end(buffer) {
            let header = buffer[..header_end].to_vec();
            buffer.drain(..header_end + 4);
            return Ok(Some(header));
        }

        let size = stream.read(&mut chunk).await?;
        if size == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            bail!("unexpected EOF while reading HTTP header");
        }
        buffer.extend_from_slice(&chunk[..size]);
        if buffer.len() > MAX_HTTP_HEADER_SIZE {
            bail!("HTTP header is too large");
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn proxy_authorized(headers: &[(String, String)], auth: &HttpAuth) -> bool {
    let Some(header) = header_value(headers, "Proxy-Authorization") else {
        return false;
    };
    let Some(encoded) = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))
    else {
        return false;
    };
    let decoded = general_purpose::STANDARD
        .decode(encoded.trim())
        .or_else(|_| general_purpose::URL_SAFE.decode(encoded.trim()));
    let Ok(decoded) = decoded else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((username, password)) = decoded.split_once(':') else {
        return false;
    };
    username == auth.username && password == auth.password
}

async fn handle_connect(
    mut stream: TcpStream,
    client: Client,
    request: HttpRequestHead,
    mut client_buffer: Vec<u8>,
) -> Result<()> {
    let request_addr = authority_to_addr(&request.target)?;
    match client.tcp(&request_addr).await {
        Ok(mut remote) => {
            send_status(
                &mut stream,
                &request.version,
                200,
                "Connection Established",
                false,
            )
            .await?;
            if !client_buffer.is_empty() {
                remote.write_all(&client_buffer).await?;
                client_buffer.clear();
            }
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut remote).await;
            Ok(())
        }
        Err(err) => {
            send_status(&mut stream, &request.version, 502, "Bad Gateway", false).await?;
            Err(err.into())
        }
    }
}

async fn handle_http_request(
    stream: &mut TcpStream,
    client: &Client,
    request: HttpRequestHead,
    client_buffer: &mut Vec<u8>,
    keep_alive_requested: bool,
) -> Result<bool> {
    let request_body_kind = request_body_kind(&request.headers)?;
    let (request_addr, request_target, host_header) =
        parse_proxy_target(&request.target, &request.headers)?;

    let mut remote = match client.tcp(&request_addr).await {
        Ok(remote) => remote,
        Err(err) => {
            send_status(stream, &request.version, 502, "Bad Gateway", false).await?;
            return Err(err.into());
        }
    };

    let outbound_request = build_outbound_request(&request, &request_target, &host_header)
        .context("build outbound request")?;
    remote.write_all(&outbound_request).await?;
    transfer_body(&mut *stream, client_buffer, &mut remote, request_body_kind).await?;
    remote.shutdown().await?;

    let mut remote_buffer = Vec::with_capacity(2048);
    loop {
        let Some(response) = read_http_response(&mut remote, &mut remote_buffer).await? else {
            send_status(stream, &request.version, 502, "Bad Gateway", false).await?;
            bail!("remote closed before sending an HTTP response");
        };

        let is_informational = (100..200).contains(&response.status) && response.status != 101;
        let body_kind = response_body_kind(&request.method, response.status, &response.headers)?;
        let can_keep_alive =
            keep_alive_requested && body_kind != BodyKind::UntilEof && !is_informational;
        let outbound_response = build_outbound_response(
            &response,
            if is_informational {
                None
            } else {
                Some(can_keep_alive)
            },
        )?;
        stream.write_all(&outbound_response).await?;
        transfer_body(&mut remote, &mut remote_buffer, stream, body_kind).await?;

        if is_informational {
            continue;
        }
        return Ok(can_keep_alive);
    }
}

fn request_body_kind(headers: &[(String, String)]) -> Result<BodyKind> {
    if header_contains_token(headers, "Transfer-Encoding", "chunked") {
        return Ok(BodyKind::Chunked);
    }
    if header_value(headers, "Transfer-Encoding").is_some() {
        bail!("unsupported request Transfer-Encoding");
    }

    let content_length = header_value(headers, "Content-Length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("invalid HTTP Content-Length header")?;
    Ok(match content_length {
        Some(0) | None => BodyKind::None,
        Some(length) => BodyKind::ContentLength(length),
    })
}

fn response_body_kind(
    request_method: &str,
    status: u16,
    headers: &[(String, String)],
) -> Result<BodyKind> {
    if request_method.eq_ignore_ascii_case("HEAD")
        || matches!(status, 101 | 204 | 304)
        || (100..200).contains(&status)
    {
        return Ok(BodyKind::None);
    }
    if header_contains_token(headers, "Transfer-Encoding", "chunked") {
        return Ok(BodyKind::Chunked);
    }
    if header_value(headers, "Transfer-Encoding").is_some() {
        bail!("unsupported response Transfer-Encoding");
    }
    let content_length = header_value(headers, "Content-Length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("invalid HTTP response Content-Length header")?;
    Ok(match content_length {
        Some(0) => BodyKind::None,
        Some(length) => BodyKind::ContentLength(length),
        None => BodyKind::UntilEof,
    })
}

async fn transfer_body<R, W>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    writer: &mut W,
    body_kind: BodyKind,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match body_kind {
        BodyKind::None => Ok(()),
        BodyKind::ContentLength(length) => {
            transfer_exact_bytes(reader, buffer, writer, length).await
        }
        BodyKind::Chunked => transfer_chunked_body(reader, buffer, writer).await,
        BodyKind::UntilEof => transfer_until_eof(reader, buffer, writer).await,
    }
}

async fn transfer_exact_bytes<R, W>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    writer: &mut W,
    mut remaining: usize,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if remaining == 0 {
        return Ok(());
    }

    let buffered = buffer.len().min(remaining);
    if buffered > 0 {
        writer.write_all(&buffer[..buffered]).await?;
        buffer.drain(..buffered);
        remaining -= buffered;
    }

    if remaining > 0 {
        let mut limited = reader.take(remaining as u64);
        copy(&mut limited, writer).await?;
    }
    Ok(())
}

async fn transfer_until_eof<R, W>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    writer: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !buffer.is_empty() {
        writer.write_all(buffer).await?;
        buffer.clear();
    }
    copy(reader, writer).await?;
    Ok(())
}

async fn transfer_chunked_body<R, W>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    writer: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let line = read_crlf_line(reader, buffer).await?;
        writer.write_all(&line).await?;

        let line_text = std::str::from_utf8(&line[..line.len().saturating_sub(2)])
            .context("invalid HTTP chunk header")?;
        let chunk_len_text = line_text.split(';').next().unwrap_or_default().trim();
        let chunk_len = usize::from_str_radix(chunk_len_text, 16)
            .with_context(|| format!("invalid HTTP chunk size {chunk_len_text}"))?;

        if chunk_len == 0 {
            loop {
                let trailer = read_crlf_line(reader, buffer).await?;
                writer.write_all(&trailer).await?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }

        transfer_exact_bytes(reader, buffer, writer, chunk_len + 2).await?;
    }
}

async fn read_crlf_line<R>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(position) = find_crlf(buffer) {
            let line = buffer[..position + 2].to_vec();
            buffer.drain(..position + 2);
            return Ok(line);
        }
        if buffer.len() > MAX_CHUNK_LINE_SIZE {
            bail!("HTTP chunk line is too large");
        }
        let size = reader.read(&mut chunk).await?;
        if size == 0 {
            bail!("unexpected EOF while reading chunked body");
        }
        buffer.extend_from_slice(&chunk[..size]);
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\r\n")
}

fn parse_proxy_target(
    target: &str,
    headers: &[(String, String)],
) -> Result<(String, String, String)> {
    if let Some(rest) = target.strip_prefix("http://") {
        let (authority, path) = split_authority_and_path(rest);
        let request_addr = authority_to_addr(authority)?;
        let host_header = normalize_host_header(authority)?;
        return Ok((request_addr, path.to_string(), host_header));
    }
    if target.starts_with("https://") {
        bail!("plain HTTPS proxy requests are not supported; use CONNECT");
    }
    if target.starts_with('/') {
        let host = header_value(headers, "Host")
            .ok_or_else(|| anyhow::anyhow!("missing Host header for proxied HTTP request"))?;
        let request_addr = authority_to_addr(host)?;
        let host_header = normalize_host_header(host)?;
        return Ok((request_addr, target.to_string(), host_header));
    }

    let request_addr = authority_to_addr(target)?;
    let host_header = normalize_host_header(target)?;
    Ok((request_addr, "/".to_string(), host_header))
}

fn split_authority_and_path(value: &str) -> (&str, &str) {
    match value.find('/') {
        Some(index) => (&value[..index], &value[index..]),
        None => (value, "/"),
    }
}

fn authority_to_addr(authority: &str) -> Result<String> {
    if authority.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(authority.to_string());
    }

    let normalized = authority.trim();
    if normalized.is_empty() {
        bail!("empty authority");
    }
    if normalized.starts_with('[') && normalized.contains("]:") {
        return Ok(normalized.to_string());
    }
    if normalized.rsplit_once(':').is_some() {
        return Ok(normalized.to_string());
    }
    Ok(format!("{normalized}:80"))
}

fn normalize_host_header(authority: &str) -> Result<String> {
    let normalized = authority.trim();
    if normalized.is_empty() {
        bail!("empty authority");
    }
    if let Ok(socket_addr) = normalized.parse::<std::net::SocketAddr>() {
        return Ok(match socket_addr {
            std::net::SocketAddr::V4(addr) if addr.port() == 80 => addr.ip().to_string(),
            std::net::SocketAddr::V4(_) => normalized.to_string(),
            std::net::SocketAddr::V6(addr) if addr.port() == 80 => format!("[{}]", addr.ip()),
            std::net::SocketAddr::V6(_) => normalized.to_string(),
        });
    }
    if normalized.ends_with(":80") {
        return Ok(normalized[..normalized.len() - 3].to_string());
    }
    Ok(normalized.to_string())
}

fn build_outbound_request(
    request: &HttpRequestHead,
    request_target: &str,
    host_header: &str,
) -> Result<Vec<u8>> {
    let mut bytes = format!(
        "{} {} {}\r\n",
        request.method, request_target, request.version
    )
    .into_bytes();
    let mut saw_host = false;

    for (name, value) in &request.headers {
        if should_skip_request_header(name) {
            continue;
        }
        if name.eq_ignore_ascii_case("Host") {
            saw_host = true;
            bytes.extend_from_slice(format!("Host: {host_header}\r\n").as_bytes());
        } else {
            bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }

    if !saw_host {
        bytes.extend_from_slice(format!("Host: {host_header}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(bytes)
}

fn build_outbound_response(
    response: &HttpResponseHead,
    keep_alive: Option<bool>,
) -> Result<Vec<u8>> {
    let reason = if response.reason.is_empty() {
        http::StatusCode::from_u16(response.status)
            .ok()
            .and_then(|status| status.canonical_reason().map(ToString::to_string))
            .unwrap_or_default()
    } else {
        response.reason.clone()
    };

    let mut bytes = format!("{} {} {}\r\n", response.version, response.status, reason).into_bytes();
    for (name, value) in &response.headers {
        if should_skip_response_header(name) {
            continue;
        }
        bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }

    match keep_alive {
        Some(true) => {
            bytes.extend_from_slice(b"Connection: keep-alive\r\n");
            bytes.extend_from_slice(b"Proxy-Connection: keep-alive\r\n");
            bytes.extend_from_slice(
                format!("Keep-Alive: timeout={KEEP_ALIVE_TIMEOUT_SECS}\r\n").as_bytes(),
            );
        }
        Some(false) => bytes.extend_from_slice(b"Connection: close\r\n"),
        None => {}
    }
    bytes.extend_from_slice(b"\r\n");
    Ok(bytes)
}

fn should_skip_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "proxy-connection"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "upgrade"
    )
}

fn should_skip_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "proxy-connection"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "upgrade"
    )
}

fn request_keep_alive(request: &HttpRequestHead) -> bool {
    version_at_least_http_11(&request.version)
        && (header_contains_token(&request.headers, "Proxy-Connection", "keep-alive")
            || header_contains_token(&request.headers, "Connection", "keep-alive"))
}

fn version_at_least_http_11(version: &str) -> bool {
    match version.strip_prefix("HTTP/") {
        Some(rest) => {
            let Some((major, minor)) = rest.split_once('.') else {
                return false;
            };
            match (major.parse::<u16>(), minor.parse::<u16>()) {
                (Ok(major), Ok(minor)) => major > 1 || (major == 1 && minor >= 1),
                _ => false,
            }
        }
        None => false,
    }
}

fn header_contains_token(headers: &[(String, String)], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .flat_map(|(_, value)| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

async fn send_status(
    stream: &mut TcpStream,
    version: &str,
    status: u16,
    reason: &str,
    keep_alive: bool,
) -> Result<()> {
    let response = if keep_alive {
        format!(
            "{version} {status} {reason}\r\nContent-Length: 0\r\nConnection: keep-alive\r\nProxy-Connection: keep-alive\r\nKeep-Alive: timeout={KEEP_ALIVE_TIMEOUT_SECS}\r\n\r\n"
        )
    } else {
        format!("{version} {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn send_proxy_auth_required(
    stream: &mut TcpStream,
    version: &str,
    realm: &str,
) -> Result<()> {
    let response = format!(
        "{version} 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"{realm}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}
