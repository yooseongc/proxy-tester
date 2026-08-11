use anyhow::{Context, bail};
use chrono::Utc;
use clap::Parser;
use futures::StreamExt;
use proxy_tester_domain::{
    MetricsSnapshot, PayloadKind, PayloadProfile, Protocol, RandomFormat, Scenario, Topology,
};
use proxy_tester_proto::v1::{
    AgentEvent, AgentHello, AgentMessage, AgentRole, Heartbeat, Telemetry,
    agent_control_client::AgentControlClient, agent_message, control_message,
};
use rand::RngCore;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use std::{
    io::Cursor,
    net::SocketAddr,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tonic::Request;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Parser, Debug)]
struct Args {
    #[arg(
        long,
        env = "PROXY_TESTER_CONTROL",
        default_value = "http://control:50051"
    )]
    control: String,
    #[arg(long, env = "PROXY_TESTER_AGENT_ID")]
    agent_id: Option<String>,
    #[arg(long, env = "PROXY_TESTER_ROLE")]
    role: Option<String>,
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
}

struct Counters {
    load_stage_index: AtomicU32,
    desired_virtual_clients: AtomicU32,
    included_in_results: AtomicBool,
    attempted: AtomicU64,
    established: AtomicU64,
    failed: AtomicU64,
    active: StdMutex<ActiveWindow>,
    transactions: AtomicU64,
    transaction_errors: AtomicU64,
    tx: AtomicU64,
    rx: AtomicU64,
    packets_tx: AtomicU64,
    packets_rx: AtomicU64,
    wire_tx_bytes: AtomicU64,
    wire_rx_bytes: AtomicU64,
    tcp_retransmissions: AtomicU64,
    tcp_connect_latencies_us: Mutex<Vec<u64>>,
    http_latencies_us: Mutex<Vec<u64>>,
}

struct ActiveWindow {
    current: u64,
    min: u64,
    max: u64,
    weighted_nanos: u128,
    window_started: Instant,
    last_change: Instant,
}

impl Default for ActiveWindow {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            current: 0,
            min: 0,
            max: 0,
            weighted_nanos: 0,
            window_started: now,
            last_change: now,
        }
    }
}

impl ActiveWindow {
    fn account_until(&mut self, now: Instant) {
        self.weighted_nanos +=
            now.saturating_duration_since(self.last_change).as_nanos() * self.current as u128;
        self.last_change = now;
    }
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            load_stage_index: Default::default(),
            desired_virtual_clients: Default::default(),
            included_in_results: Default::default(),
            attempted: Default::default(),
            established: Default::default(),
            failed: Default::default(),
            active: Default::default(),
            transactions: Default::default(),
            transaction_errors: Default::default(),
            tx: Default::default(),
            rx: Default::default(),
            packets_tx: Default::default(),
            packets_rx: Default::default(),
            wire_tx_bytes: Default::default(),
            wire_rx_bytes: Default::default(),
            tcp_retransmissions: Default::default(),
            tcp_connect_latencies_us: Default::default(),
            http_latencies_us: Default::default(),
        }
    }
}

struct ActiveConnection<'a>(&'a Counters);

impl Counters {
    fn connection_established(&self) {
        self.established.fetch_add(1, Ordering::Relaxed);
    }

    fn transaction_completed(&self) {
        self.transactions.fetch_add(1, Ordering::Relaxed);
    }

    fn connection_opened(&self) -> ActiveConnection<'_> {
        let now = Instant::now();
        let mut active = self.active.lock().unwrap();
        active.account_until(now);
        active.current += 1;
        active.min = active.min.min(active.current);
        active.max = active.max.max(active.current);
        ActiveConnection(self)
    }

    fn active_snapshot(&self) -> (u64, f64, u64, u64) {
        let now = Instant::now();
        let mut active = self.active.lock().unwrap();
        active.account_until(now);
        let elapsed = now
            .saturating_duration_since(active.window_started)
            .as_nanos()
            .max(1);
        let result = (
            active.current,
            active.weighted_nanos as f64 / elapsed as f64,
            active.min,
            active.max,
        );
        active.weighted_nanos = 0;
        active.window_started = now;
        active.min = active.current;
        active.max = active.current;
        result
    }
}

