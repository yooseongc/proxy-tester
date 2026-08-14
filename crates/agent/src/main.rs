use anyhow::{Context, bail};
use chrono::Utc;
use clap::Parser;
use futures::StreamExt;
use proxy_tester_domain::{MetricsSnapshot, PayloadKind, PayloadMode, Scenario};
use proxy_tester_proto::network_draft_from_wire;
use proxy_tester_proto::v1::{
    AgentEvent, AgentHello, AgentMessage, AgentRole, AgentStatus, CommandAck, Heartbeat, Telemetry,
    agent_control_client::AgentControlClient, agent_message, control_message,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};
use tonic::Request;
use tracing::{error, info, warn};
use uuid::Uuid;
mod artifact;
mod connector;
mod network;
mod payload;
mod replay;
mod runner;
mod telemetry;
mod tls;
mod workload;
use artifact::accept_chunk as accept_artifact_chunk;
use network::{NetworkManager, NetworkPlan};
use payload::{CompletedArtifact, PreparedPayloads};
use replay::{ReplayPlan, ReplayRole};
use telemetry::Counters;

#[derive(Parser, Debug)]
struct Args {
    #[arg(
        long,
        env = "PROXY_TESTER_CONTROL",
        default_value = "http://control:50051"
    )]
    control: String,
    #[arg(long, env = "PROXY_TESTER_NODE_ID")]
    node_id: Option<String>,
    #[arg(
        long,
        env = "PROXY_TESTER_NETWORK_JOURNAL",
        default_value = "/var/lib/proxy-tester/network-state.json"
    )]
    network_journal: std::path::PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Client,
    Server,
}
impl Role {
    fn proto(self) -> i32 {
        match self {
            Self::Client => AgentRole::Client as i32,
            Self::Server => AgentRole::Server as i32,
        }
    }
    fn from_proto(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Client),
            2 => Some(Self::Server),
            _ => None,
        }
    }
    fn replay(self) -> ReplayRole {
        match self {
            Self::Client => ReplayRole::Client,
            Self::Server => ReplayRole::Server,
        }
    }
}

