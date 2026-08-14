use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use proxy_tester_capture::ReplayTurn;
use proxy_tester_domain::{Protocol, Scenario};
use tokio::net::TcpListener;
use tracing::info;

use crate::{
    connector::{self, IoStream},
    payload::PreparedPayloads,
    replay::ReplayPlan,
    telemetry::Counters,
    tls, workload,
    workload::{WorkerGate, tcp::replay_server_detect},
};

pub(crate) async fn run_client(
    scenario: &Scenario,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
    counters: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    run_client_concurrency(scenario, payloads, replay, counters, running, paused).await
}

async fn run_client_concurrency(
    scenario: &Scenario,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
    counters: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let generating = Arc::new(AtomicBool::new(true));
    let desired_clients = Arc::new(AtomicU32::new(0));
    let next_flow = Arc::new(AtomicU64::new(0));
    let scenario = Arc::new(scenario.clone());
    let worker_count = scenario.maximum_virtual_clients();
    let mut workers = Vec::with_capacity(worker_count as usize);
    for worker_index in 0..worker_count {
        let scenario = scenario.clone();
        let counters = counters.clone();
        let running = running.clone();
        let paused = paused.clone();
        let generating = generating.clone();
        let desired_clients = desired_clients.clone();
        let payloads = payloads.clone();
        let replay = replay.clone();
        let next_flow = next_flow.clone();
        workers.push(tokio::spawn(async move {
            while running.load(Ordering::Relaxed) && generating.load(Ordering::Relaxed) {
                if paused.load(Ordering::Relaxed)
                    || worker_index >= desired_clients.load(Ordering::Relaxed)
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                execute_connection(
                    &scenario,
                    &payloads,
                    replay.as_deref(),
                    &next_flow,
                    &counters,
                    &running,
                    &paused,
                    &generating,
                    &desired_clients,
                    worker_index,
                )
                .await;
            }
        }));
    }
    let duration = Duration::from_secs(scenario.effective_duration_secs());
    let mut active_elapsed = Duration::ZERO;
    let mut last_tick = Instant::now();
    while active_elapsed < duration && running.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = Instant::now();
        if paused.load(Ordering::Relaxed) {
            last_tick = now;
        } else {
            active_elapsed += now.saturating_duration_since(last_tick);
            last_tick = now;
            let (stage_index, desired, included) =
                scenario.load_at(active_elapsed.as_millis() as u64);
            desired_clients.store(desired, Ordering::Relaxed);
            counters
                .load_stage_index
                .store(stage_index as u32, Ordering::Relaxed);
            counters
                .desired_virtual_clients
                .store(desired, Ordering::Relaxed);
            counters
                .included_in_results
                .store(included, Ordering::Relaxed);
        }
    }
    generating.store(false, Ordering::SeqCst);
    for worker in workers {
        let _ = worker.await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_connection(
    scenario: &Scenario,
    payloads: &PreparedPayloads,
    replay: Option<&ReplayPlan>,
    next_flow: &AtomicU64,
    counters: &Counters,
    running: &AtomicBool,
    paused: &AtomicBool,
    generating: &AtomicBool,
    desired_clients: &AtomicU32,
    worker_index: u32,
) {
    let connection_index = counters.attempted.fetch_add(1, Ordering::Relaxed);
    let gate = WorkerGate {
        running,
        paused,
        generating,
        desired_clients,
        worker_index,
    };
    let flow = replay.map(|plan| {
        let index = next_flow.fetch_add(1, Ordering::Relaxed) as usize % plan.flows.len();
        plan.flows[index].clone()
    });
    if let Err(error) = transact(
        scenario,
        payloads,
        flow.as_deref(),
        counters,
        &gate,
        connection_index,
    )
    .await
    {
        counters.failed.fetch_add(1, Ordering::Relaxed);
        counters.transaction_errors.fetch_add(1, Ordering::Relaxed);
        counters.record_failure(&error);
    }
}

async fn transact(
    scenario: &Scenario,
    payloads: &PreparedPayloads,
    replay_turns: Option<&[ReplayTurn]>,
    counters: &Counters,
    gate: &WorkerGate<'_>,
    connection_index: u64,
) -> anyhow::Result<()> {
    let tcp_stream = connector::connect_tcp(scenario, connection_index, counters).await?;
    let _active_connection = counters.connection_opened();
    let mut stream = connector::upgrade(tcp_stream, scenario, counters).await?;
    let result = if scenario.protocol == Protocol::Http2 {
        workload::http2::transactions(stream, scenario, payloads, replay_turns, counters, gate)
            .await
    } else if let Some(turns) = replay_turns {
        workload::tcp::replay_client(&mut *stream, turns, scenario, counters).await
    } else {
        match scenario.protocol {
            Protocol::Http1 => {
                let absolute = scenario.is_explicit_proxy() && !scenario.tls.enabled;
                workload::http1::transactions(
                    &mut *stream,
                    scenario,
                    payloads,
                    absolute,
                    counters,
                    gate,
                )
                .await
            }
            _ => workload::tcp::transaction(&mut *stream, scenario, payloads, counters).await,
        }
    };
    if result.is_ok() && !matches!(scenario.protocol, Protocol::Http1 | Protocol::Http2) {
        counters.transaction_completed();
    }
    result
}

pub(crate) async fn run_server(
    scenario: &Scenario,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
    counters: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let port = scenario.server_port();
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let tls_acceptor = scenario
        .tls
        .enabled
        .then(|| tls::acceptor(scenario))
        .transpose()?;
    info!(port, "responder listening");
    let duration = Duration::from_millis(scenario.effective_duration_secs() * 1000 + 700);
    let mut active_elapsed = Duration::ZERO;
    let mut last_tick = Instant::now();
    while active_elapsed < duration && running.load(Ordering::Relaxed) {
        if paused.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            last_tick = Instant::now();
            continue;
        }
        let now = Instant::now();
        active_elapsed += now.saturating_duration_since(last_tick);
        last_tick = now;
        let (stage_index, desired, included) = scenario.load_at(active_elapsed.as_millis() as u64);
        counters
            .load_stage_index
            .store(stage_index as u32, Ordering::Relaxed);
        counters
            .desired_virtual_clients
            .store(desired, Ordering::Relaxed);
        counters
            .included_in_results
            .store(included, Ordering::Relaxed);
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        let Ok(Ok((stream, _))) = accepted else {
            continue;
        };
        counters.connection_established();
        let scenario = scenario.clone();
        let counters = counters.clone();
        let tls_acceptor = tls_acceptor.clone();
        let payloads = payloads.clone();
        let replay = replay.clone();
        tokio::spawn(async move {
            let _active_connection = counters.connection_opened();
            let mut stream: Box<dyn IoStream> = match tls_acceptor {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(stream) => Box::new(stream),
                    Err(_) => {
                        counters.transaction_errors.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                },
                None => Box::new(stream),
            };
            let result = if scenario.protocol == Protocol::Http2 {
                let replay_response = replay
                    .as_ref()
                    .and_then(|plan| plan.flows.first())
                    .and_then(|flow| flow.get(1))
                    .map(|turn| turn.payload.clone());
                workload::http2::serve(stream, payloads, replay_response, counters.clone()).await
            } else if let Some(plan) = replay {
                replay_server_detect(&mut *stream, &plan.flows, &scenario, &counters).await
            } else if scenario.protocol == Protocol::Http1 {
                workload::http1::serve(&mut *stream, &payloads, &counters).await
            } else {
                workload::tcp::serve(&mut *stream, &payloads, &counters).await
            };
            if result.is_ok() {
                if !matches!(scenario.protocol, Protocol::Http1 | Protocol::Http2) {
                    counters.transaction_completed();
                }
            } else {
                counters.transaction_errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    Ok(())
}