impl Drop for ActiveConnection<'_> {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut active = self.0.active.lock().unwrap();
        active.account_until(now);
        active.current = active.current.saturating_sub(1);
        active.min = active.min.min(active.current);
    }
}

#[derive(Clone)]
struct Prepared {
    run_id: Uuid,
    scenario: Scenario,
    payloads: Arc<PreparedPayloads>,
}

#[derive(Clone)]
struct PreparedPayloads {
    request: Arc<[u8]>,
    response: Arc<[u8]>,
}
impl PreparedPayloads {
    fn new(sc: &Scenario) -> anyhow::Result<Self> {
        Ok(Self {
            request: materialize(&sc.request_payload())?,
            response: materialize(&sc.response_payload())?,
        })
    }
}
fn materialize(profile: &PayloadProfile) -> anyhow::Result<Arc<[u8]>> {
    let bytes = match profile.kind {
        PayloadKind::Empty => Vec::new(),
        PayloadKind::Fixed => vec![0; profile.size_bytes],
        PayloadKind::Text => profile.text.as_bytes().to_vec(),
        PayloadKind::Random => {
            let mut data = vec![0; profile.size_bytes];
            rand::rng().fill_bytes(&mut data);
            if profile.random_format == RandomFormat::PrintableAscii {
                for byte in &mut data {
                    *byte = 0x20 + (*byte % 95);
                }
            }
            data
        }
        PayloadKind::File => bail!("file artifact transfer is unavailable on this agent"),
    };
    info!(kind=?profile.kind, bytes=bytes.len(), sha256=%format!("{:x}", Sha256::digest(&bytes)), "payload prepared");
    Ok(bytes.into())
}

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

struct WorkerGate<'a> {
    running: &'a AtomicBool,
    paused: &'a AtomicBool,
    generating: &'a AtomicBool,
    desired_clients: &'a AtomicU32,
    worker_index: u32,
}

