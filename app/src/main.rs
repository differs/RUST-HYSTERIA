mod cli;
mod config;
mod http_proxy;
mod share;
mod socks5;
mod speedtest;
mod udp_forwarding;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{AppMetadata, Cli, ClientArgs, Command, PingArgs, ServerArgs, ShareArgs, SpeedtestArgs};
use config::{
    build_client_core_config, build_runnable_client_config, build_runnable_server_config,
    load_client_config, load_server_config,
};
use http_proxy::serve_http_proxy;
use hysteria_core::{Client, Server};
use share::{build_share_uri, render_qr};
use socks5::serve_socks5;
use speedtest::run_speedtest_command;
use tokio::{io::copy_bidirectional, net::TcpListener, task::JoinSet};
use udp_forwarding::serve_udp_forwarder;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let meta = AppMetadata::current();
    execute(cli, meta).await
}

async fn execute(cli: Cli, meta: AppMetadata) -> Result<()> {
    match cli
        .command
        .clone()
        .unwrap_or(Command::Client(ClientArgs::default()))
    {
        Command::Client(args) => run_client(&cli, &meta, &args).await,
        Command::Server(args) => run_server(&cli, &meta, &args).await,
        Command::Version => run_version(&meta),
        Command::Ping(args) => run_ping(&cli, &meta, &args).await,
        Command::Share(args) => run_share(&cli, &meta, &args),
        Command::Speedtest(args) => run_speedtest(&cli, &meta, &args).await,
        Command::Update => run_update(&cli, &meta),
    }
}

async fn run_client(cli: &Cli, _meta: &AppMetadata, args: &ClientArgs) -> Result<()> {
    let loaded = load_client_config(cli.config.as_deref())?;
    let runtime = build_runnable_client_config(&loaded.value)?;
    println!("client config: {}", loaded.path.display());

    let (client, info) = Client::connect(runtime.core)
        .await
        .context("failed to connect hysteria client")?;
    println!(
        "connected: remote={} udp_enabled={} negotiated_tx={}B/s qr={}",
        client.remote_addr(),
        info.udp_enabled,
        info.tx,
        args.qr
    );
    if args.qr {
        let uri = build_share_uri(&loaded.value).context("failed to build share URI")?;
        println!("share URI: {uri}");
        println!("{}", render_qr(&uri).context("failed to render QR code")?);
    }

    let mut listeners = JoinSet::new();
    if let Some(socks5) = runtime.socks5 {
        let client = client.clone();
        listeners.spawn(async move { serve_socks5(socks5, client).await });
    }
    if let Some(http) = runtime.http {
        let client = client.clone();
        listeners.spawn(async move { serve_http_proxy(http, client).await });
    }
    for entry in runtime.tcp_forwarding {
        let listener = TcpListener::bind(&entry.listen)
            .await
            .with_context(|| format!("failed to bind TCP forwarding listener {}", entry.listen))?;
        let bound = listener
            .local_addr()
            .with_context(|| format!("failed to read local address for {}", entry.listen))?;
        println!("tcp forwarding: {} -> {}", bound, entry.remote);

        let client = client.clone();
        let remote = entry.remote.clone();
        listeners.spawn(async move { serve_tcp_forwarder(listener, client, remote).await });
    }
    for entry in runtime.udp_forwarding {
        let client = client.clone();
        listeners.spawn(async move { serve_udp_forwarder(entry, client).await });
    }

    let result = tokio::select! {
        maybe = listeners.join_next(), if !listeners.is_empty() => {
            match maybe {
                Some(joined) => joined.context("client runtime task panicked")?,
                None => Ok(()),
            }
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            println!("received shutdown signal");
            Ok(())
        }
    };

    listeners.abort_all();
    let _ = client.close().await;
    result
}

async fn run_server(cli: &Cli, _meta: &AppMetadata, _args: &ServerArgs) -> Result<()> {
    let loaded = load_server_config(cli.config.as_deref())?;
    let runtime = build_runnable_server_config(&loaded.value)?;
    println!("server config: {}", loaded.path.display());

    let server = Arc::new(
        Server::bind(runtime.core)
            .await
            .context("failed to bind hysteria server")?,
    );
    println!("server up and running: {}", server.local_addr()?);

    let task_server = server.clone();
    let mut serve_task = tokio::spawn(async move { task_server.serve().await });

    tokio::select! {
        joined = &mut serve_task => {
            joined.context("server task panicked")??;
            Ok(())
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            println!("received shutdown signal");
            server.close();
            serve_task.await.context("server task panicked")??;
            Ok(())
        }
    }
}

fn run_version(meta: &AppMetadata) -> Result<()> {
    println!("{}", meta.about_long());
    Ok(())
}

