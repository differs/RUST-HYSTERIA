use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use hysteria_core::{Client, UdpSession};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::Mutex,
};

#[derive(Clone, Debug)]
pub struct LocalSocksConfig {
    pub listen: String,
    pub username: String,
    pub password: String,
    pub disable_udp: bool,
}

const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_METHOD_NONE: u8 = 0x00;
const SOCKS5_METHOD_USERNAME_PASSWORD: u8 = 0x02;
const SOCKS5_METHOD_UNACCEPTABLE: u8 = 0xff;
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_CMD_UDP_ASSOCIATE: u8 = 0x03;
const SOCKS5_REP_SUCCESS: u8 = 0x00;
const SOCKS5_REP_HOST_UNREACHABLE: u8 = 0x04;
const SOCKS5_REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;
const USERPASS_VERSION: u8 = 0x01;
const USERPASS_STATUS_SUCCESS: u8 = 0x00;
const USERPASS_STATUS_FAILURE: u8 = 0x01;

pub async fn serve_socks5(config: LocalSocksConfig, client: Client) -> Result<()> {
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind SOCKS5 listener {}", config.listen))?;

    let auth = if !config.username.is_empty() && !config.password.is_empty() {
        Some((config.username, config.password))
    } else {
        None
    };

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let client = client.clone();
        let auth = auth.clone();
        let disable_udp = config.disable_udp;
        tokio::spawn(async move {
            if let Err(err) = handle_client(stream, client, auth, disable_udp).await {
                eprintln!("SOCKS5 connection {peer_addr} failed: {err:#}");
            }
        });
    }
}

async fn handle_client(
    mut stream: TcpStream,
    client: Client,
    auth: Option<(String, String)>,
    disable_udp: bool,
) -> Result<()> {
    negotiate(&mut stream, auth.as_ref()).await?;
    let request = read_request(&mut stream).await?;
    match request.command {
        SOCKS5_CMD_CONNECT => handle_connect(stream, client, &request.address).await,
        SOCKS5_CMD_UDP_ASSOCIATE if !disable_udp => handle_udp_associate(stream, client).await,
        _ => {
            write_reply(&mut stream, SOCKS5_REP_COMMAND_NOT_SUPPORTED, None).await?;
            Ok(())
        }
    }
}

async fn negotiate(stream: &mut TcpStream, auth: Option<&(String, String)>) -> Result<()> {
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header).await?;
    if header[0] != SOCKS5_VERSION {
        bail!("unsupported SOCKS version {}", header[0]);
    }

    let method_count = header[1] as usize;
    let mut methods = vec![0_u8; method_count];
    stream.read_exact(&mut methods).await?;

    let selected = if auth.is_some() {
        SOCKS5_METHOD_USERNAME_PASSWORD
    } else {
        SOCKS5_METHOD_NONE
    };
    if !methods.contains(&selected) {
        stream
            .write_all(&[SOCKS5_VERSION, SOCKS5_METHOD_UNACCEPTABLE])
            .await?;
        bail!("no acceptable SOCKS5 authentication method");
    }

    stream.write_all(&[SOCKS5_VERSION, selected]).await?;

    if let Some((expected_username, expected_password)) = auth {
        let mut auth_header = [0_u8; 2];
        stream.read_exact(&mut auth_header).await?;
        if auth_header[0] != USERPASS_VERSION {
            stream
                .write_all(&[USERPASS_VERSION, USERPASS_STATUS_FAILURE])
                .await?;
            bail!("unsupported username/password auth version");
        }

        let username_len = auth_header[1] as usize;
        let mut username = vec![0_u8; username_len];
        stream.read_exact(&mut username).await?;

        let mut password_len = [0_u8; 1];
        stream.read_exact(&mut password_len).await?;
        let mut password = vec![0_u8; password_len[0] as usize];
        stream.read_exact(&mut password).await?;

        if username == expected_username.as_bytes() && password == expected_password.as_bytes() {
            stream
                .write_all(&[USERPASS_VERSION, USERPASS_STATUS_SUCCESS])
                .await?;
        } else {
            stream
                .write_all(&[USERPASS_VERSION, USERPASS_STATUS_FAILURE])
                .await?;
            bail!("invalid SOCKS5 credentials");
        }
    }

    Ok(())
}

struct SocksRequest {
    command: u8,
    address: String,
}

async fn read_request(stream: &mut TcpStream) -> Result<SocksRequest> {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != SOCKS5_VERSION {
        bail!("unsupported SOCKS request version {}", header[0]);
    }
    let address = read_address(stream, header[3]).await?;
    Ok(SocksRequest {
        command: header[1],
        address,
    })
}