impl WorkerGate<'_> {
    fn enabled(&self) -> bool {
        self.running.load(Ordering::Relaxed)
            && self.generating.load(Ordering::Relaxed)
            && self.worker_index < self.desired_clients.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
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
    let exe = std::env::current_exe()?
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let role = match args.role.as_deref() {
        Some("client") => Role::Client,
        Some("server") => Role::Server,
        _ if exe.contains("client") => Role::Client,
        _ => Role::Server,
    };
    let id = args.agent_id.unwrap_or_else(|| {
        format!(
            "{}-1",
            if role == Role::Client {
                "client"
            } else {
                "server"
            }
        )
    });
    loop {
        match run_connection(&args.control, &id, role).await {
            Ok(()) => warn!("control stream ended"),
            Err(e) => error!(%e,"control connection failed"),
        };
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_connection(control: &str, id: &str, role: Role) -> anyhow::Result<()> {
    let mut client = AgentControlClient::connect(control.to_owned()).await?;
    let (tx, rx) = mpsc::channel(128);
    tx.send(AgentMessage {
        body: Some(agent_message::Body::Hello(AgentHello {
            agent_id: id.into(),
            role: role.proto(),
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION").into(),
            interfaces: list_interfaces(),
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
    let prepared: Arc<Mutex<Option<Prepared>>> = Default::default();
    type ActiveRun = (Uuid, Arc<AtomicBool>, Arc<AtomicBool>);
    let active_run: Arc<Mutex<Option<ActiveRun>>> = Default::default();
    while let Some(cmd) = commands.next().await {
        match cmd?.body {
            Some(control_message::Body::Prepare(p)) => {
                let scenario: Scenario =
                    serde_json::from_str::<Scenario>(&p.scenario_json)?.migrate();
                scenario.validate()?;
                let payloads = Arc::new(PreparedPayloads::new(&scenario)?);
                *prepared.lock().await = Some(Prepared {
                    run_id: Uuid::parse_str(&p.run_id)?,
                    scenario,
                    payloads,
                });
                info!(run=%p.run_id,"prepared");
            }
            Some(control_message::Body::Start(s)) => {
                let Some(job) = prepared.lock().await.clone() else {
                    continue;
                };
                let mut active = active_run.lock().await;
                if active
                    .as_ref()
                    .is_some_and(|(_, running, _)| running.load(Ordering::SeqCst))
                {
                    continue;
                }
                let running = Arc::new(AtomicBool::new(true));
                let paused = Arc::new(AtomicBool::new(false));
                *active = Some((job.run_id, running.clone(), paused.clone()));
                drop(active);
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    let delay = (s.start_unix_ms - Utc::now().timestamp_millis()).max(0) as u64;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let result =
                        run_job(job.clone(), role, tx2.clone(), running.clone(), paused).await;
                    if let Err(e) = result {
                        let _ = send_event(&tx2, job.run_id, "error", &e.to_string()).await;
                    }
                    running.store(false, Ordering::SeqCst);
                });
            }
            Some(control_message::Body::Stop(command)) => {
                if let Ok(run_id) = Uuid::parse_str(&command.run_id)
                    && let Some((active_id, running, paused)) = active_run.lock().await.as_ref()
                    && *active_id == run_id
                {
                    running.store(false, Ordering::SeqCst);
                    paused.store(false, Ordering::SeqCst);
                }
            }
            Some(control_message::Body::SetPaused(command)) => {
                if let Ok(run_id) = Uuid::parse_str(&command.run_id)
                    && let Some((active_id, running, paused)) = active_run.lock().await.as_ref()
                    && *active_id == run_id
                    && running.load(Ordering::SeqCst)
                {
                    paused.store(command.paused, Ordering::SeqCst);
                }
            }
            None => {}
        }
    }
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
    let interface_task = if !job.scenario.observation_interfaces.is_empty() {
        Some(tokio::spawn(monitor_interfaces(
            job.scenario.observation_interfaces.clone(),
            counters.clone(),
            running.clone(),
        )))
    } else {
        None
    };
    let metric_task = tokio::spawn(report_metrics(
        job.run_id,
        counters.clone(),
        tx.clone(),
        running.clone(),
        started,
    ));
    let result = match role {
        Role::Client => {
            run_client(
                &job.scenario,
                job.payloads.clone(),
                counters.clone(),
                running.clone(),
                paused.clone(),
            )
            .await
        }
        Role::Server => {
            run_server(
                &job.scenario,
                job.payloads.clone(),
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
    if role == Role::Client && result.is_ok() {
        // Give the responder one telemetry interval to flush its final counters before
        // control releases the global run lock.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        send_event(&tx, job.run_id, "info", "run_completed").await?;
    }
    result
}

async fn report_metrics(
    run_id: Uuid,
    c: Arc<Counters>,
    tx: mpsc::Sender<AgentMessage>,
    running: Arc<AtomicBool>,
    started: Instant,
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
        if tx
            .send(AgentMessage {
                body: Some(agent_message::Body::Telemetry(Telemetry {
                    run_id: run_id.to_string(),
                    metrics_json: serde_json::to_string(&m).unwrap(),
                })),
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn percentile_ms(samples: &[u64], percentile: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f64 * percentile) as usize] as f64 / 1000.0
}

async fn monitor_interfaces(
    interfaces: Vec<String>,
    counters: Arc<Counters>,
    running: Arc<AtomicBool>,
) {
    let baseline_tx: u64 = interfaces
        .iter()
        .map(|i| read_interface_stat(i, "tx_packets"))
        .sum();
    let baseline_rx: u64 = interfaces
        .iter()
        .map(|i| read_interface_stat(i, "rx_packets"))
        .sum();
    let baseline_tx_bytes: u64 = interfaces
        .iter()
        .map(|i| read_interface_stat(i, "tx_bytes"))
        .sum();
    let baseline_rx_bytes: u64 = interfaces
        .iter()
        .map(|i| read_interface_stat(i, "rx_bytes"))
        .sum();
    let baseline_retransmissions = read_tcp_retransmissions();
    while running.load(Ordering::Relaxed) {
        let tx: u64 = interfaces
            .iter()
            .map(|i| read_interface_stat(i, "tx_packets"))
            .sum();
        let rx: u64 = interfaces
            .iter()
            .map(|i| read_interface_stat(i, "rx_packets"))
            .sum();
        let tx_bytes: u64 = interfaces
            .iter()
            .map(|i| read_interface_stat(i, "tx_bytes"))
            .sum();
        let rx_bytes: u64 = interfaces
            .iter()
            .map(|i| read_interface_stat(i, "rx_bytes"))
            .sum();
        counters
            .packets_tx
            .store(tx.saturating_sub(baseline_tx), Ordering::Relaxed);
        counters
            .packets_rx
            .store(rx.saturating_sub(baseline_rx), Ordering::Relaxed);
        counters.wire_tx_bytes.store(
            tx_bytes.saturating_sub(baseline_tx_bytes),
            Ordering::Relaxed,
        );
        counters.wire_rx_bytes.store(
            rx_bytes.saturating_sub(baseline_rx_bytes),
            Ordering::Relaxed,
        );
        counters.tcp_retransmissions.store(
            read_tcp_retransmissions().saturating_sub(baseline_retransmissions),
            Ordering::Relaxed,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn read_tcp_retransmissions() -> u64 {
    let Ok(snmp) = std::fs::read_to_string("/proc/net/snmp") else {
        return 0;
    };
    let mut lines = snmp.lines().filter(|line| line.starts_with("Tcp:"));
    let (Some(header), Some(values)) = (lines.next(), lines.next()) else {
        return 0;
    };
    let names = header.split_whitespace().skip(1);
    let values = values.split_whitespace().skip(1);
    names
        .zip(values)
        .find_map(|(name, value)| {
            (name == "RetransSegs")
                .then(|| value.parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn read_interface_stat(interface: &str, stat: &str) -> u64 {
    std::fs::read_to_string(format!("/sys/class/net/{interface}/statistics/{stat}"))
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

async fn run_client(
    sc: &Scenario,
    payloads: Arc<PreparedPayloads>,
    c: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    run_client_concurrency(sc, payloads, c, running, paused).await
}

async fn run_client_concurrency(
    sc: &Scenario,
    payloads: Arc<PreparedPayloads>,
    c: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let generating = Arc::new(AtomicBool::new(true));
    let desired_clients = Arc::new(AtomicU32::new(0));
    let sc = Arc::new(sc.clone());
    let worker_count = sc.maximum_virtual_clients();
    let mut workers = Vec::with_capacity(worker_count as usize);
    for worker_index in 0..worker_count {
        let sc = sc.clone();
        let c = c.clone();
        let running = running.clone();
        let paused = paused.clone();
        let generating = generating.clone();
        let desired_clients = desired_clients.clone();
        let payloads = payloads.clone();
        workers.push(tokio::spawn(async move {
            while running.load(Ordering::Relaxed) && generating.load(Ordering::Relaxed) {
                if paused.load(Ordering::Relaxed)
                    || worker_index >= desired_clients.load(Ordering::Relaxed)
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                execute_connection(
                    &sc,
                    &payloads,
                    &c,
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
    let duration = Duration::from_secs(sc.effective_duration_secs());
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
            let (_, desired, _) = sc.load_at(active_elapsed.as_millis() as u64);
            desired_clients.store(desired, Ordering::Relaxed);
            let (stage_index, _, included) = sc.load_at(active_elapsed.as_millis() as u64);
            c.load_stage_index
                .store(stage_index as u32, Ordering::Relaxed);
            c.desired_virtual_clients.store(desired, Ordering::Relaxed);
            c.included_in_results.store(included, Ordering::Relaxed);
        }
    }
    generating.store(false, Ordering::SeqCst);
    for worker in workers {
        let _ = worker.await;
    }
    Ok(())
}

async fn execute_connection(
    sc: &Scenario,
    payloads: &PreparedPayloads,
    c: &Counters,
    running: &AtomicBool,
    paused: &AtomicBool,
    generating: &AtomicBool,
    desired_clients: &AtomicU32,
    worker_index: u32,
) {
    c.attempted.fetch_add(1, Ordering::Relaxed);
    let gate = WorkerGate {
        running,
        paused,
        generating,
        desired_clients,
        worker_index,
    };
    match transact(sc, payloads, c, &gate).await {
        Ok(()) => {}
        Err(_) => {
            c.failed.fetch_add(1, Ordering::Relaxed);
            c.transaction_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn transact(
    sc: &Scenario,
    payloads: &PreparedPayloads,
    c: &Counters,
    gate: &WorkerGate<'_>,
) -> anyhow::Result<()> {
    let connect_addr = if sc.topology == Topology::ExplicitProxy {
        sc.proxy_addr.as_ref().unwrap()
    } else {
        &sc.target_addr
    };
    let connect_started = Instant::now();
    let mut tcp_stream = tokio::time::timeout(
        Duration::from_millis(sc.timeouts.connect_ms),
        TcpStream::connect(connect_addr),
    )
    .await??;
    c.tcp_connect_latencies_us
        .lock()
        .await
        .push(connect_started.elapsed().as_micros() as u64);
    c.connection_established();
    let _active_connection = c.connection_opened();
    let needs_tunnel = sc.topology == Topology::ExplicitProxy
        && (sc.tls.enabled || sc.protocol != Protocol::Http1);
    if needs_tunnel {
        connect_tunnel(&mut tcp_stream, sc, c).await?;
    }
    let mut stream: Box<dyn IoStream> = if sc.tls.enabled {
        Box::new(connect_tls(tcp_stream, sc).await?)
    } else {
        Box::new(tcp_stream)
    };
    let result = match sc.protocol {
        Protocol::Http1 => {
            let absolute = sc.topology == Topology::ExplicitProxy && !sc.tls.enabled;
            http_transactions(&mut *stream, sc, payloads, absolute, c, gate).await
        }
        _ => tcp_transaction(&mut *stream, sc, payloads, c).await,
    };
    if result.is_ok() && sc.protocol != Protocol::Http1 {
        c.transaction_completed();
    }
    result
}

async fn connect_tunnel(stream: &mut TcpStream, sc: &Scenario, c: &Counters) -> anyhow::Result<()> {
    let req = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nProxy-Connection: keep-alive\r\n\r\n",
        sc.target_addr, sc.target_addr
    );
    stream.write_all(req.as_bytes()).await?;
    c.tx.fetch_add(req.len() as u64, Ordering::Relaxed);
    let head = tokio::time::timeout(
        Duration::from_millis(sc.timeouts.proxy_connect_ms),
        read_headers(stream, 16 * 1024),
    )
    .await??;
    c.rx.fetch_add(head.len() as u64, Ordering::Relaxed);
    if !head.starts_with(b"HTTP/1.1 200") && !head.starts_with(b"HTTP/1.0 200") {
        bail!("CONNECT rejected")
    };
    Ok(())
}
async fn tcp_transaction(
    stream: &mut (impl IoStream + ?Sized),
    sc: &Scenario,
    payloads: &PreparedPayloads,
    c: &Counters,
) -> anyhow::Result<()> {
    stream.write_all(&payloads.request).await?;
    c.tx.fetch_add(payloads.request.len() as u64, Ordering::Relaxed);
    tokio::time::timeout(
        Duration::from_millis(sc.timeouts.response_ms),
        read_exact_count(stream, payloads.response.len()),
    )
    .await??;
    c.rx.fetch_add(payloads.response.len() as u64, Ordering::Relaxed);
    Ok(())
}
async fn http_transactions(
    stream: &mut (impl IoStream + ?Sized),
    sc: &Scenario,
    payloads: &PreparedPayloads,
    absolute: bool,
    c: &Counters,
    gate: &WorkerGate<'_>,
) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(8192);
    let mut index = 0;
    while gate.enabled() {
        if sc.request.transactions_per_connection > 0
            && index >= sc.request.transactions_per_connection
        {
            break;
        }
        while gate.paused.load(Ordering::Relaxed)
            && gate.running.load(Ordering::Relaxed)
            && gate.generating.load(Ordering::Relaxed)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !gate.enabled() {
            break;
        }
        let transaction_started = Instant::now();
        let target = if absolute {
            format!("http://{}{}", sc.target_addr, sc.request.path)
        } else {
            sc.request.path.clone()
        };
        let last = sc.request.transactions_per_connection > 0
            && index + 1 == sc.request.transactions_per_connection;
        let connection = if sc.request.keep_alive && !last {
            "keep-alive"
        } else {
            "close"
        };
        let req = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: {}\r\nContent-Length: {}\r\nX-Response-Bytes: {}\r\n\r\n",
            sc.request.method,
            target,
            sc.request.host,
            connection,
            payloads.request.len(),
            payloads.response.len()
        );
        stream.write_all(req.as_bytes()).await?;
        stream.write_all(&payloads.request).await?;
        c.tx.fetch_add(
            (req.len() + payloads.request.len()) as u64,
            Ordering::Relaxed,
        );
        let (response, body_len) = tokio::time::timeout(
            Duration::from_millis(sc.timeouts.response_ms),
            read_http_message(stream, &mut pending, 64 * 1024),
        )
        .await??;
        c.rx.fetch_add((response.len() + body_len) as u64, Ordering::Relaxed);
        if !response.starts_with(b"HTTP/1.1 2") {
            bail!("HTTP transaction failed")
        }
        c.transaction_completed();
        c.http_latencies_us
            .lock()
            .await
            .push(transaction_started.elapsed().as_micros() as u64);
        index += 1;
        if !last && sc.request.think_time_ms > 0 {
            tokio::time::sleep(Duration::from_millis(sc.request.think_time_ms)).await;
        }
    }
    Ok(())
}

async fn connect_tls(
    stream: TcpStream,
    sc: &Scenario,
) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let config = if sc.tls.verify_peer {
        let mut roots = RootCertStore::empty();
        let pem = sc.tls.ca_pem.as_deref().context("CA PEM required")?;
        let certificates = rustls_pemfile::certs(&mut Cursor::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let (added, _) = roots.add_parsable_certificates(certificates);
        if added == 0 {
            bail!("CA PEM contains no usable certificates")
        }
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    } else {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth()
    };
    let server_name = ServerName::try_from(sc.tls.server_name.clone())?;
    Ok(TlsConnector::from(Arc::new(config))
        .connect(server_name, stream)
        .await?)
}

fn build_tls_acceptor(sc: &Scenario) -> anyhow::Result<TlsAcceptor> {
    let certificate_pem = sc
        .tls
        .server_cert_pem
        .as_deref()
        .context("server certificate required")?;
    let key_pem = sc
        .tls
        .server_key_pem
        .as_deref()
        .context("server private key required")?;
    let certificates = rustls_pemfile::certs(&mut Cursor::new(certificate_pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut Cursor::new(key_pem.as_bytes()))?
        .context("server private key PEM is empty")?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn run_server(
    sc: &Scenario,
    payloads: Arc<PreparedPayloads>,
    c: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let port = sc
        .target_addr
        .rsplit_once(':')
        .context("target port")?
        .1
        .parse::<u16>()?;
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let tls_acceptor = sc.tls.enabled.then(|| build_tls_acceptor(sc)).transpose()?;
    info!(port, "responder listening");
    let duration = Duration::from_millis(sc.effective_duration_secs() * 1000 + 700);
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
        let (stage_index, desired, included) = sc.load_at(active_elapsed.as_millis() as u64);
        c.load_stage_index
            .store(stage_index as u32, Ordering::Relaxed);
        c.desired_virtual_clients.store(desired, Ordering::Relaxed);
        c.included_in_results.store(included, Ordering::Relaxed);
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        let Ok(Ok((stream, _))) = accepted else {
            continue;
        };
        c.connection_established();
        let sc = sc.clone();
        let c2 = c.clone();
        let tls_acceptor = tls_acceptor.clone();
        let payloads = payloads.clone();
        tokio::spawn(async move {
            let _active_connection = c2.connection_opened();
            let mut stream: Box<dyn IoStream> = match tls_acceptor {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(stream) => Box::new(stream),
                    Err(_) => {
                        c2.transaction_errors.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                },
                None => Box::new(stream),
            };
            let r = if sc.protocol == Protocol::Http1 {
                serve_http_connection(&mut *stream, &payloads, &c2).await
            } else {
                serve_tcp(&mut *stream, &payloads, &c2).await
            };
            if r.is_ok() {
                if sc.protocol != Protocol::Http1 {
                    c2.transaction_completed();
                }
            } else {
                c2.transaction_errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    Ok(())
}
async fn serve_tcp(
    stream: &mut (impl IoStream + ?Sized),
    payloads: &PreparedPayloads,
    c: &Counters,
) -> anyhow::Result<()> {
    read_exact_count(stream, payloads.request.len()).await?;
    c.rx.fetch_add(payloads.request.len() as u64, Ordering::Relaxed);
    stream.write_all(&payloads.response).await?;
    c.tx.fetch_add(payloads.response.len() as u64, Ordering::Relaxed);
    Ok(())
}
async fn serve_http_connection(
    stream: &mut (impl IoStream + ?Sized),
    payloads: &PreparedPayloads,
    c: &Counters,
) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(8192);
    loop {
        let (headers, body_len) = read_http_message(stream, &mut pending, 64 * 1024).await?;
        c.rx.fetch_add((headers.len() + body_len) as u64, Ordering::Relaxed);
        let response_len =
            parse_header(&headers, "x-response-bytes").unwrap_or(payloads.response.len());
        let keep_alive = header_equals(&headers, "connection", "keep-alive");
        let connection = if keep_alive { "keep-alive" } else { "close" };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {response_len}\r\nConnection: {connection}\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        if response_len == payloads.response.len() {
            stream.write_all(&payloads.response).await?;
        } else {
            write_zeroes(stream, response_len).await?;
        }
        c.tx.fetch_add((head.len() + response_len) as u64, Ordering::Relaxed);
        c.transaction_completed();
        if !keep_alive {
            return Ok(());
        }
    }
}

async fn read_http_message(
    stream: &mut (impl AsyncRead + Unpin + ?Sized),
    pending: &mut Vec<u8>,
    max_headers: usize,
) -> anyhow::Result<(Vec<u8>, usize)> {
    let header_end = loop {
        if let Some(position) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
        if pending.len() > max_headers {
            bail!("headers too large")
        }
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before HTTP message")
        }
        pending.extend_from_slice(&chunk[..n]);
    };
    let remainder = pending.split_off(header_end);
    let headers = std::mem::replace(pending, remainder);
    let body_len = parse_header(&headers, "content-length").unwrap_or(0);
    while pending.len() < body_len {
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before HTTP body")
        }
        pending.extend_from_slice(&chunk[..n]);
    }
    pending.drain(..body_len);
    Ok((headers, body_len))
}

async fn read_headers(
    stream: &mut (impl AsyncRead + Unpin + ?Sized),
    max: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(1024);
    let mut b = [0u8; 1024];
    loop {
        let n = stream.read(&mut b).await?;
        if n == 0 {
            bail!("connection closed before headers")
        };
        out.extend_from_slice(&b[..n]);
        if out.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(out);
        }
        if out.len() > max {
            bail!("headers too large")
        }
    }
}
fn parse_header(raw: &[u8], name: &str) -> Option<usize> {
    let text = String::from_utf8_lossy(raw);
    text.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.trim().parse().ok())
}
fn header_equals(raw: &[u8], name: &str, expected: &str) -> bool {
    let text = String::from_utf8_lossy(raw);
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(header, value)| {
            header.eq_ignore_ascii_case(name) && value.trim().eq_ignore_ascii_case(expected)
        })
}
async fn write_zeroes(
    stream: &mut (impl AsyncWrite + Unpin + ?Sized),
    mut n: usize,
) -> anyhow::Result<()> {
    static ZERO: [u8; 65536] = [0; 65536];
    while n > 0 {
        let size = n.min(ZERO.len());
        stream.write_all(&ZERO[..size]).await?;
        n -= size;
    }
    Ok(())
}
async fn read_exact_count(
    stream: &mut (impl AsyncRead + Unpin + ?Sized),
    mut n: usize,
) -> anyhow::Result<()> {
    let mut buf = [0u8; 65536];
    while n > 0 {
        let size = n.min(buf.len());
        stream.read_exact(&mut buf[..size]).await?;
        n -= size;
    }
    Ok(())
}
async fn send_event(
    tx: &mpsc::Sender<AgentMessage>,
    run_id: Uuid,
    level: &str,
    message: &str,
) -> anyhow::Result<()> {
    tx.send(AgentMessage {
        body: Some(agent_message::Body::Event(AgentEvent {
            run_id: run_id.to_string(),
            level: level.into(),
            message: message.into(),
        })),
    })
    .await?;
    Ok(())
}
fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into())
}
fn list_interfaces() -> Vec<String> {
    std::fs::read_dir("/sys/class/net")
        .map(|it| {
            it.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                .collect()
        })
        .unwrap_or_default()
}
fn tokio_stream<T: Send + 'static>(mut rx: mpsc::Receiver<T>) -> impl futures::Stream<Item = T> {
    async_stream::stream! {while let Some(v)=rx.recv().await{yield v;}}
}