async fn run_ping(cli: &Cli, _meta: &AppMetadata, args: &PingArgs) -> Result<()> {
    let loaded = load_client_config(cli.config.as_deref())?;
    let core = build_client_core_config(&loaded.value)?;
    anyhow::ensure!(args.count > 0, "--count must be greater than 0");
    println!("ping mode");
    println!("client config: {}", loaded.path.display());

    let (client, info) = Client::connect(core)
        .await
        .context("failed to connect hysteria client")?;
    println!(
        "connected to server: remote={} udp_enabled={} negotiated_tx={}B/s",
        client.remote_addr(),
        info.udp_enabled,
        info.tx,
    );
    println!(
        "PING {} count={} interval={}",
        args.address,
        args.count,
        humantime::format_duration(args.interval)
    );

    let mut samples = Vec::new();
    let mut failures = 0_u32;
    for seq in 1..=args.count {
        let start = std::time::Instant::now();
        match client.tcp(&args.address).await {
            Ok(stream) => {
                drop(stream);
                let elapsed = start.elapsed();
                samples.push(elapsed);
                println!(
                    "reply from {}: seq={} time={:.3} ms",
                    args.address,
                    seq,
                    elapsed.as_secs_f64() * 1000.0
                );
            }
            Err(err) => {
                failures += 1;
                println!("reply from {}: seq={} error={err}", args.address, seq);
            }
        }

        if seq < args.count {
            tokio::time::sleep(args.interval).await;
        }
    }

    println!();
    println!("--- {} ping statistics ---", args.address);
    let received = samples.len() as u32;
    let loss = failures as f64 / args.count as f64 * 100.0;
    println!(
        "{} probes sent, {} successful, {:.2}% packet loss",
        args.count, received, loss
    );
    if !samples.is_empty() {
        let min = samples.iter().min().copied().unwrap();
        let max = samples.iter().max().copied().unwrap();
        let total = samples
            .iter()
            .fold(std::time::Duration::ZERO, |acc, sample| acc + *sample);
        let sample_values = samples
            .iter()
            .map(|sample| sample.as_secs_f64() * 1000.0)
            .collect::<Vec<_>>();
        let avg = total.as_secs_f64() * 1000.0 / sample_values.len() as f64;
        let variance = sample_values
            .iter()
            .map(|sample| {
                let delta = *sample - avg;
                delta * delta
            })
            .sum::<f64>()
            / sample_values.len() as f64;
        let stddev = variance.sqrt();
        let jitter = sample_values
            .windows(2)
            .map(|window| (window[1] - window[0]).abs())
            .sum::<f64>()
            / sample_values.len().saturating_sub(1).max(1) as f64;
        println!(
            "round-trip min/avg/max/stddev = {:.3}/{:.3}/{:.3}/{:.3} ms",
            min.as_secs_f64() * 1000.0,
            avg,
            max.as_secs_f64() * 1000.0,
            stddev,
        );
        println!("jitter = {:.3} ms", jitter);
    }

    client.close().await?;
    Ok(())
}

fn run_share(cli: &Cli, _meta: &AppMetadata, args: &ShareArgs) -> Result<()> {
    let loaded = load_client_config(cli.config.as_deref())?;
    let uri = build_share_uri(&loaded.value).context("failed to build share URI")?;
    if !args.no_text {
        println!("{uri}");
    }
    if args.qr {
        println!("{}", render_qr(&uri).context("failed to render QR code")?);
    }
    Ok(())
}

async fn run_speedtest(cli: &Cli, _meta: &AppMetadata, args: &SpeedtestArgs) -> Result<()> {
    let loaded = load_client_config(cli.config.as_deref())?;
    let core = build_client_core_config(&loaded.value)?;
    println!("speed test mode");
    println!("client config: {}", loaded.path.display());

    let (client, info) = Client::connect(core)
        .await
        .context("failed to connect hysteria client")?;
    println!(
        "connected to server: remote={} udp_enabled={} negotiated_tx={}B/s",
        client.remote_addr(),
        info.udp_enabled,
        info.tx,
    );

    let client_for_task = client.clone();
    let args_owned = args.clone();
    let mut run_task =
        tokio::spawn(async move { run_speedtest_command(&client_for_task, &args_owned).await });

    let result = tokio::select! {
        joined = &mut run_task => {
            joined.context("speedtest task panicked")?
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            println!("received shutdown signal");
            run_task.abort();
            Ok(())
        }
    };

    client.close().await?;
    result?;
    println!("speed test complete");
    Ok(())
}

fn run_update(cli: &Cli, meta: &AppMetadata) -> Result<()> {
    println!("[skeleton] update check mode");
    println!(
        "version={} platform={} arch={} build-type={} disable-update-check={}",
        meta.version, meta.platform, meta.arch, meta.build_type, cli.disable_update_check
    );
    Ok(())
}

async fn serve_tcp_forwarder(listener: TcpListener, client: Client, remote: String) -> Result<()> {
    loop {
        let (mut inbound, peer_addr) = listener
            .accept()
            .await
            .with_context(|| format!("failed to accept TCP forwarding connection for {remote}"))?;
        let remote_addr = remote.clone();
        let client = client.clone();

        tokio::spawn(async move {
            match client.tcp(&remote_addr).await {
                Ok(mut outbound) => {
                    if let Err(err) = copy_bidirectional(&mut inbound, &mut outbound).await {
                        eprintln!("tcp forwarding relay error {peer_addr} -> {remote_addr}: {err}");
                    }
                }
                Err(err) => {
                    eprintln!("failed to open proxied stream {peer_addr} -> {remote_addr}: {err}");
                }
            }
        });
    }
}
