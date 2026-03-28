use std::{sync::Arc, time::Duration};

use hysteria_core::{
    Client, ClientConfig, CoreError, ObfsConfig, PasswordAuthenticator, Server, ServerConfig,
    run_client_health_check,
};
use hysteria_extras::speedtest::Client as SpeedtestClient;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn tls_material() -> (
    Vec<CertificateDer<'static>>,
    PrivateKeyDer<'static>,
    CertificateDer<'static>,
) {
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate certificate");
    let cert_der = cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(key_pair.serialize_der());
    (vec![cert_der.clone()], key.into(), cert_der)
}

async fn spawn_echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo listener");
    let addr = listener.local_addr().expect("echo local addr");
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let (mut reader, mut writer) = socket.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    (addr, task)
}

async fn spawn_hysteria_server(
    password: &str,
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    disable_udp: bool,
    obfs: Option<ObfsConfig>,
    speed_test: bool,
) -> (Arc<Server>, tokio::task::JoinHandle<()>) {
    let server = Arc::new(
        Server::bind(ServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            certificates,
            private_key,
            authenticator: Arc::new(PasswordAuthenticator::new(password)),
            obfs,
            speed_test,
            disable_udp,
            udp_idle_timeout: Duration::from_secs(60),
            bandwidth_max_tx: 0,
            bandwidth_max_rx: 0,
            ignore_client_bandwidth: false,
            quic: hysteria_core::QuicTransportConfig::server_default(),
        })
        .await
        .expect("bind hysteria server"),
    );

    let task_server = server.clone();
    let task = tokio::spawn(async move {
        task_server.serve().await.expect("serve hysteria server");
    });
    (server, task)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn handshake_and_tcp_proxy_work() {
    let (echo_addr, echo_task) = spawn_echo_server().await;
    let (certificates, private_key, certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, true, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.root_certificates = vec![certificate];

    let (client, info) = Client::connect(client_config)
        .await
        .expect("connect hysteria client");
    assert!(!info.udp_enabled);

    let mut stream = client
        .tcp(&echo_addr.to_string())
        .await
        .expect("open proxied tcp stream");
    stream.write_all(b"hello").await.expect("write hello");
    stream.shutdown().await.expect("shutdown client send");

    let mut received = Vec::new();
    stream
        .read_to_end(&mut received)
        .await
        .expect("read echoed bytes");
    assert_eq!(received, b"hello");

    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
    echo_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn auth_failure_returns_http_status() {
    let (certificates, private_key, certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, true, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "wrong-password".into();
    client_config.tls.root_certificates = vec![certificate];

    let err = Client::connect(client_config)
        .await
        .expect_err("authentication should fail");
    assert!(matches!(err, CoreError::Authentication(404)));

    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
}

#[test]
fn password_authenticator_matches_expected_secret() {
    let auth = PasswordAuthenticator::with_auth_id("hunter2", "demo-user");
    let peer: std::net::SocketAddr = "127.0.0.1:443".parse().unwrap();
    assert_eq!(
        hysteria_core::Authenticator::authenticate(&auth, peer, "hunter2", 0),
        Some("demo-user".to_string())
    );
    assert_eq!(
        hysteria_core::Authenticator::authenticate(&auth, peer, "wrong", 0),
        None
    );
}

async fn spawn_udp_echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
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

async fn spawn_tagged_udp_echo_server(
    tag: &'static [u8],
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind tagged udp echo socket");
    let addr = socket.local_addr().expect("tagged udp echo local addr");
    let task = tokio::spawn(async move {
        let mut buf = [0_u8; 4096];
        loop {
            let (size, peer) = match socket.recv_from(&mut buf).await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let mut response = tag.to_vec();
            response.extend_from_slice(&buf[..size]);
            let _ = socket.send_to(&response, peer).await;
        }
    });
    (addr, task)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn udp_proxy_roundtrip_works() {
    let (echo_addr, echo_task) = spawn_udp_echo_server().await;
    let (certificates, private_key, certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, false, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.root_certificates = vec![certificate];

    let (client, info) = Client::connect(client_config)
        .await
        .expect("connect hysteria client");
    assert!(info.udp_enabled);

    let udp = client.udp().expect("open proxied udp session");
    udp.send(b"hello-udp", &echo_addr.to_string())
        .await
        .expect("send udp payload");
    let (received, from) = udp.receive().await.expect("receive udp payload");
    assert_eq!(from, echo_addr.to_string());
    assert_eq!(received, b"hello-udp");

    udp.close().await.expect("close udp session");
    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
    echo_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn udp_proxy_keeps_same_destination_on_first_resolved_target() {
    let (primary_addr, primary_task) = spawn_tagged_udp_echo_server(b"primary:").await;
    let (secondary_addr, secondary_task) = spawn_tagged_udp_echo_server(b"secondary:").await;
    let (certificates, private_key, certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, false, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.root_certificates = vec![certificate];

    let (client, info) = Client::connect(client_config)
        .await
        .expect("connect hysteria client");
    assert!(info.udp_enabled);

    let udp = client.udp().expect("open proxied udp session");

    let destination = format!("localhost:{}", primary_addr.port());
    udp.send(b"first", &destination)
        .await
        .expect("send first udp payload");
    let (first_response, _) = udp.receive().await.expect("receive first udp payload");
    assert_eq!(first_response, b"primary:first");

    let drift_destination = format!("localhost:{}", secondary_addr.port());
    udp.send(b"second", &destination)
        .await
        .expect("send second udp payload");
    let (second_response, _) = udp.receive().await.expect("receive second udp payload");
    assert_eq!(second_response, b"primary:second");

    udp.send(b"third", &drift_destination)
        .await
        .expect("send third udp payload");
    let (third_response, _) = udp.receive().await.expect("receive third udp payload");
    assert_eq!(third_response, b"secondary:third");

    udp.close().await.expect("close udp session");
    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
    primary_task.abort();
    secondary_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn salamander_obfs_tcp_and_udp_work() {
    let (tcp_echo_addr, tcp_echo_task) = spawn_echo_server().await;
    let (udp_echo_addr, udp_echo_task) = spawn_udp_echo_server().await;
    let (certificates, private_key, certificate) = tls_material();
    let obfs = Some(ObfsConfig::Salamander {
        password: "average_password".to_string(),
    });
    let (server, server_task) = spawn_hysteria_server(
        "hunter2",
        certificates,
        private_key,
        false,
        obfs.clone(),
        false,
    )
    .await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.obfs = obfs;
    client_config.tls.root_certificates = vec![certificate];

    let (client, info) = Client::connect(client_config)
        .await
        .expect("connect hysteria client");
    assert!(info.udp_enabled);

    let mut tcp = client
        .tcp(&tcp_echo_addr.to_string())
        .await
        .expect("open proxied tcp stream");
    tcp.write_all(b"hello-obfs")
        .await
        .expect("write hello-obfs");
    tcp.shutdown().await.expect("shutdown proxied tcp stream");

    let mut tcp_received = Vec::new();
    tcp.read_to_end(&mut tcp_received)
        .await
        .expect("read echoed tcp bytes");
    assert_eq!(tcp_received, b"hello-obfs");

    let udp = client.udp().expect("open proxied udp session");
    udp.send(b"hello-obfs-udp", &udp_echo_addr.to_string())
        .await
        .expect("send udp payload");
    let (udp_received, from) = udp.receive().await.expect("receive udp payload");
    assert_eq!(from, udp_echo_addr.to_string());
    assert_eq!(udp_received, b"hello-obfs-udp");

    udp.close().await.expect("close udp session");
    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
    tcp_echo_task.abort();
    udp_echo_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn speedtest_server_handler_works() {
    let (certificates, private_key, certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, true, None, true).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.root_certificates = vec![certificate];

    let (client, _) = Client::connect(client_config)
        .await
        .expect("connect hysteria client");
    let stream = client
        .tcp("@SpeedTest:0")
        .await
        .expect("open speedtest stream");
    let mut speedtest = SpeedtestClient::new(stream);
    let download = speedtest
        .download(64 * 1024, Duration::ZERO, |_| {})
        .await
        .expect("speedtest download");
    assert_eq!(download.bytes, 64 * 1024);

    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn healthcheck_stream_works_without_speedtest_enabled() {
    let (certificates, private_key, certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, true, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.root_certificates = vec![certificate];

    let (client, _) = Client::connect(client_config)
        .await
        .expect("connect hysteria client");
    run_client_health_check(&client)
        .await
        .expect("healthcheck should succeed");

    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn pin_sha256_accepts_matching_certificate() {
    let (certificates, private_key, certificate) = tls_material();
    let expected_hash: [u8; 32] = Sha256::digest(certificate.as_ref()).into();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, true, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.insecure = true;
    client_config.tls.pinned_certificate_sha256 = Some(expected_hash);

    let (client, _) = Client::connect(client_config)
        .await
        .expect("connect with matching pin");

    client.close().await.expect("close client");
    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local UDP/TCP socket bind permissions"]
async fn pin_sha256_rejects_mismatched_certificate() {
    let (certificates, private_key, _certificate) = tls_material();
    let (server, server_task) =
        spawn_hysteria_server("hunter2", certificates, private_key, true, None, false).await;

    let mut client_config = ClientConfig::new(server.local_addr().unwrap(), "localhost");
    client_config.auth = "hunter2".into();
    client_config.tls.insecure = true;
    client_config.tls.pinned_certificate_sha256 = Some([0x11; 32]);

    let err = Client::connect(client_config)
        .await
        .expect_err("pin mismatch should fail");
    assert!(matches!(err, CoreError::Tls(_)));

    server.close();
    let joined = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server task timed out");
    joined.expect("server task join");
}
