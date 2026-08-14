use std::{sync::Arc, sync::atomic::Ordering, time::Duration};

use anyhow::bail;
use proxy_tester_capture::{Direction, ReplayTurn};
use proxy_tester_domain::{Protocol, Scenario};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::{connector::IoStream, payload::PreparedPayloads, telemetry::Counters};

pub(crate) async fn transaction(
    stream: &mut (impl IoStream + ?Sized),
    scenario: &Scenario,
    payloads: &PreparedPayloads,
    counters: &Counters,
) -> anyhow::Result<()> {
    stream.write_all(&payloads.request).await?;
    counters
        .tx
        .fetch_add(payloads.request.len() as u64, Ordering::Relaxed);
    tokio::time::timeout(
        Duration::from_millis(scenario.timeouts.response_ms),
        read_exact_count(stream, payloads.response.len()),
    )
    .await??;
    counters
        .rx
        .fetch_add(payloads.response.len() as u64, Ordering::Relaxed);
    Ok(())
}

pub(crate) async fn replay_client(
    stream: &mut (impl IoStream + ?Sized),
    turns: &[ReplayTurn],
    scenario: &Scenario,
    counters: &Counters,
) -> anyhow::Result<()> {
    let mut transaction_started = None;
    for turn in turns {
        match turn.direction {
            Direction::ClientToServer => {
                if scenario.protocol == Protocol::Http1 {
                    transaction_started = Some(std::time::Instant::now());
                }
                stream.write_all(&turn.payload).await?;
                counters
                    .tx
                    .fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
            }
            Direction::ServerToClient => {
                tokio::time::timeout(
                    Duration::from_millis(scenario.timeouts.response_ms),
                    read_exact_count(stream, turn.payload.len()),
                )
                .await??;
                counters
                    .rx
                    .fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
                if scenario.protocol == Protocol::Http1 {
                    counters.transaction_completed();
                    if let Some(started) = transaction_started.take() {
                        counters
                            .http_latencies_us
                            .lock()
                            .await
                            .push(started.elapsed().as_micros() as u64);
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn replay_server(
    stream: &mut (impl IoStream + ?Sized),
    turns: &[ReplayTurn],
    scenario: &Scenario,
    counters: &Counters,
) -> anyhow::Result<()> {
    for turn in turns {
        match turn.direction {
            Direction::ClientToServer => {
                tokio::time::timeout(
                    Duration::from_millis(scenario.timeouts.response_ms),
                    read_exact_count(stream, turn.payload.len()),
                )
                .await??;
                counters
                    .rx
                    .fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
            }
            Direction::ServerToClient => {
                stream.write_all(&turn.payload).await?;
                counters
                    .tx
                    .fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
                if scenario.protocol == Protocol::Http1 {
                    counters.transaction_completed();
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn replay_server_detect(
    stream: &mut (impl IoStream + ?Sized),
    flows: &[Arc<[ReplayTurn]>],
    scenario: &Scenario,
    counters: &Counters,
) -> anyhow::Result<()> {
    let mut received = Vec::new();
    let mut candidates: Vec<usize> = (0..flows.len()).collect();
    loop {
        let mut byte = [0_u8; 1];
        tokio::time::timeout(
            Duration::from_millis(scenario.timeouts.response_ms),
            stream.read_exact(&mut byte),
        )
        .await??;
        received.push(byte[0]);
        candidates.retain(|index| flows[*index][0].payload.starts_with(&received));
        if candidates.is_empty() {
            bail!("first client turn does not match any capture flow");
        }
        if candidates.len() == 1 {
            let flow = &flows[candidates[0]];
            if flow[0].payload.len() == received.len() {
                counters
                    .rx
                    .fetch_add(received.len() as u64, Ordering::Relaxed);
                return replay_server(stream, &flow[1..], scenario, counters).await;
            }
        }
    }
}

pub(crate) async fn serve(
    stream: &mut (impl IoStream + ?Sized),
    payloads: &PreparedPayloads,
    counters: &Counters,
) -> anyhow::Result<()> {
    read_exact_count(stream, payloads.request.len()).await?;
    counters
        .rx
        .fetch_add(payloads.request.len() as u64, Ordering::Relaxed);
    stream.write_all(&payloads.response).await?;
    counters
        .tx
        .fetch_add(payloads.response.len() as u64, Ordering::Relaxed);
    Ok(())
}

async fn read_exact_count(
    stream: &mut (impl AsyncRead + Unpin + ?Sized),
    mut remaining: usize,
) -> anyhow::Result<()> {
    let mut buffer = [0_u8; 65_536];
    while remaining > 0 {
        let size = remaining.min(buffer.len());
        stream.read_exact(&mut buffer[..size]).await?;
        remaining -= size;
    }
    Ok(())
}
