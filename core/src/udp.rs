use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::{
    net::{UdpSocket, lookup_host},
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
    time,
};

use crate::{
    CoreError, CoreResult,
    frag::{Defragger, frag_udp_message},
    limit::BandwidthLimiter,
    protocol::{MAX_DATAGRAM_FRAME_SIZE, MAX_UDP_SIZE, UDPMessage, parse_udp_message},
};

pub const DEFAULT_CLIENT_UDP_MESSAGE_CHANNEL_SIZE: usize = 1024;
const UDP_IDLE_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const DEFAULT_UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpSessionConfig {
    pub message_channel_size: usize,
}

impl Default for UdpSessionConfig {
    fn default() -> Self {
        Self {
            message_channel_size: DEFAULT_CLIENT_UDP_MESSAGE_CHANNEL_SIZE,
        }
    }
}

#[derive(Clone)]
pub struct UdpSession {
    inner: Arc<ClientUdpSessionInner>,
}

struct ClientUdpSessionInner {
    session_id: u32,
    manager: Arc<ClientUdpManager>,
    receiver: AsyncMutex<mpsc::Receiver<UDPMessage>>,
    defragger: AsyncMutex<Defragger>,
    closed: AtomicBool,
}

impl fmt::Debug for UdpSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpSession")
            .field("session_id", &self.inner.session_id)
            .finish_non_exhaustive()
    }
}

impl UdpSession {
    pub fn session_id(&self) -> u32 {
        self.inner.session_id
    }

    pub async fn receive(&self) -> CoreResult<(Vec<u8>, String)> {
        loop {
            let message = {
                let mut receiver = self.inner.receiver.lock().await;
                receiver.recv().await
            };

            let Some(message) = message else {
                return Err(CoreError::Closed("udp session closed".into()));
            };

            let mut defragger = self.inner.defragger.lock().await;
            if let Some(message) = defragger.feed(message) {
                return Ok((message.data, message.addr));
            }
        }
    }

    pub async fn send(&self, data: &[u8], addr: &str) -> CoreResult<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(CoreError::Closed("udp session closed".into()));
        }

        let message = UDPMessage {
            session_id: self.inner.session_id,
            packet_id: 0,
            frag_id: 0,
            frag_count: 1,
            addr: addr.to_string(),
            data: data.to_vec(),
        };
        send_udp_message(
            &self.inner.manager.connection,
            self.inner.manager.tx_limiter.as_ref(),
            &message,
        )
        .await
    }

    pub async fn close(&self) -> CoreResult<()> {
        self.inner.close();
        Ok(())
    }
}

impl Drop for UdpSession {
    fn drop(&mut self) {
        self.inner.close();
    }
}

impl ClientUdpSessionInner {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.manager.remove_session(self.session_id);
        }
    }
}

pub(crate) struct ClientUdpManager {
    connection: quinn::Connection,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
    sessions: Mutex<HashMap<u32, mpsc::Sender<UDPMessage>>>,
    next_id: AtomicU32,
    closed: AtomicBool,
}

impl ClientUdpManager {
    pub(crate) fn new(
        connection: quinn::Connection,
        tx_limiter: Option<Arc<BandwidthLimiter>>,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            connection,
            tx_limiter,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            closed: AtomicBool::new(false),
        });
        tokio::spawn(client_udp_reader_loop(manager.clone()));
        manager
    }

    pub(crate) fn new_udp(self: &Arc<Self>, config: UdpSessionConfig) -> CoreResult<UdpSession> {
        if self.closed.load(Ordering::Acquire) {
            return Err(CoreError::Closed("udp session manager closed".into()));
        }
        if config.message_channel_size == 0 {
            return Err(CoreError::Config(
                "udp session message_channel_size must be greater than 0".into(),
            ));
        }

        let session_id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let (sender, receiver) = mpsc::channel(config.message_channel_size);
        self.sessions
            .lock()
            .expect("udp session mutex poisoned")
            .insert(session_id, sender);

        Ok(UdpSession {
            inner: Arc::new(ClientUdpSessionInner {
                session_id,
                manager: self.clone(),
                receiver: AsyncMutex::new(receiver),
                defragger: AsyncMutex::new(Defragger::default()),
                closed: AtomicBool::new(false),
            }),
        })
    }

    fn remove_session(&self, session_id: u32) {
        self.sessions
            .lock()
            .expect("udp session mutex poisoned")
            .remove(&session_id);
    }

    fn close_all(&self) {
        self.closed.store(true, Ordering::Release);
        self.sessions
            .lock()
            .expect("udp session mutex poisoned")
            .clear();
    }
}

