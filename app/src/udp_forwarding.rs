use std::{
    collections::HashMap,
    net::{SocketAddr, UdpSocket as StdUdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use hysteria_core::{Client, UdpSession, UdpSessionConfig};
use socket2::SockRef;
use tokio::{net::UdpSocket, sync::Mutex, task::JoinHandle, time};

use crate::config::UdpForwardingEntry;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const IDLE_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const UDP_BUFFER_SIZE: usize = 64 * 1024;

pub async fn serve_udp_forwarder(config: UdpForwardingEntry, client: Client) -> Result<()> {
    let socket = Arc::new(bind_udp_forward_socket(&config)?);
    let local_addr = socket
        .local_addr()
        .context("failed to read UDP forwarding listen address")?;

    let timeout = if config.timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        config.timeout
    };
    let session_config = UdpSessionConfig {
        message_channel_size: if config.channel_depth == 0 {
            UdpSessionConfig::default().message_channel_size
        } else {
            config.channel_depth
        },
    };
    if let Some(mtu) = config.wireguard_mtu {
        println!(
            "wireguard forwarding: {} -> {} mtu={} timeout={} socket_recv_buffer={} socket_send_buffer={} channel_depth={}",
            local_addr,
            config.remote,
            mtu,
            humantime::format_duration(timeout),
            config.socket_receive_buffer.max(0),
            config.socket_send_buffer.max(0),
            session_config.message_channel_size,
        );
    } else {
        println!(
            "udp forwarding: {} -> {} timeout={} socket_recv_buffer={} socket_send_buffer={} channel_depth={}",
            local_addr,
            config.remote,
            humantime::format_duration(timeout),
            config.socket_receive_buffer.max(0),
            config.socket_send_buffer.max(0),
            session_config.message_channel_size,
        );
    }
    let sessions = Arc::new(Mutex::new(
        HashMap::<SocketAddr, Arc<UdpForwardSession>>::new(),
    ));
    let cleanup_task = tokio::spawn(idle_cleanup_loop(sessions.clone(), timeout));
    let mut buffer = vec![0_u8; UDP_BUFFER_SIZE];

    let run_result = async {
        loop {
            let (size, peer_addr) = socket
                .recv_from(&mut buffer)
                .await
                .with_context(|| format!("failed to receive UDP packet for {}", config.remote))?;

            let session = get_or_create_session(
                socket.clone(),
                sessions.clone(),
                client.clone(),
                peer_addr,
                session_config,
            )
            .await?;

            if let Err(err) = session.feed(&buffer[..size], &config.remote).await {
                eprintln!(
                    "udp forwarding upload error {peer_addr} -> {}: {err}",
                    config.remote
                );
                close_session(sessions.clone(), peer_addr, session).await;
            }
        }
    }
    .await;

    cleanup_task.abort();
    close_all_sessions(sessions).await;
    run_result
}

struct UdpForwardSession {
    tunnel: UdpSession,
    last_activity: Mutex<Instant>,
    timed_out: AtomicBool,
    _receive_task: JoinHandle<()>,
}

impl UdpForwardSession {
    async fn feed(&self, data: &[u8], remote: &str) -> Result<()> {
        self.touch().await;
        self.tunnel.send(data, remote).await?;
        Ok(())
    }

    async fn touch(&self) {
        *self.last_activity.lock().await = Instant::now();
    }

    async fn is_idle(&self, timeout: Duration) -> bool {
        Instant::now().duration_since(*self.last_activity.lock().await) > timeout
    }

    fn mark_timed_out(&self) -> bool {
        !self.timed_out.swap(true, Ordering::AcqRel)
    }
}

async fn get_or_create_session(
    socket: Arc<UdpSocket>,
    sessions: Arc<Mutex<HashMap<SocketAddr, Arc<UdpForwardSession>>>>,
    client: Client,
    peer_addr: SocketAddr,
    session_config: UdpSessionConfig,
) -> Result<Arc<UdpForwardSession>> {
    if let Some(existing) = sessions.lock().await.get(&peer_addr).cloned() {
        return Ok(existing);
    }

    let tunnel = client
        .udp_with_config(session_config)
        .with_context(|| format!("failed to open proxied UDP session for {peer_addr}"))?;
    let receive_tunnel = tunnel.clone();
    let receive_socket = socket.clone();
    let receive_sessions = sessions.clone();

    let receive_task = tokio::spawn(async move {
        loop {
            match receive_tunnel.receive().await {
                Ok((payload, _)) => {
                    if let Some(session) = receive_sessions.lock().await.get(&peer_addr).cloned() {
                        session.touch().await;
                    }
                    if let Err(err) = receive_socket.send_to(&payload, peer_addr).await {
                        eprintln!(
                            "udp forwarding download error {} -> {peer_addr}: {err}",
                            peer_addr
                        );
                        break;
                    }
                }
                Err(err) => {
                    let timed_out = receive_sessions
                        .lock()
                        .await
                        .get(&peer_addr)
                        .map(|session| session.timed_out.load(Ordering::Acquire))
                        .unwrap_or(false);
                    if !timed_out {
                        eprintln!("udp forwarding session {peer_addr} closed: {err}");
                    }
                    break;
                }
            }
        }

        if let Some(session) = receive_sessions.lock().await.remove(&peer_addr) {
            let _ = session.tunnel.close().await;
        }
    });

    let session = Arc::new(UdpForwardSession {
        tunnel,
        last_activity: Mutex::new(Instant::now()),
        timed_out: AtomicBool::new(false),
        _receive_task: receive_task,
    });
    sessions.lock().await.insert(peer_addr, session.clone());
    Ok(session)
}

async fn idle_cleanup_loop(
    sessions: Arc<Mutex<HashMap<SocketAddr, Arc<UdpForwardSession>>>>,
    timeout: Duration,
) {
    let mut ticker = time::interval(IDLE_CLEANUP_INTERVAL);
    loop {
        ticker.tick().await;

        let snapshot: Vec<_> = sessions
            .lock()
            .await
            .iter()
            .map(|(peer_addr, session)| (*peer_addr, session.clone()))
            .collect();
        let mut expired = Vec::new();
        for (peer_addr, session) in snapshot {
            if session.is_idle(timeout).await {
                expired.push((peer_addr, session));
            }
        }

        for (peer_addr, session) in expired {
            if session.mark_timed_out() {
                close_session(sessions.clone(), peer_addr, session).await;
            }
        }
    }
}

async fn close_session(
    sessions: Arc<Mutex<HashMap<SocketAddr, Arc<UdpForwardSession>>>>,
    peer_addr: SocketAddr,
    session: Arc<UdpForwardSession>,
) {
    sessions.lock().await.remove(&peer_addr);
    let _ = session.tunnel.close().await;
}

async fn close_all_sessions(sessions: Arc<Mutex<HashMap<SocketAddr, Arc<UdpForwardSession>>>>) {
    let entries: Vec<_> = sessions.lock().await.drain().collect();
    for (_, session) in entries {
        let _ = session.tunnel.close().await;
    }
}

fn bind_udp_forward_socket(config: &UdpForwardingEntry) -> Result<UdpSocket> {
    let socket = StdUdpSocket::bind(&config.listen)
        .with_context(|| format!("failed to bind UDP forwarding listener {}", config.listen))?;
    configure_socket_buffers(
        &socket,
        config.socket_receive_buffer,
        config.socket_send_buffer,
    );
    socket
        .set_nonblocking(true)
        .context("failed to mark UDP forwarding socket nonblocking")?;
    UdpSocket::from_std(socket).context("failed to create async UDP forwarding socket")
}

fn configure_socket_buffers(socket: &StdUdpSocket, recv: usize, send: usize) {
    let sock_ref = SockRef::from(socket);
    if recv > 0 {
        let _ = sock_ref.set_recv_buffer_size(recv);
    }
    if send > 0 {
        let _ = sock_ref.set_send_buffer_size(send);
    }
}
