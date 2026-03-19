use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use hysteria_core::Client;
use hysteria_extras::speedtest::{Client as SpeedtestClient, SPEEDTEST_ADDR};
use tokio::{sync::oneshot, task::JoinHandle, time};

use crate::cli::SpeedtestArgs;

pub async fn run_speedtest_command(client: &Client, args: &SpeedtestArgs) -> Result<()> {
    let size_based = args.data_size.is_some();
    let data_size = args.data_size.unwrap_or(0);
    let duration = if size_based {
        Duration::ZERO
    } else {
        args.duration
    };

    if !args.skip_download {
        run_single_test(
            client,
            "download",
            size_based,
            true,
            data_size,
            duration,
            args.use_bytes,
        )
        .await?;
    }
    if !args.skip_upload {
        run_single_test(
            client,
            "upload",
            size_based,
            false,
            data_size,
            duration,
            args.use_bytes,
        )
        .await?;
    }
    Ok(())
}

async fn run_single_test(
    client: &Client,
    name: &'static str,
    size_based: bool,
    download: bool,
    data_size: u32,
    duration: Duration,
    use_bytes: bool,
) -> Result<()> {
    println!("performing {name} test");
    let stream = client
        .tcp(SPEEDTEST_ADDR)
        .await
        .with_context(|| "failed to connect (server may not support speed test)")?;
    let mut speedtest = SpeedtestClient::new(stream);

    let total_bytes = Arc::new(AtomicU64::new(0));
    let (stop_tx, reporter) = spawn_progress_reporter(
        name,
        total_bytes.clone(),
        size_based,
        data_size,
        duration,
        use_bytes,
    );

    let summary = if download {
        speedtest
            .download(data_size, duration, |bytes| {
                total_bytes.fetch_add(bytes, Ordering::Relaxed);
            })
            .await
    } else {
        speedtest
            .upload(data_size, duration, |bytes| {
                total_bytes.fetch_add(bytes, Ordering::Relaxed);
            })
            .await
    }
    .with_context(|| format!("{name} test failed"))?;

    let _ = stop_tx.send(());
    let _ = reporter.await;
    println!(
        "{name} complete: bytes={} elapsed={} average={}",
        summary.bytes,
        humantime::format_duration(summary.elapsed),
        format_speed(summary.bytes, summary.elapsed, use_bytes)
    );
    Ok(())
}

fn spawn_progress_reporter(
    name: &'static str,
    total_bytes: Arc<AtomicU64>,
    size_based: bool,
    data_size: u32,
    duration: Duration,
    use_bytes: bool,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let start = Instant::now();
        let mut last_report = Instant::now();
        let mut last_total = 0_u64;
        let mut ticker = time::interval(if size_based {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(500)
        });
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let total = total_bytes.load(Ordering::Relaxed);
                    let delta = total.saturating_sub(last_total);
                    let elapsed = now.duration_since(last_report);
                    let total_elapsed = now.duration_since(start);
                    last_report = now;
                    last_total = total;

                    let progress = if size_based {
                        if data_size == 0 {
                            100.0
                        } else {
                            total as f64 / data_size as f64 * 100.0
                        }
                    } else if duration.is_zero() {
                        100.0
                    } else {
                        total_elapsed.as_secs_f64() / duration.as_secs_f64() * 100.0
                    };

                    let current_speed = format_speed(delta, elapsed, use_bytes);
                    let average_speed = format_speed(total, total_elapsed, use_bytes);
                    if size_based {
                        println!(
                            "{name}ing: total_bytes={} progress={:.2}% current={} average={}",
                            total,
                            progress.min(100.0),
                            current_speed,
                            average_speed,
                        );
                    } else {
                        println!(
                            "{name}ing: elapsed={} remaining={} total_bytes={} progress={:.2}% current={} average={}",
                            humantime::format_duration(total_elapsed),
                            humantime::format_duration(duration.saturating_sub(total_elapsed)),
                            total,
                            progress.min(100.0),
                            current_speed,
                            average_speed,
                        );
                    }
                }
            }
        }
    });
    (stop_tx, handle)
}

pub fn format_speed(bytes: u64, duration: Duration, use_bytes: bool) -> String {
    if duration.is_zero() {
        return if use_bytes {
            "0.00 B/s".to_string()
        } else {
            "0.00 bps".to_string()
        };
    }

    let mut speed = bytes as f64 / duration.as_secs_f64();
    let units = if use_bytes {
        ["B/s", "KB/s", "MB/s", "GB/s"]
    } else {
        speed *= 8.0;
        ["bps", "Kbps", "Mbps", "Gbps"]
    };

    let mut unit_index = 0usize;
    while speed > 1000.0 && unit_index < units.len() - 1 {
        speed /= 1000.0;
        unit_index += 1;
    }
    format!("{speed:.2} {}", units[unit_index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_speed_matches_go_style() {
        assert_eq!(
            format_speed(125_000, Duration::from_secs(1), false),
            "1000.00 Kbps"
        );
        assert_eq!(
            format_speed(125_000, Duration::from_secs(1), true),
            "125.00 KB/s"
        );
    }
}