#[derive(Clone)]
struct Prepared {
    run_id: Uuid,
    scenario: Scenario,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("proxy_agent=info".parse()?),
        )
        .init();
    let args = Args::parse();
    let id = args
        .node_id
        .clone()
        .context("PROXY_TESTER_NODE_ID is required")?;
    let network = NetworkManager::new(args.network_journal.clone());
    if let Err(error) = network.recover().await {
        error!(%error,"startup network recovery failed");
    }
    let instance_id = Uuid::new_v4().to_string();
    loop {
        match run_connection(&args.control, &id, &instance_id, network.clone()).await {
            Ok(()) => warn!("control stream ended"),
            Err(e) => error!(%e,"control connection failed"),
        };
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_connection(
    control: &str,
    id: &str,
    instance_id: &str,
    network: NetworkManager,
) -> anyhow::Result<()> {
    let mut client = AgentControlClient::connect(control.to_owned()).await?;
    let (tx, rx) = mpsc::channel(128);
    let inventory = network.inventory().await.unwrap_or_default();
    tx.send(AgentMessage {
        body: Some(agent_message::Body::Hello(AgentHello {
            node_id: id.into(),
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION").into(),
            instance_id: instance_id.into(),
            inventory: Some(inventory.into()),
        })),
    })
    .await?;
    let mut commands = client
        .agent_stream(Request::new(tokio_stream(rx)))
        .await?
        .into_inner();
    let heartbeat = tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if heartbeat
                .send(AgentMessage {
                    body: Some(agent_message::Body::Heartbeat(Heartbeat {
                        unix_ms: Utc::now().timestamp_millis(),
                    })),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let prepared: Arc<Mutex<HashMap<i32, Prepared>>> = Default::default();
    let mut incoming_artifacts = HashMap::new();
    let mut completed_artifacts = HashMap::new();
    type ActiveRun = (Uuid, Arc<AtomicBool>, Arc<AtomicBool>);
    let active_run: Arc<Mutex<HashMap<i32, ActiveRun>>> = Default::default();
    tx.send(AgentMessage {
        body: Some(agent_message::Body::Status(AgentStatus {
            active_run_id: String::new(),
            phase: "idle".into(),
        })),
    })
    .await?;
    let mut completed_commands = HashSet::new();
    let mut command_order = VecDeque::new();
    while let Some(cmd) = commands.next().await {
        match cmd?.body {
            Some(control_message::Body::Prepare(p)) => {
                let endpoint_role =
                    Role::from_proto(p.endpoint_role).context("invalid endpoint role")?;
                if command_seen(&completed_commands, &p.command_id) {
                    send_ack(&tx, &p.command_id, &p.run_id, "prepare", true, "").await?;
                    continue;
                }
                if !incoming_artifacts.is_empty() {
                    bail!("PrepareRun arrived before artifact transfer completed");
                }
                let mut scenario: Scenario = serde_json::from_str(&p.scenario_json)?;
                scenario.runtime_target_addr =
                    (!p.target_addr.is_empty()).then_some(p.target_addr.clone());
                scenario.runtime_interface =
                    (!p.interface_name.is_empty()).then_some(p.interface_name.clone());
                scenario.runtime_namespace =
                    (!p.namespace.is_empty()).then_some(p.namespace.clone());
                scenario.runtime_source_ips = p
                    .source_ips
                    .iter()
                    .map(|value| value.parse())
                    .collect::<Result<Vec<_>, _>>()?;
                scenario.validate()?;
                let payloads = Arc::new(PreparedPayloads::new(&scenario, &completed_artifacts)?);
                let replay = if scenario.payload_mode == PayloadMode::CaptureReplay {
                    let id = scenario
                        .capture_artifact_id
                        .context("capture artifact ID")?;
                    let path = completed_artifacts
                        .get(&id)
                        .and_then(|artifact| match artifact {
                            CompletedArtifact::Capture(path) => Some(path),
                            _ => None,
                        })
                        .with_context(|| format!("capture artifact {id} was not transferred"))?;
                    let bytes = tokio::fs::read(path).await?;
                    let replay =
                        ReplayPlan::from_capture(&bytes, &scenario, endpoint_role.replay())?;
                    Some(Arc::new(replay))
                } else {
                    None
                };
                prepared.lock().await.insert(
                    p.endpoint_role,
                    Prepared {
                        run_id: Uuid::parse_str(&p.run_id)?,
                        scenario,
                        payloads,
                        replay,
                    },
                );
                info!(run=%p.run_id,"prepared");
                remember_command(&mut completed_commands, &mut command_order, &p.command_id);
                send_ack(&tx, &p.command_id, &p.run_id, "prepare", true, "").await?;
            }
            Some(control_message::Body::ArtifactChunk(chunk)) => {
                accept_artifact_chunk(&mut incoming_artifacts, &mut completed_artifacts, chunk)
                    .await?;
            }
            Some(control_message::Body::Start(s)) => {
                let endpoint_role =
                    Role::from_proto(s.endpoint_role).context("invalid endpoint role")?;
                if command_seen(&completed_commands, &s.command_id) {
                    send_ack(&tx, &s.command_id, &s.run_id, "start", true, "").await?;
                    continue;
                }
                let Some(job) = prepared.lock().await.get(&s.endpoint_role).cloned() else {
                    send_ack(
                        &tx,
                        &s.command_id,
                        &s.run_id,
                        "start",
                        false,
                        "run was not prepared",
                    )
                    .await?;
                    continue;
                };
                let mut active = active_run.lock().await;
                if active
                    .values()
                    .any(|(_, running, _)| running.load(Ordering::SeqCst))
                    && active.contains_key(&s.endpoint_role)
                {
                    send_ack(
                        &tx,
                        &s.command_id,
                        &s.run_id,
                        "start",
                        false,
                        "another run is active",
                    )
                    .await?;
                    continue;
                }
                let running = Arc::new(AtomicBool::new(true));
                let paused = Arc::new(AtomicBool::new(false));
                active.insert(
                    s.endpoint_role,
                    (job.run_id, running.clone(), paused.clone()),
                );
                drop(active);
                remember_command(&mut completed_commands, &mut command_order, &s.command_id);
                send_ack(&tx, &s.command_id, &s.run_id, "start", true, "").await?;
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    let delay = (s.start_unix_ms - Utc::now().timestamp_millis()).max(0) as u64;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let result = run_job_isolated(
                        job.clone(),
                        endpoint_role,
                        tx2.clone(),
                        running.clone(),
                        paused,
                    )
                    .await;
                    if let Err(e) = result {
                        let _ =
                            send_event(&tx2, job.run_id, endpoint_role, "error", &e.to_string())
                                .await;
                    }
                    running.store(false, Ordering::SeqCst);
                });
            }
            Some(control_message::Body::Stop(command)) => {
                if command_seen(&completed_commands, &command.command_id) {
                    send_ack(&tx, &command.command_id, &command.run_id, "stop", true, "").await?;
                    continue;
                }
                if let Ok(run_id) = Uuid::parse_str(&command.run_id) {
                    for (endpoint_role, (active_id, running, paused)) in
                        active_run.lock().await.iter()
                    {
                        if *active_id == run_id
                            && (command.endpoint_role == 0
                                || command.endpoint_role == *endpoint_role)
                        {
                            running.store(false, Ordering::SeqCst);
                            paused.store(false, Ordering::SeqCst);
                        }
                    }
                }
                remember_command(
                    &mut completed_commands,
                    &mut command_order,
                    &command.command_id,
                );
                send_ack(&tx, &command.command_id, &command.run_id, "stop", true, "").await?;
            }
            Some(control_message::Body::SetPaused(command)) => {
                if command_seen(&completed_commands, &command.command_id) {
                    send_ack(&tx, &command.command_id, &command.run_id, "pause", true, "").await?;
                    continue;
                }
                if let Ok(run_id) = Uuid::parse_str(&command.run_id) {
                    for (endpoint_role, (active_id, running, paused)) in
                        active_run.lock().await.iter()
                    {
                        if *active_id == run_id
                            && (command.endpoint_role == 0
                                || command.endpoint_role == *endpoint_role)
                            && running.load(Ordering::SeqCst)
                        {
                            paused.store(command.paused, Ordering::SeqCst);
                        }
                    }
                }
                remember_command(
                    &mut completed_commands,
                    &mut command_order,
                    &command.command_id,
                );
                send_ack(&tx, &command.command_id, &command.run_id, "pause", true, "").await?;
            }
            Some(control_message::Body::Network(command)) => {
                use proxy_tester_proto::v1::network_command::Action;
                let mut result_plan = None;
                let (stage, outcome) = match command.action {
                    Some(Action::Plan(request)) => {
                        let draft =
                            network_draft_from_wire(request.draft.context("network draft")?)?;
                        let inventory = network.inventory().await?;
                        let outcome = match network.plan(
                            id,
                            &request.profile_revision_id,
                            &draft,
                            &inventory,
                        ) {
                            Ok(plan) => {
                                result_plan = Some(plan.into());
                                Ok(())
                            }
                            Err(error) => Err(error),
                        };
                        ("plan", outcome)
                    }
                    Some(Action::Stage(request)) => {
                        let plan: NetworkPlan = request.plan.context("network plan")?.into();
                        let result = network
                            .apply(&command.operation_id, &plan, command.lease_expires_unix_ms)
                            .await;
                        if result.is_ok() {
                            let manager = network.clone();
                            let operation = command.operation_id.clone();
                            let expires = command.lease_expires_unix_ms;
                            tokio::spawn(async move {
                                manager.enforce_lease(operation, expires).await;
                            });
                        }
                        ("stage", result)
                    }
                    Some(Action::Commit(_)) => ("commit", network.commit().await),
                    Some(Action::Rollback(_)) => ("rollback", network.recover().await),
                    Some(Action::Teardown(_)) => ("teardown", network.recover().await),
                    Some(Action::Reconcile(_)) => ("reconcile", network.recover().await),
                    None => (
                        "unknown",
                        Err(anyhow::anyhow!("network action is required")),
                    ),
                };
                let (ok, error) = match outcome {
                    Ok(()) => (true, String::new()),
                    Err(error) => (false, error.to_string()),
                };
                tx.send(AgentMessage {
                    body: Some(agent_message::Body::NetworkProgress(
                        proxy_tester_proto::v1::NetworkProgress {
                            command_id: command.command_id.clone(),
                            operation_id: command.operation_id.clone(),
                            stage: stage.into(),
                            ok,
                            plan: result_plan,
                            error: error.clone(),
                        },
                    )),
                })
                .await?;
                send_ack(&tx, &command.command_id, "", stage, ok, &error).await?;
            }
            None => {}
        }
    }
    Ok(())
}

fn command_seen(completed: &HashSet<String>, command_id: &str) -> bool {
    !command_id.is_empty() && completed.contains(command_id)
}

fn remember_command(
    completed: &mut HashSet<String>,
    order: &mut VecDeque<String>,
    command_id: &str,
) {
    if command_id.is_empty() || !completed.insert(command_id.to_owned()) {
        return;
    }
    order.push_back(command_id.to_owned());
    if order.len() > 256
        && let Some(oldest) = order.pop_front()
    {
        completed.remove(&oldest);
    }
}

async fn send_ack(
    tx: &mpsc::Sender<AgentMessage>,
    command_id: &str,
    run_id: &str,
    phase: &str,
    ok: bool,
    error: &str,
) -> anyhow::Result<()> {
    tx.send(AgentMessage {
        body: Some(agent_message::Body::CommandAck(CommandAck {
            command_id: command_id.into(),
            run_id: run_id.into(),
            phase: phase.into(),
            ok,
            error: error.into(),
        })),
    })
    .await?;
    Ok(())
}

async fn run_job(
    job: Prepared,
    role: Role,
    tx: mpsc::Sender<AgentMessage>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let counters = Arc::new(Counters::default());
    let started = Instant::now();
    let interface_task = job.scenario.runtime_interface.clone().map(|interface| {
        tokio::spawn(telemetry::monitor_interfaces(
            vec![interface],
            counters.clone(),
            running.clone(),
        ))
    });
    let metric_task = tokio::spawn(report_metrics(
        job.run_id,
        counters.clone(),
        tx.clone(),
        running.clone(),
        started,
        role,
        random_payload_hashes(&job.scenario, &job.payloads, role),
    ));
    let result = match role {
        Role::Client => {
            runner::run_client(
                &job.scenario,
                job.payloads.clone(),
                job.replay.clone(),
                counters.clone(),
                running.clone(),
                paused.clone(),
            )
            .await
        }
        Role::Server => {
            runner::run_server(
                &job.scenario,
                job.payloads.clone(),
                job.replay.clone(),
                counters.clone(),
                running.clone(),
                paused.clone(),
            )
            .await
        }
    };
    running.store(false, Ordering::SeqCst);
    let _ = metric_task.await;
    if let Some(task) = interface_task {
        let _ = task.await;
    }
    if result.is_ok() {
        // Give both endpoints one telemetry interval to flush final counters before
        // control marks the run complete after receiving both completion reports.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        send_event(&tx, job.run_id, role, "info", "run_completed").await?;
    }
    result
}

async fn run_job_isolated(
    job: Prepared,
    role: Role,
    tx: mpsc::Sender<AgentMessage>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let Some(namespace) = job
        .scenario
        .runtime_namespace
        .clone()
        .filter(|v| !v.is_empty())
    else {
        return run_job(job, role, tx, running, paused).await;
    };
    #[cfg(target_os = "linux")]
    {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name(format!("endpoint-{namespace}"))
            .spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    use std::os::fd::AsRawFd;
                    let namespace = std::fs::File::open(format!("/var/run/netns/{namespace}"))?;
                    if unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(run_job(job, role, tx, running, paused))
                })();
                let _ = result_tx.send(result);
            })?;
        return result_rx.await.context("endpoint worker exited")?;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = namespace;
        run_job(job, role, tx, running, paused).await
    }
}