async fn client_udp_reader_loop(manager: Arc<ClientUdpManager>) {
    loop {
        let datagram = match manager.connection.read_datagram().await {
            Ok(datagram) => datagram,
            Err(_) => {
                manager.close_all();
                return;
            }
        };

        let Ok(message) = parse_udp_message(&datagram) else {
            continue;
        };

        let sender = manager
            .sessions
            .lock()
            .expect("udp session mutex poisoned")
            .get(&message.session_id)
            .cloned();
        if let Some(sender) = sender {
            let _ = sender.send(message).await;
        }
    }
}

pub(crate) async fn run_server_udp(
    connection: quinn::Connection,
    idle_timeout: Duration,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
) -> CoreResult<()> {
    let mut sessions = HashMap::<u32, ServerUdpSession>::new();
    let (close_tx, mut close_rx) = mpsc::unbounded_channel::<u32>();
    let mut cleanup_interval = time::interval(UDP_IDLE_CLEANUP_INTERVAL);

    let run_result = loop {
        tokio::select! {
            _ = cleanup_interval.tick() => {
                let now = Instant::now();
                let idle_ids: Vec<u32> = sessions
                    .iter()
                    .filter_map(|(session_id, session)| session.is_idle(now, idle_timeout).then_some(*session_id))
                    .collect();
                for session_id in idle_ids {
                    if let Some(mut session) = sessions.remove(&session_id) {
                        session.close();
                    }
                }
            }
            maybe_session_id = close_rx.recv() => {
                let Some(session_id) = maybe_session_id else {
                    continue;
                };
                if let Some(mut session) = sessions.remove(&session_id) {
                    session.close();
                }
            }
            datagram = connection.read_datagram() => {
                match datagram {
                    Ok(datagram) => {
                        let Ok(message) = parse_udp_message(&datagram) else {
                            continue;
                        };

                        let session = match sessions.entry(message.session_id) {
                            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                entry.insert(ServerUdpSession::new(
                                    message.session_id,
                                    connection.clone(),
                                    tx_limiter.clone(),
                                    close_tx.clone(),
                                ))
                            }
                        };

                        if let Err(_err) = session.feed(message).await {
                            session.close();
                        }
                    }
                    Err(quinn::ConnectionError::ApplicationClosed { .. })
                    | Err(quinn::ConnectionError::LocallyClosed) => break Ok(()),
                    Err(err) => break Err(err.into()),
                }
            }
        }
    };

    for (_, mut session) in sessions {
        session.close();
    }
    run_result
}

struct ServerUdpSession {
    session_id: u32,
    connection: quinn::Connection,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
    close_tx: mpsc::UnboundedSender<u32>,
    defragger: Defragger,
    socket: Option<Arc<UdpSocket>>,
    receive_task: Option<JoinHandle<()>>,
    last_activity: Instant,
}

impl ServerUdpSession {
    fn new(
        session_id: u32,
        connection: quinn::Connection,
        tx_limiter: Option<Arc<BandwidthLimiter>>,
        close_tx: mpsc::UnboundedSender<u32>,
    ) -> Self {
        Self {
            session_id,
            connection,
            tx_limiter,
            close_tx,
            defragger: Defragger::default(),
            socket: None,
            receive_task: None,
            last_activity: Instant::now(),
        }
    }

    async fn feed(&mut self, message: UDPMessage) -> CoreResult<()> {
        self.last_activity = Instant::now();
        let Some(message) = self.defragger.feed(message) else {
            return Ok(());
        };

        if self.socket.is_none() {
            self.init_socket(&message.addr).await?;
        }

        if let Some(socket) = &self.socket {
            send_udp_packet(socket, &message.addr, &message.data).await?;
        }
        Ok(())
    }