async fn read_address(stream: &mut TcpStream, atyp: u8) -> Result<String> {
    let host = match atyp {
        SOCKS5_ATYP_IPV4 => {
            let mut raw = [0_u8; 4];
            stream.read_exact(&mut raw).await?;
            IpAddr::V4(Ipv4Addr::from(raw)).to_string()
        }
        SOCKS5_ATYP_IPV6 => {
            let mut raw = [0_u8; 16];
            stream.read_exact(&mut raw).await?;
            IpAddr::V6(Ipv6Addr::from(raw)).to_string()
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len).await?;
            let mut raw = vec![0_u8; len[0] as usize];
            stream.read_exact(&mut raw).await?;
            String::from_utf8(raw).context("invalid SOCKS5 domain address")?
        }
        _ => bail!("unsupported SOCKS5 address type {}", atyp),
    };

    let mut port_bytes = [0_u8; 2];
    stream.read_exact(&mut port_bytes).await?;
    let port = u16::from_be_bytes(port_bytes);
    Ok(format!("{host}:{port}"))
}

async fn handle_connect(mut stream: TcpStream, client: Client, address: &str) -> Result<()> {
    match client.tcp(address).await {
        Ok(mut remote) => {
            write_reply(&mut stream, SOCKS5_REP_SUCCESS, None).await?;
            let _ = copy_bidirectional(&mut stream, &mut remote).await;
            Ok(())
        }
        Err(err) => {
            write_reply(&mut stream, SOCKS5_REP_HOST_UNREACHABLE, None).await?;
            Err(err.into())
        }
    }
}

async fn handle_udp_associate(mut control: TcpStream, client: Client) -> Result<()> {
    let bind_ip = control
        .local_addr()
        .context("failed to read SOCKS5 local address")?
        .ip();
    let udp_socket = Arc::new(
        UdpSocket::bind(SocketAddr::new(bind_ip, 0))
            .await
            .context("failed to bind SOCKS5 UDP socket")?,
    );
    let hy_udp = client.udp().context("failed to open proxied UDP session")?;

    write_reply(
        &mut control,
        SOCKS5_REP_SUCCESS,
        Some(
            udp_socket
                .local_addr()
                .context("failed to read SOCKS5 UDP listen address")?,
        ),
    )
    .await?;

    let client_addr = Arc::new(Mutex::new(None::<SocketAddr>));

    let mut local_task = tokio::spawn(udp_local_to_remote(
        udp_socket.clone(),
        hy_udp.clone(),
        client_addr.clone(),
    ));
    let mut remote_task = tokio::spawn(udp_remote_to_local(
        udp_socket.clone(),
        hy_udp.clone(),
        client_addr,
    ));
    let mut control_task = tokio::spawn(async move {
        let mut buf = [0_u8; 1024];
        loop {
            let size = control.read(&mut buf).await?;
            if size == 0 {
                return Ok::<(), anyhow::Error>(());
            }
        }
    });

    let result = tokio::select! {
        joined = &mut local_task => joined.context("SOCKS5 UDP upload task panicked")?,
        joined = &mut remote_task => joined.context("SOCKS5 UDP download task panicked")?,
        joined = &mut control_task => joined.context("SOCKS5 control task panicked")?,
    };

    local_task.abort();
    remote_task.abort();
    control_task.abort();
    let _ = hy_udp.close().await;
    result
}

async fn udp_local_to_remote(
    udp_socket: Arc<UdpSocket>,
    hy_udp: UdpSession,
    client_addr: Arc<Mutex<Option<SocketAddr>>>,
) -> Result<()> {
    let mut buffer = [0_u8; 4096];
    loop {
        let (size, peer_addr) = udp_socket.recv_from(&mut buffer).await?;
        let Some((destination, payload)) = parse_udp_datagram(&buffer[..size])? else {
            continue;
        };

        let mut client = client_addr.lock().await;
        match *client {
            None => *client = Some(peer_addr),
            Some(existing) if existing != peer_addr => continue,
            Some(_) => {}
        }
        drop(client);

        hy_udp.send(&payload, &destination).await?;
    }
}

async fn udp_remote_to_local(
    udp_socket: Arc<UdpSocket>,
    hy_udp: UdpSession,
    client_addr: Arc<Mutex<Option<SocketAddr>>>,
) -> Result<()> {
    loop {
        let (payload, from_addr) = hy_udp.receive().await?;
        let packet = build_udp_datagram(&from_addr, &payload)?;
        let target = { *client_addr.lock().await };
        if let Some(target) = target {
            udp_socket.send_to(&packet, target).await?;
        }
    }
}