async fn report_metrics(
    run_id: Uuid,
    c: Arc<Counters>,
    tx: mpsc::Sender<AgentMessage>,
    running: Arc<AtomicBool>,
    started: Instant,
    role: Role,
    random_payload_hashes: (Option<String>, Option<String>),
) {
    let mut prev_est = 0;
    let mut prev_tx = 0;
    let mut prev_rx = 0;
    let mut prev_tr = 0;
    let mut prev_packets_tx = 0;
    let mut prev_packets_rx = 0;
    let mut prev_wire_tx = 0;
    let mut prev_wire_rx = 0;
    let mut prev_retransmissions = 0;
    let mut last_report = Instant::now();
    while running.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let report_interval = last_report.elapsed().as_secs_f64().max(0.001);
        last_report = Instant::now();
        let est = c.established.load(Ordering::Relaxed);
        let txb = c.tx.load(Ordering::Relaxed);
        let rxb = c.rx.load(Ordering::Relaxed);
        let tr = c.transactions.load(Ordering::Relaxed);
        let packets_tx = c.packets_tx.load(Ordering::Relaxed);
        let packets_rx = c.packets_rx.load(Ordering::Relaxed);
        let wire_tx = c.wire_tx_bytes.load(Ordering::Relaxed);
        let wire_rx = c.wire_rx_bytes.load(Ordering::Relaxed);
        let retransmissions = c.tcp_retransmissions.load(Ordering::Relaxed);
        // Latency percentiles describe this telemetry interval. Draining also keeps
        // long-running tests from retaining every latency sample indefinitely.
        let tcp_latencies = {
            let mut samples = c.tcp_connect_latencies_us.lock().await;
            std::mem::take(&mut *samples)
        };
        let http_latencies = {
            let mut samples = c.http_latencies_us.lock().await;
            std::mem::take(&mut *samples)
        };
        let tcp_p50 = percentile_ms(&tcp_latencies, 0.50);
        let tcp_p95 = percentile_ms(&tcp_latencies, 0.95);
        let tcp_p99 = percentile_ms(&tcp_latencies, 0.99);
        let http_p50 = percentile_ms(&http_latencies, 0.50);
        let http_p95 = percentile_ms(&http_latencies, 0.95);
        let http_p99 = percentile_ms(&http_latencies, 0.99);
        let (
            active_connections,
            active_connections_avg,
            active_connections_min,
            active_connections_max,
        ) = c.active_snapshot();
        let m = MetricsSnapshot {
            run_id,
            unix_ms: Utc::now().timestamp_millis(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            load_stage_index: c.load_stage_index.load(Ordering::Relaxed),
            desired_virtual_clients: c.desired_virtual_clients.load(Ordering::Relaxed),
            included_in_results: c.included_in_results.load(Ordering::Relaxed),
            connections_attempted: c.attempted.load(Ordering::Relaxed),
            connections_established: est,
            connections_failed: c.failed.load(Ordering::Relaxed),
            active_connections,
            active_connections_avg,
            active_connections_min,
            active_connections_max,
            transactions: tr,
            transaction_errors: c.transaction_errors.load(Ordering::Relaxed),
            timeout_errors: c.timeout_errors.load(Ordering::Relaxed),
            reset_errors: c.reset_errors.load(Ordering::Relaxed),
            tls_handshake_errors: c.tls_handshake_errors.load(Ordering::Relaxed),
            proxy_connect_errors: c.proxy_connect_errors.load(Ordering::Relaxed),
            http_error_responses: c.http_error_responses.load(Ordering::Relaxed),
            request_random_sha256: random_payload_hashes.0.clone(),
            response_random_sha256: random_payload_hashes.1.clone(),
            bytes_tx: txb,
            bytes_rx: rxb,
            packets_tx,
            packets_rx,
            cps: (est - prev_est) as f64 / report_interval,
            tps: (tr - prev_tr) as f64 / report_interval,
            tx_bps: (txb - prev_tx) as f64 * 8.0 / report_interval,
            rx_bps: (rxb - prev_rx) as f64 * 8.0 / report_interval,
            tx_pps: (packets_tx - prev_packets_tx) as f64 / report_interval,
            rx_pps: (packets_rx - prev_packets_rx) as f64 / report_interval,
            latency_p50_ms: if http_p50 > 0.0 { http_p50 } else { tcp_p50 },
            latency_p95_ms: if http_p95 > 0.0 { http_p95 } else { tcp_p95 },
            latency_p99_ms: if http_p99 > 0.0 { http_p99 } else { tcp_p99 },
            tcp_connect_latency_p50_ms: tcp_p50,
            tcp_connect_latency_p95_ms: tcp_p95,
            tcp_connect_latency_p99_ms: tcp_p99,
            http_latency_p50_ms: http_p50,
            http_latency_p95_ms: http_p95,
            http_latency_p99_ms: http_p99,
            wire_tx_bytes: wire_tx,
            wire_rx_bytes: wire_rx,
            wire_tx_bps: wire_tx.saturating_sub(prev_wire_tx) as f64 * 8.0 / report_interval,
            wire_rx_bps: wire_rx.saturating_sub(prev_wire_rx) as f64 * 8.0 / report_interval,
            wire_tx_pps: (packets_tx - prev_packets_tx) as f64 / report_interval,
            wire_rx_pps: (packets_rx - prev_packets_rx) as f64 / report_interval,
            tcp_retransmissions: retransmissions,
            tcp_retransmissions_per_sec: (retransmissions - prev_retransmissions) as f64
                / report_interval,
        };
        prev_est = est;
        prev_tx = txb;
        prev_rx = rxb;
        prev_tr = tr;
        prev_packets_tx = packets_tx;
        prev_packets_rx = packets_rx;
        prev_wire_tx = wire_tx;
        prev_wire_rx = wire_rx;
        prev_retransmissions = retransmissions;
        let metrics_json = match serde_json::to_string(&m) {
            Ok(value) => value,
            Err(error) => {
                warn!(%error, %run_id, "failed to serialize telemetry");
                break;
            }
        };
        if tx
            .send(AgentMessage {
                body: Some(agent_message::Body::Telemetry(Telemetry {
                    run_id: run_id.to_string(),
                    metrics_json,
                    endpoint_role: role.proto(),
                })),
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn random_payload_hashes(
    scenario: &Scenario,
    payloads: &PreparedPayloads,
    role: Role,
) -> (Option<String>, Option<String>) {
    let request = (role == Role::Client && scenario.request_payload.kind == PayloadKind::Random)
        .then(|| format!("{:x}", Sha256::digest(&payloads.request)));
    let response = (role == Role::Server && scenario.response_payload.kind == PayloadKind::Random)
        .then(|| format!("{:x}", Sha256::digest(&payloads.response)));
    (request, response)
}

fn percentile_ms(samples: &[u64], percentile: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f64 * percentile) as usize] as f64 / 1000.0
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default, clippy::items_after_test_module)]
mod tests {
    use super::*;
    use proxy_tester_capture::{Direction, ReplayTurn};
    use proxy_tester_domain::Protocol;
    use proxy_tester_proto::v1::ArtifactChunk;
    use workload::tcp::{replay_client, replay_server_detect};

    #[test]
    fn command_deduplication_is_bounded_and_idempotent() {
        let mut completed = HashSet::new();
        let mut order = VecDeque::new();
        remember_command(&mut completed, &mut order, "first");
        remember_command(&mut completed, &mut order, "first");
        assert!(command_seen(&completed, "first"));
        assert_eq!(order.len(), 1);
        for index in 0..256 {
            remember_command(&mut completed, &mut order, &format!("command-{index}"));
        }
        assert_eq!(completed.len(), 256);
        assert!(!command_seen(&completed, "first"));
        assert!(!command_seen(&completed, ""));
    }

    fn chunk(
        id: Uuid,
        offset: u64,
        data: &[u8],
        total: u64,
        sha256: &str,
        eof: bool,
    ) -> ArtifactChunk {
        ArtifactChunk {
            artifact_id: id.to_string(),
            offset,
            data: data.to_vec(),
            total_size: total,
            sha256: sha256.into(),
            eof,
            artifact_kind: "payload".into(),
        }
    }

    #[tokio::test]
    async fn artifact_chunks_are_assembled_and_verified() {
        let id = Uuid::new_v4();
        let bytes = b"request payload";
        let sha = format!("{:x}", Sha256::digest(bytes));
        let mut incoming = HashMap::new();
        let mut completed = HashMap::new();
        accept_artifact_chunk(
            &mut incoming,
            &mut completed,
            chunk(id, 0, &bytes[..4], bytes.len() as u64, &sha, false),
        )
        .await
        .unwrap();
        accept_artifact_chunk(
            &mut incoming,
            &mut completed,
            chunk(id, 4, &bytes[4..], bytes.len() as u64, &sha, true),
        )
        .await
        .unwrap();
        let CompletedArtifact::Payload(completed) = &completed[&id] else {
            panic!("payload expected")
        };
        assert_eq!(completed.as_ref(), bytes);
    }

    #[tokio::test]
    async fn artifact_chunk_rejects_bad_offset_and_digest() {
        let id = Uuid::new_v4();
        let mut incoming = HashMap::new();
        let mut completed = HashMap::new();
        assert!(
            accept_artifact_chunk(
                &mut incoming,
                &mut completed,
                chunk(id, 1, b"x", 1, "bad", true)
            )
            .await
            .is_err()
        );
        incoming.clear();
        assert!(
            accept_artifact_chunk(
                &mut incoming,
                &mut completed,
                chunk(id, 0, b"x", 1, "bad", true)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn capture_artifact_stays_on_disk_until_prepare() {
        let id = Uuid::new_v4();
        let bytes = b"pcap bytes";
        let sha = format!("{:x}", Sha256::digest(bytes));
        let mut message = chunk(id, 0, bytes, bytes.len() as u64, &sha, true);
        message.artifact_kind = "pcap".into();
        let mut incoming = HashMap::new();
        let mut completed = HashMap::new();
        accept_artifact_chunk(&mut incoming, &mut completed, message)
            .await
            .unwrap();
        let CompletedArtifact::Capture(path) = &completed[&id] else {
            panic!("capture file expected")
        };
        assert_eq!(tokio::fs::read(path).await.unwrap(), bytes);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn replay_peers_follow_bidirectional_turns() {
        let turns = vec![
            ReplayTurn {
                direction: Direction::ClientToServer,
                payload: b"one".to_vec(),
            },
            ReplayTurn {
                direction: Direction::ServerToClient,
                payload: b"two".to_vec(),
            },
            ReplayTurn {
                direction: Direction::ClientToServer,
                payload: b"three".to_vec(),
            },
            ReplayTurn {
                direction: Direction::ServerToClient,
                payload: b"four".to_vec(),
            },
        ];
        let (mut client_stream, mut server_stream) = tokio::io::duplex(128);
        let scenario = Scenario::default();
        let client_counters = Counters::default();
        let server_counters = Counters::default();
        let (client, server) = tokio::join!(
            replay_client(&mut client_stream, &turns, &scenario, &client_counters),
            workload::tcp::replay_server(&mut server_stream, &turns, &scenario, &server_counters,),
        );
        client.unwrap();
        server.unwrap();
        assert_eq!(client_counters.tx.load(Ordering::Relaxed), 8);
        assert_eq!(client_counters.rx.load(Ordering::Relaxed), 7);
        assert_eq!(server_counters.rx.load(Ordering::Relaxed), 8);
        assert_eq!(server_counters.tx.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn replay_plan_is_built_from_scapy_fixture() {
        let capture = include_bytes!("../../../tests/pcap/fixtures/plaintext_flows.pcap");
        let plan =
            ReplayPlan::from_capture(capture, &Scenario::default(), ReplayRole::Client).unwrap();
        assert_eq!(plan.flows.len(), 2);
        assert_eq!(plan.flows[0].len(), 2);
        assert_eq!(plan.flows[1].len(), 2);
    }

    #[test]
    fn http_replay_plan_filters_flows_and_rewrites_endpoint() {
        let capture = include_bytes!("../../../tests/pcap/fixtures/plaintext_flows.pcap");
        let mut scenario = Scenario::default();
        scenario.protocol = Protocol::Http1;
        scenario.path = proxy_tester_domain::ScenarioPath::ExplicitProxy {
            client_node_id: "client".into(),
            client_bind_ip: "192.0.2.10".parse().unwrap(),
            server_node_id: "server".into(),
            server_listen_ip: "192.0.2.20".parse().unwrap(),
            server_port: 8080,
            proxy_addr: "proxy.test:3128".into(),
        };
        scenario.request.host = "origin.test".into();
        let plan = ReplayPlan::from_capture(capture, &scenario, ReplayRole::Client).unwrap();
        assert_eq!(plan.flows.len(), 1);
        assert_eq!(plan.flows[0].len(), 2);
        assert!(
            plan.flows[0][0]
                .payload
                .starts_with(b"POST http://192.0.2.20:8080/scan HTTP/1.1\r\n")
        );
        assert!(
            plan.flows[0][0]
                .payload
                .windows(b"Host: origin.test\r\n".len())
                .any(|window| window == b"Host: origin.test\r\n")
        );
        assert!(plan.flows[0][0].payload.ends_with(b"DLP-SECRET"));
        assert_eq!(
            plan.flows[0][1].payload,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK"
        );
        let server_plan = ReplayPlan::from_capture(capture, &scenario, ReplayRole::Server).unwrap();
        assert!(
            server_plan.flows[0][0]
                .payload
                .starts_with(b"POST /scan HTTP/1.1\r\n")
        );
    }

    #[tokio::test]
    async fn responder_detects_flow_from_first_client_turn() {
        let first: Arc<[ReplayTurn]> = vec![
            ReplayTurn {
                direction: Direction::ClientToServer,
                payload: b"alpha".to_vec(),
            },
            ReplayTurn {
                direction: Direction::ServerToClient,
                payload: b"A".to_vec(),
            },
        ]
        .into();
        let second: Arc<[ReplayTurn]> = vec![
            ReplayTurn {
                direction: Direction::ClientToServer,
                payload: b"beta".to_vec(),
            },
            ReplayTurn {
                direction: Direction::ServerToClient,
                payload: b"B".to_vec(),
            },
        ]
        .into();
        let plan = ReplayPlan {
            flows: vec![first, second.clone()].into(),
        };
        let (mut client_stream, mut server_stream) = tokio::io::duplex(128);
        let scenario = Scenario::default();
        let client_counters = Counters::default();
        let server_counters = Counters::default();
        let (client, server) = tokio::join!(
            replay_client(&mut client_stream, &second, &scenario, &client_counters),
            replay_server_detect(&mut server_stream, &plan.flows, &scenario, &server_counters,),
        );
        client.unwrap();
        server.unwrap();
        assert_eq!(client_counters.rx.load(Ordering::Relaxed), 1);
        assert_eq!(server_counters.rx.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn failures_are_classified_without_losing_aggregate_errors() {
        let counters = Counters::default();
        counters.record_failure(&anyhow::anyhow!(
            "HTTP CONNECT failed: CONNECT rejected with response HTTP/1.1 403"
        ));
        counters.record_failure(&anyhow::anyhow!(
            "TLS handshake failed: invalid peer certificate"
        ));
        counters.record_failure(&anyhow::anyhow!("deadline has elapsed"));
        counters.record_failure(&anyhow::anyhow!("connection reset by peer"));
        counters.record_failure(&anyhow::anyhow!("HTTP error response 451"));
        assert_eq!(counters.proxy_connect_errors.load(Ordering::Relaxed), 1);
        assert_eq!(counters.tls_handshake_errors.load(Ordering::Relaxed), 1);
        assert_eq!(counters.timeout_errors.load(Ordering::Relaxed), 1);
        assert_eq!(counters.reset_errors.load(Ordering::Relaxed), 1);
        assert_eq!(counters.http_error_responses.load(Ordering::Relaxed), 1);
    }
}
async fn send_event(
    tx: &mpsc::Sender<AgentMessage>,
    run_id: Uuid,
    role: Role,
    level: &str,
    message: &str,
) -> anyhow::Result<()> {
    tx.send(AgentMessage {
        body: Some(agent_message::Body::Event(AgentEvent {
            run_id: run_id.to_string(),
            level: level.into(),
            message: message.into(),
            endpoint_role: role.proto(),
        })),
    })
    .await?;
    Ok(())
}
fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into())
}
fn tokio_stream<T: Send + 'static>(mut rx: mpsc::Receiver<T>) -> impl futures::Stream<Item = T> {
    async_stream::stream! {while let Some(v)=rx.recv().await{yield v;}}
}