    async fn init_socket(&mut self, addr: &str) -> CoreResult<()> {
        let bind_addr = resolve_udp_bind_addr(addr).await?;
        let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
        let receive_socket = socket.clone();
        let connection = self.connection.clone();
        let session_id = self.session_id;
        let tx_limiter = self.tx_limiter.clone();
        let close_tx = self.close_tx.clone();

        self.receive_task = Some(tokio::spawn(async move {
            let _ =
                server_udp_receive_loop(session_id, connection, receive_socket, tx_limiter).await;
            let _ = close_tx.send(session_id);
        }));
        self.socket = Some(socket);

        Ok(())
    }

    fn is_idle(&self, now: Instant, timeout: Duration) -> bool {
        now.duration_since(self.last_activity) > timeout
    }

    fn close(&mut self) {
        if let Some(handle) = self.receive_task.take() {
            handle.abort();
        }
        self.socket.take();
    }
}

async fn resolve_udp_bind_addr(addr: &str) -> CoreResult<SocketAddr> {
    let mut resolved = lookup_host(addr)
        .await
        .map_err(|err| CoreError::Dial(err.to_string()))?;
    let target = resolved
        .next()
        .ok_or_else(|| CoreError::Dial(format!("failed to resolve udp target {addr}")))?;
    Ok(match target {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().expect("valid ipv4 bind address"),
        SocketAddr::V6(_) => "[::]:0".parse().expect("valid ipv6 bind address"),
    })
}

async fn send_udp_packet(socket: &UdpSocket, addr: &str, data: &[u8]) -> CoreResult<()> {
    let mut resolved = lookup_host(addr)
        .await
        .map_err(|err| CoreError::Dial(err.to_string()))?;
    let target = resolved
        .next()
        .ok_or_else(|| CoreError::Dial(format!("failed to resolve udp target {addr}")))?;
    socket.send_to(data, target).await?;
    Ok(())
}

async fn server_udp_receive_loop(
    session_id: u32,
    connection: quinn::Connection,
    socket: Arc<UdpSocket>,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
) -> CoreResult<()> {
    let mut buffer = vec![0_u8; MAX_UDP_SIZE];
    loop {
        let (size, from_addr) = socket.recv_from(&mut buffer).await?;
        let message = UDPMessage {
            session_id,
            packet_id: 0,
            frag_id: 0,
            frag_count: 1,
            addr: from_addr.to_string(),
            data: buffer[..size].to_vec(),
        };
        send_udp_message(&connection, tx_limiter.as_ref(), &message).await?;
    }
}

async fn send_udp_message(
    connection: &quinn::Connection,
    tx_limiter: Option<&Arc<BandwidthLimiter>>,
    message: &UDPMessage,
) -> CoreResult<()> {
    match send_single_udp_message(connection, tx_limiter, message).await {
        Ok(()) => Ok(()),
        Err(quinn::SendDatagramError::TooLarge) => {
            let max_size = connection
                .max_datagram_size()
                .unwrap_or(MAX_DATAGRAM_FRAME_SIZE);
            if max_size <= message.header_size() {
                return Err(CoreError::Transport(
                    "udp datagram header exceeds peer datagram size".into(),
                ));
            }

            let mut fragmented = message.clone();
            fragmented.packet_id = rand::random::<u16>().max(1);
            for fragment in frag_udp_message(&fragmented, max_size) {
                send_single_udp_message(connection, tx_limiter, &fragment).await?;
            }
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

async fn send_single_udp_message(
    connection: &quinn::Connection,
    tx_limiter: Option<&Arc<BandwidthLimiter>>,
    message: &UDPMessage,
) -> Result<(), quinn::SendDatagramError> {
    let mut buffer = vec![0_u8; message.size()];
    let size = message
        .serialize(&mut buffer)
        .ok_or(quinn::SendDatagramError::TooLarge)?;
    if let Some(limiter) = tx_limiter {
        limiter.wait_for(size).await;
    }
    connection.send_datagram(Bytes::copy_from_slice(&buffer[..size]))
}
