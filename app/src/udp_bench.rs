use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use socket2::SockRef;
use tokio::{net::UdpSocket, sync::watch, time};

use crate::cli::{UdpBenchArgs, UdpBenchCommand, UdpBenchEchoArgs, UdpBenchRunArgs};

const BENCH_HEADER_SIZE: usize = 8;
const WG_MTU_PACKET_SIZES: [usize; 3] = [1280, 1360, 1420];
const BENCH_SOCKET_BUFFER_SIZE: usize = 8 * 1024 * 1024;

pub async fn run_udp_bench_command(args: &UdpBenchArgs) -> Result<()> {
    match &args.command {
        UdpBenchCommand::Echo(args) => run_udp_echo(args).await,
        UdpBenchCommand::Run(args) => run_udp_bench(args).await,
    }
}

async fn run_udp_echo(args: &UdpBenchEchoArgs) -> Result<()> {
    if args.workers == 0 {
        bail!("--workers must be greater than 0");
    }

    let bind_addr: SocketAddr = args
        .listen
        .parse()
        .with_context(|| format!("invalid UDP echo listen address {}", args.listen))?;
    let std_socket = bind_udp_bench_std_socket(bind_addr)?;
    let local_addr = std_socket
        .local_addr()
        .context("failed to read local UDP address")?;
    println!(
        "udp bench echo listening: {} workers={}",
        local_addr, args.workers
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut workers = Vec::with_capacity(args.workers);
    for worker_id in 0..args.workers {
        let worker_socket = if worker_id == 0 {
            std_socket
                .try_clone()
                .context("failed to clone UDP echo socket")?
        } else {
            std_socket
                .try_clone()
                .context("failed to clone UDP echo socket")?
        };
        let socket =
            UdpSocket::from_std(worker_socket).context("failed to create async UDP echo socket")?;
        let mut shutdown = shutdown_rx.clone();
        workers.push(tokio::spawn(async move {
            let mut buffer = vec![0_u8; 65_536];
            loop {
                tokio::select! {
                    recv = socket.recv_from(&mut buffer) => {
                        let (size, peer) = recv.context("failed to receive UDP packet")?;
                        socket
                            .send_to(&buffer[..size], peer)
                            .await
                            .with_context(|| format!("failed to echo UDP packet to {peer}"))?;
                    }
                    changed = shutdown.changed() => {
                        changed.context("udp echo shutdown channel closed")?;
                        if *shutdown.borrow() {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
            }
        }));
    }

    let signal = tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C");
    let _ = shutdown_tx.send(true);
    for worker in workers {
        worker.await.context("udp echo worker panicked")??;
    }
    signal?;
    println!("received shutdown signal");
    Ok(())
}

fn bind_udp_bench_std_socket(bind_addr: SocketAddr) -> Result<std::net::UdpSocket> {
    let socket = std::net::UdpSocket::bind(bind_addr)
        .with_context(|| format!("failed to bind UDP bench socket {bind_addr}"))?;
    let sock_ref = SockRef::from(&socket);
    let _ = sock_ref.set_recv_buffer_size(BENCH_SOCKET_BUFFER_SIZE);
    let _ = sock_ref.set_send_buffer_size(BENCH_SOCKET_BUFFER_SIZE);
    socket
        .set_nonblocking(true)
        .context("failed to mark UDP bench socket nonblocking")?;
    Ok(socket)
}

async fn run_udp_bench(args: &UdpBenchRunArgs) -> Result<()> {
    if args.packets == 0 {
        bail!("--packets must be greater than 0");
    }
    if !args.wg_mtu_sweep && args.packet_size < BENCH_HEADER_SIZE {
        bail!("--packet-size must be at least {BENCH_HEADER_SIZE}");
    }
    if args.target_mbps < 0.0 {
        bail!("--target-mbps must not be negative");
    }
    if args.window == 0 {
        bail!("--window must be greater than 0");
    }
    if args.tail_timeout.is_zero() {
        bail!("--tail-timeout must be greater than 0");
    }

    let target: SocketAddr = args
        .target
        .parse()
        .with_context(|| format!("invalid UDP target {}", args.target))?;
    let packet_sizes = if args.wg_mtu_sweep {
        WG_MTU_PACKET_SIZES.to_vec()
    } else {
        vec![args.packet_size]
    };

    for (index, packet_size) in packet_sizes.iter().enumerate() {
        if *packet_size < BENCH_HEADER_SIZE {
            bail!("packet size {packet_size} is smaller than bench header");
        }
        if index > 0 {
            println!();
        }
        let summary = run_udp_bench_once(args, target, *packet_size).await?;
        print_udp_bench_summary(&summary);
    }

    Ok(())
}

async fn run_udp_bench_once(
    args: &UdpBenchRunArgs,
    target: SocketAddr,
    packet_size: usize,
) -> Result<UdpBenchSummary> {
    let bind_addr = match args.listen.as_deref() {
        Some(listen) => listen
            .parse()
            .with_context(|| format!("invalid UDP listen address {listen}"))?,
        None => default_bind_addr(target),
    };
    let socket = Arc::new(bind_udp_bench_socket(bind_addr)?);
    socket
        .connect(target)
        .await
        .with_context(|| format!("failed to connect UDP bench socket to {target}"))?;
    let local_addr = socket
        .local_addr()
        .context("failed to read UDP bench local address")?;
    println!(
        "udp bench: local={} target={} packets={} packet_size={}B window={} tail_timeout={}",
        local_addr,
        target,
        args.packets,
        packet_size,
        args.window,
        humantime::format_duration(args.tail_timeout)
    );

    let send_times = Arc::new(
        (0..args.packets)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>(),
    );
    let mut send_buf = vec![0_u8; packet_size];
    let payload = &mut send_buf[BENCH_HEADER_SIZE..];
    payload.fill(b'x');

    let packet_bytes = packet_size as u64;
    let target_packets = u64::from(args.packets);
    let start = Instant::now();
    let sender_done = Arc::new(AtomicBool::new(false));
    let receive_socket = socket.clone();
    let receive_send_times = send_times.clone();
    let receive_done = sender_done.clone();
    let tail_timeout = args.tail_timeout;
    let receive_task = tokio::spawn(async move {
        receive_udp_bench(
            receive_socket,
            receive_send_times,
            target_packets,
            packet_size,
            start,
            tail_timeout,
            receive_done,
        )
        .await
    });

    for next_seq in 0..target_packets {
        let sent_ns = elapsed_nanos(start);
        send_times[next_seq as usize].store(sent_ns.max(1), Ordering::Relaxed);
        send_buf[..BENCH_HEADER_SIZE].copy_from_slice(&next_seq.to_be_bytes());
        socket
            .send(&send_buf)
            .await
            .with_context(|| format!("failed to send UDP packet to {target}"))?;
        if args.target_mbps > 0.0 {
            let target_elapsed =
                target_elapsed_for_rate(next_seq + 1, packet_size, args.target_mbps);
            let elapsed = start.elapsed();
            if target_elapsed > elapsed {
                time::sleep(target_elapsed - elapsed).await;
            }
        }
        if args.window > 0 && (next_seq as usize + 1) % args.window == 0 {
            tokio::task::yield_now().await;
        }
    }
    let send_elapsed = start.elapsed();
    sender_done.store(true, Ordering::Release);

    let (received_packets, rtts) = receive_task
        .await
        .context("udp bench receiver task panicked")??;

    Ok(UdpBenchSummary {
        packet_size,
        sent_packets: target_packets,
        received_packets,
        payload_bytes: packet_bytes,
        send_elapsed,
        elapsed: start.elapsed(),
        rtts,
    })
}

fn default_bind_addr(target: SocketAddr) -> SocketAddr {
    match target {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

fn bind_udp_bench_socket(bind_addr: SocketAddr) -> Result<UdpSocket> {
    UdpSocket::from_std(bind_udp_bench_std_socket(bind_addr)?)
        .context("failed to create async UDP bench socket")
}

async fn receive_udp_bench(
    socket: Arc<UdpSocket>,
    send_times: Arc<Vec<AtomicU64>>,
    target_packets: u64,
    packet_size: usize,
    start: Instant,
    tail_timeout: Duration,
    sender_done: Arc<AtomicBool>,
) -> Result<(u64, Vec<Duration>)> {
    let mut buffer = vec![0_u8; packet_size.max(65_536)];
    let mut received = 0_u64;
    let mut rtts = Vec::with_capacity(target_packets as usize);
    let mut last_progress = Instant::now();

    loop {
        if received == target_packets {
            break;
        }

        match time::timeout(Duration::from_millis(50), socket.recv(&mut buffer)).await {
            Ok(Ok(size)) => {
                if size < BENCH_HEADER_SIZE {
                    continue;
                }
                let seq = u64::from_be_bytes(
                    buffer[..BENCH_HEADER_SIZE]
                        .try_into()
                        .expect("bench header size checked"),
                );
                let Some(sent_ns) = send_times
                    .get(seq as usize)
                    .map(|ts| ts.swap(0, Ordering::Relaxed))
                else {
                    continue;
                };
                if sent_ns == 0 {
                    continue;
                }
                let now_ns = elapsed_nanos(start);
                rtts.push(Duration::from_nanos(now_ns.saturating_sub(sent_ns)));
                received += 1;
                last_progress = Instant::now();
            }
            Ok(Err(err)) => return Err(err).context("failed to receive UDP benchmark reply"),
            Err(_) => {
                if sender_done.load(Ordering::Acquire) && last_progress.elapsed() >= tail_timeout {
                    break;
                }
            }
        }
    }

    Ok((received, rtts))
}

fn elapsed_nanos(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

#[derive(Debug)]
struct UdpBenchSummary {
    packet_size: usize,
    sent_packets: u64,
    received_packets: u64,
    payload_bytes: u64,
    send_elapsed: Duration,
    elapsed: Duration,
    rtts: Vec<Duration>,
}

fn print_udp_bench_summary(summary: &UdpBenchSummary) {
    let lost = summary
        .sent_packets
        .saturating_sub(summary.received_packets);
    let loss_pct = lost as f64 / summary.sent_packets.max(1) as f64 * 100.0;
    println!();
    println!("--- udp bench summary ---");
    println!("packet_size={}B", summary.packet_size);
    println!(
        "packets sent={} received={} lost={} loss={:.2}%",
        summary.sent_packets, summary.received_packets, lost, loss_pct
    );
    println!(
        "payload sent={} payload received={} send_elapsed={} elapsed={}",
        summary.sent_packets.saturating_mul(summary.payload_bytes),
        summary
            .received_packets
            .saturating_mul(summary.payload_bytes),
        humantime::format_duration(summary.send_elapsed),
        humantime::format_duration(summary.elapsed)
    );
    println!(
        "throughput send={} recv={}",
        format_throughput(
            summary.sent_packets.saturating_mul(summary.payload_bytes),
            summary.send_elapsed,
        ),
        format_throughput(
            summary
                .received_packets
                .saturating_mul(summary.payload_bytes),
            summary.elapsed,
        ),
    );
    if !summary.rtts.is_empty() {
        let mut rtts = summary.rtts.clone();
        rtts.sort_unstable();
        println!(
            "rtt min/p50/p95/p99/max = {:.3}/{:.3}/{:.3}/{:.3}/{:.3} ms",
            duration_ms(rtts[0]),
            duration_ms(quantile(&rtts, 0.50)),
            duration_ms(quantile(&rtts, 0.95)),
            duration_ms(quantile(&rtts, 0.99)),
            duration_ms(*rtts.last().expect("rtts not empty")),
        );
    }
}

fn format_throughput(bytes: u64, duration: Duration) -> String {
    if duration.is_zero() {
        return "0.00 MB/s (0.00 Mbps)".to_string();
    }

    let seconds = duration.as_secs_f64();
    let bytes_per_sec = bytes as f64 / seconds;
    let mb_per_sec = bytes_per_sec / 1_000_000.0;
    let mbps = bytes_per_sec * 8.0 / 1_000_000.0;
    format!("{mb_per_sec:.2} MB/s ({mbps:.2} Mbps)")
}

fn quantile(samples: &[Duration], quantile: f64) -> Duration {
    let last = samples.len().saturating_sub(1);
    let index = ((last as f64) * quantile).round() as usize;
    samples[index.min(last)]
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn target_elapsed_for_rate(packet_count: u64, packet_size: usize, target_mbps: f64) -> Duration {
    if target_mbps <= 0.0 {
        return Duration::ZERO;
    }
    let bits = packet_count as f64 * packet_size as f64 * 8.0;
    Duration::from_secs_f64(bits / (target_mbps * 1_000_000.0))
}

#[cfg(test)]
mod tests {
    use super::{BENCH_HEADER_SIZE, format_throughput, quantile};
    use std::time::Duration;

    #[test]
    fn bench_packet_header_size_matches_sequence_encoding() {
        let seq = 42_u64;
        let bytes = seq.to_be_bytes();
        assert_eq!(bytes.len(), BENCH_HEADER_SIZE);
        assert_eq!(u64::from_be_bytes(bytes), seq);
    }

    #[test]
    fn throughput_format_includes_both_units() {
        let text = format_throughput(2_000_000, Duration::from_secs(1));
        assert!(text.contains("MB/s"));
        assert!(text.contains("Mbps"));
    }

    #[test]
    fn quantile_selects_expected_sorted_sample() {
        let samples = [
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
            Duration::from_millis(5),
        ];
        assert_eq!(quantile(&samples, 0.50), Duration::from_millis(3));
        assert_eq!(quantile(&samples, 0.95), Duration::from_millis(5));
    }
}