async fn write_reply(
    stream: &mut TcpStream,
    reply: u8,
    bind_addr: Option<SocketAddr>,
) -> Result<()> {
    let bind_addr =
        bind_addr.unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
    let mut payload = vec![SOCKS5_VERSION, reply, 0];
    payload.extend_from_slice(&encode_socket_addr(bind_addr)?);
    stream.write_all(&payload).await?;
    Ok(())
}

fn parse_udp_datagram(buf: &[u8]) -> Result<Option<(String, Vec<u8>)>> {
    if buf.len() < 4 || buf[0] != 0 || buf[1] != 0 || buf[2] != 0 {
        return Ok(None);
    }
    let atyp = buf[3];
    let (address, payload_offset) = parse_address_from_bytes(&buf[4..], atyp)?;
    Ok(Some((address, buf[4 + payload_offset..].to_vec())))
}

fn build_udp_datagram(address: &str, payload: &[u8]) -> Result<Vec<u8>> {
    let mut packet = vec![0_u8, 0_u8, 0_u8];
    packet.extend_from_slice(&encode_address(address)?);
    packet.extend_from_slice(payload);
    Ok(packet)
}

fn encode_socket_addr(addr: SocketAddr) -> Result<Vec<u8>> {
    Ok(match addr {
        SocketAddr::V4(addr) => {
            let mut bytes = vec![SOCKS5_ATYP_IPV4];
            bytes.extend_from_slice(&addr.ip().octets());
            bytes.extend_from_slice(&addr.port().to_be_bytes());
            bytes
        }
        SocketAddr::V6(addr) => {
            let mut bytes = vec![SOCKS5_ATYP_IPV6];
            bytes.extend_from_slice(&addr.ip().octets());
            bytes.extend_from_slice(&addr.port().to_be_bytes());
            bytes
        }
    })
}

fn encode_address(address: &str) -> Result<Vec<u8>> {
    let (host, port) = split_host_port(address)?;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        let mut bytes = vec![SOCKS5_ATYP_IPV4];
        bytes.extend_from_slice(&ip.octets());
        bytes.extend_from_slice(&port.to_be_bytes());
        return Ok(bytes);
    }
    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        let mut bytes = vec![SOCKS5_ATYP_IPV6];
        bytes.extend_from_slice(&ip.octets());
        bytes.extend_from_slice(&port.to_be_bytes());
        return Ok(bytes);
    }

    if host.len() > u8::MAX as usize {
        bail!("SOCKS5 domain name is too long");
    }
    let mut bytes = vec![SOCKS5_ATYP_DOMAIN, host.len() as u8];
    bytes.extend_from_slice(host.as_bytes());
    bytes.extend_from_slice(&port.to_be_bytes());
    Ok(bytes)
}

fn parse_address_from_bytes(buf: &[u8], atyp: u8) -> Result<(String, usize)> {
    match atyp {
        SOCKS5_ATYP_IPV4 => {
            if buf.len() < 6 {
                bail!("truncated SOCKS5 IPv4 address");
            }
            let host = Ipv4Addr::from([buf[0], buf[1], buf[2], buf[3]]).to_string();
            let port = u16::from_be_bytes([buf[4], buf[5]]);
            Ok((format!("{host}:{port}"), 6))
        }
        SOCKS5_ATYP_IPV6 => {
            if buf.len() < 18 {
                bail!("truncated SOCKS5 IPv6 address");
            }
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&buf[..16]);
            let host = Ipv6Addr::from(octets).to_string();
            let port = u16::from_be_bytes([buf[16], buf[17]]);
            Ok((format!("[{host}]:{port}"), 18))
        }
        SOCKS5_ATYP_DOMAIN => {
            if buf.is_empty() {
                bail!("truncated SOCKS5 domain address");
            }
            let host_len = buf[0] as usize;
            if buf.len() < 1 + host_len + 2 {
                bail!("truncated SOCKS5 domain address");
            }
            let host = String::from_utf8(buf[1..1 + host_len].to_vec())
                .context("invalid SOCKS5 domain address")?;
            let port = u16::from_be_bytes([buf[1 + host_len], buf[1 + host_len + 1]]);
            Ok((format!("{host}:{port}"), 1 + host_len + 2))
        }
        _ => bail!("unsupported SOCKS5 address type {atyp}"),
    }
}

fn split_host_port(address: &str) -> Result<(String, u16)> {
    if let Ok(socket_addr) = address.parse::<SocketAddr>() {
        return Ok((socket_addr.ip().to_string(), socket_addr.port()));
    }

    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("missing port in address {address}"))?;
    Ok((
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        port.parse()
            .with_context(|| format!("invalid port in address {address}"))?,
    ))
}
