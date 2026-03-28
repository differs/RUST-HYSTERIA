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

use crate::config::Socks5Config;

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

pub async fn serve_socks5(config: Socks5Config, client: Client) -> Result<()> {
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind SOCKS5 listener {}", config.listen))?;
    println!(
        "SOCKS5 server listening: {}",
        listener
            .local_addr()
            .context("failed to read SOCKS5 listen address")?
    );

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
    if buf.len() < 4 {
        return Ok(None);
    }
    if buf[0] != 0 || buf[1] != 0 {
        return Ok(None);
    }
    if buf[2] != 0 {
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

#[cfg(test)]
pub(crate) fn test_build_udp_datagram(address: &str, payload: &[u8]) -> Result<Vec<u8>> {
    build_udp_datagram(address, payload)
}

#[cfg(test)]
pub(crate) fn test_parse_udp_datagram(buf: &[u8]) -> Result<Option<(String, Vec<u8>)>> {
    parse_udp_datagram(buf)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use hysteria_core::{
        Client, ClientConfig, PasswordAuthenticator, QuicTransportConfig, Server, ServerConfig,
    };
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream, UdpSocket},
    };

    use super::{
        SOCKS5_ATYP_IPV4, SOCKS5_CMD_UDP_ASSOCIATE, SOCKS5_METHOD_NONE, SOCKS5_REP_SUCCESS,
        SOCKS5_VERSION, handle_client, test_build_udp_datagram, test_parse_udp_datagram,
    };

    fn tls_material() -> (
        Vec<CertificateDer<'static>>,
        PrivateKeyDer<'static>,
        CertificateDer<'static>,
    ) {
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("generate certificate");
        let cert_der = cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(key_pair.serialize_der());
        (vec![cert_der.clone()], key.into(), cert_der)
    }

    async fn spawn_hysteria_server(
        password: &str,
    ) -> (
        Arc<Server>,
        tokio::task::JoinHandle<()>,
        CertificateDer<'static>,
    ) {
        let (certificates, private_key, certificate) = tls_material();
        let server = Arc::new(
            Server::bind(ServerConfig {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                certificates,
                private_key,
                authenticator: Arc::new(PasswordAuthenticator::new(password)),
                obfs: None,
                speed_test: false,
                disable_udp: false,
                udp_idle_timeout: Duration::from_secs(60),
                bandwidth_max_tx: 0,
                bandwidth_max_rx: 0,
                ignore_client_bandwidth: false,
                quic: QuicTransportConfig::server_default(),
            })
            .await
            .expect("bind hysteria server"),
        );
        let task_server = server.clone();
        let task = tokio::spawn(async move {
            task_server.serve().await.expect("serve hysteria server");
        });
        (server, task, certificate)
    }

    async fn spawn_udp_echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind udp echo socket");
        let addr = socket.local_addr().expect("udp echo local addr");
        let task = tokio::spawn(async move {
            let mut buf = [0_u8; 4096];
            loop {
                let (size, peer) = match socket.recv_from(&mut buf).await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let _ = socket.send_to(&buf[..size], peer).await;
            }
        });
        (addr, task)
    }

    async fn connect_hysteria_client(
        server_addr: std::net::SocketAddr,
        certificate: CertificateDer<'static>,
    ) -> Client {
        let mut client_config = ClientConfig::new(server_addr, "localhost");
        client_config.auth = "hunter2".into();
        client_config.tls.root_certificates = vec![certificate];
        let (client, info) = Client::connect(client_config)
            .await
            .expect("connect hysteria client");
        assert!(info.udp_enabled);
        client
    }

    async fn spawn_socks5_session(client: Client) -> (TcpStream, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind socks5 listener");
        let listen_addr = listener.local_addr().expect("socks5 local addr");
        let task_client = client.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept socks5 client");
            if let Err(err) = handle_client(stream, task_client, None, false).await {
                let message = err.to_string();
                assert!(
                    message.contains("udp session closed") || message.contains("closed"),
                    "serve socks5 session: {err:#}"
                );
            }
        });

        let mut control = TcpStream::connect(listen_addr)
            .await
            .expect("connect to socks5 listener");
        control
            .write_all(&[SOCKS5_VERSION, 1, SOCKS5_METHOD_NONE])
            .await
            .expect("write greeting");
        let mut negotiate_reply = [0_u8; 2];
        control
            .read_exact(&mut negotiate_reply)
            .await
            .expect("read greeting reply");
        assert_eq!(negotiate_reply, [SOCKS5_VERSION, SOCKS5_METHOD_NONE]);

        let request = [
            SOCKS5_VERSION,
            SOCKS5_CMD_UDP_ASSOCIATE,
            0,
            SOCKS5_ATYP_IPV4,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        control
            .write_all(&request)
            .await
            .expect("write udp associate");

        let mut reply = [0_u8; 10];
        control
            .read_exact(&mut reply)
            .await
            .expect("read udp associate reply");
        assert_eq!(reply[0], SOCKS5_VERSION);
        assert_eq!(reply[1], SOCKS5_REP_SUCCESS);
        assert_eq!(reply[3], SOCKS5_ATYP_IPV4);
        let relay_addr = std::net::SocketAddr::from((
            std::net::Ipv4Addr::new(reply[4], reply[5], reply[6], reply[7]),
            u16::from_be_bytes([reply[8], reply[9]]),
        ));
        (control, relay_addr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socks5_udp_associate_roundtrip_works() {
        let (server, server_task, certificate) = spawn_hysteria_server("hunter2").await;
        let (echo_addr, echo_task) = spawn_udp_echo_server().await;
        let client = connect_hysteria_client(server.local_addr().unwrap(), certificate).await;

        let (control, relay_addr) = spawn_socks5_session(client.clone()).await;
        let udp_client = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind udp client");
        let payload = b"h3-probe";
        let packet =
            test_build_udp_datagram(&echo_addr.to_string(), payload).expect("build udp datagram");
        udp_client
            .send_to(&packet, relay_addr)
            .await
            .expect("send udp packet");

        let mut buf = [0_u8; 4096];
        let (size, _) = udp_client
            .recv_from(&mut buf)
            .await
            .expect("receive udp echo");
        let (from, echoed_payload) = test_parse_udp_datagram(&buf[..size])
            .expect("parse udp datagram")
            .expect("udp datagram payload");
        assert_eq!(from, echo_addr.to_string());
        assert_eq!(echoed_payload, payload);

        drop(control);
        client.close().await.expect("close client");
        server.close();
        let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server task timed out");
        joined.expect("server task join");
        echo_task.abort();
    }
}
