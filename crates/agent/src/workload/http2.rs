use std::{sync::Arc, sync::atomic::Ordering, time::Duration};

use anyhow::{Context, bail};
use bytes::Bytes;
use futures::{StreamExt, stream::FuturesUnordered};
use proxy_tester_capture::ReplayTurn;
use proxy_tester_domain::Scenario;

use super::WorkerGate;
use crate::{connector::IoStream, payload::PreparedPayloads, telemetry::Counters};

async fn send_body(send: &mut h2::SendStream<Bytes>, payload: &[u8]) -> anyhow::Result<()> {
    if payload.is_empty() {
        send.send_data(Bytes::new(), true)?;
        return Ok(());
    }
    let mut offset = 0;
    while offset < payload.len() {
        send.reserve_capacity((payload.len() - offset).min(16 * 1024));
        let capacity = futures::future::poll_fn(|context| send.poll_capacity(context))
            .await
            .context("HTTP/2 stream closed while waiting for flow-control capacity")??;
        let size = capacity.min(16 * 1024).min(payload.len() - offset);
        if size == 0 {
            continue;
        }
        offset += size;
        send.send_data(
            Bytes::copy_from_slice(&payload[offset - size..offset]),
            offset == payload.len(),
        )?;
    }
    Ok(())
}

async fn request(
    sender: h2::client::SendRequest<Bytes>,
    scenario: &Scenario,
    request_payload: &[u8],
    response_len: usize,
) -> anyhow::Result<(u64, u64, u64)> {
    let started = std::time::Instant::now();
    let uri = format!("https://{}{}", scenario.request.host, scenario.request.path);
    let request = http::Request::builder()
        .method(scenario.request.method.as_str())
        .uri(uri)
        .version(http::Version::HTTP_2)
        .header("x-response-bytes", response_len.to_string())
        .body(())?;
    let mut ready = sender.ready().await?;
    let (response, mut send) = ready.send_request(request, request_payload.is_empty())?;
    if !request_payload.is_empty() {
        send_body(&mut send, request_payload).await?;
    }
    let response = response.await?;
    if !response.status().is_success() {
        bail!("HTTP error response {}", response.status());
    }
    let mut body = response.into_body();
    let mut received = 0_u64;
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        received += chunk.len() as u64;
        body.flow_control().release_capacity(chunk.len())?;
    }
    Ok((
        started.elapsed().as_micros() as u64,
        request_payload.len() as u64,
        received,
    ))
}

pub(crate) async fn transactions(
    stream: Box<dyn IoStream>,
    scenario: &Scenario,
    payloads: &PreparedPayloads,
    replay_turns: Option<&[ReplayTurn]>,
    counters: &Counters,
    gate: &WorkerGate<'_>,
) -> anyhow::Result<()> {
    let mut builder = h2::client::Builder::new();
    builder.initial_max_send_streams(scenario.http2.max_concurrent_streams as usize);
    let (sender, connection) = builder.handshake(stream).await?;
    let driver = tokio::spawn(connection);
    let maximum = scenario.request.transactions_per_connection as u64;
    let concurrency = scenario.http2.max_concurrent_streams as usize;
    let mut launched = 0_u64;
    let mut pending = FuturesUnordered::new();
    while (gate.enabled() && (maximum == 0 || launched < maximum)) || !pending.is_empty() {
        while gate.enabled() && pending.len() < concurrency && (maximum == 0 || launched < maximum)
        {
            let request_payload = replay_turns
                .and_then(|turns| turns.get((launched as usize * 2) % turns.len()))
                .map(|turn| turn.payload.as_slice())
                .unwrap_or(&payloads.request);
            let response_len = replay_turns
                .and_then(|turns| turns.get((launched as usize * 2 + 1) % turns.len()))
                .map(|turn| turn.payload.len())
                .unwrap_or(payloads.response.len());
            pending.push(request(
                sender.clone(),
                scenario,
                request_payload,
                response_len,
            ));
            launched += 1;
        }
        let Some(result) = pending.next().await else {
            break;
        };
        let (latency, sent, received) = result?;
        counters.tx.fetch_add(sent, Ordering::Relaxed);
        counters.rx.fetch_add(received, Ordering::Relaxed);
        counters.transaction_completed();
        counters.http_latencies_us.lock().await.push(latency);
        if scenario.request.think_time_ms > 0 {
            tokio::time::sleep(Duration::from_millis(scenario.request.think_time_ms)).await;
        }
    }
    drop(sender);
    driver.abort();
    let _ = driver.await;
    Ok(())
}

pub(crate) async fn serve(
    stream: Box<dyn IoStream>,
    payloads: Arc<PreparedPayloads>,
    replay_response: Option<Vec<u8>>,
    counters: Arc<Counters>,
) -> anyhow::Result<()> {
    let mut connection = h2::server::handshake(stream).await?;
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(result) = connection.accept().await {
        let (request, mut respond) = result?;
        let payloads = payloads.clone();
        let counters = counters.clone();
        let replay_response = replay_response.clone();
        tasks.spawn(async move {
            let outcome: anyhow::Result<()> = async {
                let response_len = request
                    .headers()
                    .get("x-response-bytes")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(payloads.response.len());
                let mut request_body = request.into_body();
                let mut received = 0_u64;
                while let Some(chunk) = request_body.data().await {
                    let chunk = chunk?;
                    received += chunk.len() as u64;
                    request_body.flow_control().release_capacity(chunk.len())?;
                }
                counters.rx.fetch_add(received, Ordering::Relaxed);
                let response = http::Response::builder()
                    .status(200)
                    .version(http::Version::HTTP_2)
                    .header("content-length", response_len.to_string())
                    .body(())?;
                let mut send = respond.send_response(response, response_len == 0)?;
                if response_len > 0 {
                    if let Some(body) = replay_response
                        .as_ref()
                        .filter(|body| body.len() == response_len)
                    {
                        send_body(&mut send, body).await?;
                    } else if response_len == payloads.response.len() {
                        send_body(&mut send, &payloads.response).await?;
                    } else {
                        send_body(&mut send, &vec![0; response_len]).await?;
                    }
                }
                counters
                    .tx
                    .fetch_add(response_len as u64, Ordering::Relaxed);
                counters.transaction_completed();
                Ok(())
            }
            .await;
            if outcome.is_err() {
                counters.transaction_errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result?;
    }
    Ok(())
}
