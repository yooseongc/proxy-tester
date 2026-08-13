use anyhow::{Context, bail};
use bytes::Bytes;
use chrono::Utc;
use clap::Parser;
use futures::{StreamExt, stream::FuturesUnordered};
use proxy_tester_capture::{Direction, HttpTransaction, ReplayTurn, analyze_capture};
use proxy_tester_domain::{
    MetricsSnapshot, PayloadKind, PayloadMode, PayloadProfile, Protocol, RandomFormat, Scenario,
    TlsVersion,
};
use proxy_tester_proto::v1::{
    AgentEvent, AgentHello, AgentMessage, AgentRole, AgentStatus, CommandAck, Heartbeat, Telemetry,
    agent_control_client::AgentControlClient, agent_message, control_message,
};
use rand::RngCore;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
    SupportedProtocolVersion, crypto::CryptoProvider, version,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
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
mod network;
use network::{NetworkManager, NetworkPlan};

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
    #[arg(long, env = "PROXY_TESTER_ROLE")]
    role: Option<String>,
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
    timeout_errors: AtomicU64,
    reset_errors: AtomicU64,
    tls_handshake_errors: AtomicU64,
    proxy_connect_errors: AtomicU64,
    http_error_responses: AtomicU64,
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
            timeout_errors: Default::default(),
            reset_errors: Default::default(),
            tls_handshake_errors: Default::default(),
            proxy_connect_errors: Default::default(),
            http_error_responses: Default::default(),
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

    fn record_failure(&self, error: &anyhow::Error) {
        let message = format!("{error:#}").to_ascii_lowercase();
        if message.contains("deadline has elapsed") || message.contains("timed out") {
            self.timeout_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("connection reset") || message.contains("forcibly closed") {
            self.reset_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("tls handshake failed") {
            self.tls_handshake_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("http connect failed") {
            self.proxy_connect_errors.fetch_add(1, Ordering::Relaxed);
        }
        if message.contains("http error response") {
            self.http_error_responses.fetch_add(1, Ordering::Relaxed);
        }
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
    replay: Option<Arc<ReplayPlan>>,
}

#[derive(Clone)]
struct ReplayPlan {
    flows: Arc<[Arc<[ReplayTurn]>]>,
}

impl ReplayPlan {
    fn from_capture(bytes: &[u8], scenario: &Scenario, role: Role) -> anyhow::Result<Self> {
        let (_, analysis) = analyze_capture(bytes)?;
        if analysis.flows.is_empty() {
            bail!("capture contains no supported flows");
        }
        let flows: Arc<[Arc<[ReplayTurn]>]> = analysis
            .flows
            .into_iter()
            .filter_map(|flow| {
                let turns = if scenario.protocol == Protocol::Http1 {
                    (!flow.http_transactions.is_empty()).then(|| {
                        flow.http_transactions
                            .into_iter()
                            .flat_map(|transaction| http_replay_turns(transaction, scenario, role))
                            .collect()
                    })
                } else if scenario.protocol == Protocol::Http2 {
                    (!flow.http2_transactions.is_empty()).then(|| {
                        flow.http2_transactions
                            .into_iter()
                            .flat_map(|transaction| {
                                [
                                    ReplayTurn {
                                        direction: Direction::ClientToServer,
                                        payload: transaction.request_body,
                                    },
                                    ReplayTurn {
                                        direction: Direction::ServerToClient,
                                        payload: transaction.response_body,
                                    },
                                ]
                            })
                            .collect()
                    })
                } else {
                    Some(flow.turns)
                }?;
                Some(Arc::<[ReplayTurn]>::from(turns))
            })
            .collect::<Vec<_>>()
            .into();
        if flows.is_empty() {
            bail!("capture contains no supported HTTP/1.1 transactions");
        }
        for (index, flow) in flows.iter().enumerate() {
            let Some(first) = flow.first() else {
                bail!("flow {index} has no replay turns");
            };
            if first.direction != Direction::ClientToServer || first.payload.is_empty() {
                bail!("flow {index} must start with a non-empty client turn");
            }
            for (other_index, other) in flows.iter().enumerate() {
                if index != other_index
                    && other.first().is_some_and(|turn| {
                        turn.payload.starts_with(&first.payload)
                            || first.payload.starts_with(&turn.payload)
                    })
                {
                    bail!("flows {index} and {other_index} have ambiguous first client turns");
                }
            }
        }
        Ok(Self { flows })
    }
}

fn http_replay_turns(
    transaction: HttpTransaction,
    scenario: &Scenario,
    role: Role,
) -> [ReplayTurn; 2] {
    [
        ReplayTurn {
            direction: Direction::ClientToServer,
            payload: rewrite_http_request(&transaction.request, scenario, role),
        },
        ReplayTurn {
            direction: Direction::ServerToClient,
            payload: transaction.response,
        },
    ]
}

fn rewrite_http_request(request: &[u8], scenario: &Scenario, role: Role) -> Vec<u8> {
    let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
        return request.to_vec();
    };
    let header_end = header_end + 4;
    let headers = &request[..header_end];
    let Some(line_end) = headers.windows(2).position(|part| part == b"\r\n") else {
        return request.to_vec();
    };
    let mut fields = headers[..line_end].splitn(3, |byte| *byte == b' ');
    let (Some(method), Some(target), Some(version)) = (fields.next(), fields.next(), fields.next())
    else {
        return request.to_vec();
    };
    let path = if target.starts_with(b"http://") {
        target
            .windows(1)
            .enumerate()
            .skip(7)
            .find(|(_, byte)| *byte == b"/")
            .map(|(index, _)| &target[index..])
            .unwrap_or(b"/")
    } else {
        target
    };
    let absolute = role == Role::Client && scenario.is_explicit_proxy() && !scenario.tls.enabled;
    let target = if absolute {
        format!(
            "http://{}{}",
            scenario.target_addr(),
            String::from_utf8_lossy(path)
        )
        .into_bytes()
    } else {
        path.to_vec()
    };
    let mut output = Vec::with_capacity(request.len() + target.len());
    output.extend_from_slice(method);
    output.push(b' ');
    output.extend_from_slice(&target);
    output.push(b' ');
    output.extend_from_slice(version);
    output.extend_from_slice(b"\r\n");
    let mut replaced_host = false;
    for line in headers[line_end + 2..header_end - 2].split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line
            .split(|byte| *byte == b':')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(b"host"))
        {
            output.extend_from_slice(format!("Host: {}\r\n", scenario.request.host).as_bytes());
            replaced_host = true;
        } else {
            output.extend_from_slice(line);
            output.extend_from_slice(b"\r\n");
        }
    }
    if !replaced_host {
        output.extend_from_slice(format!("Host: {}\r\n", scenario.request.host).as_bytes());
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&request[header_end..]);
    output
}

#[derive(Clone)]
struct PreparedPayloads {
    request: Arc<[u8]>,
    response: Arc<[u8]>,
}
impl PreparedPayloads {
    fn new(sc: &Scenario, artifacts: &HashMap<Uuid, CompletedArtifact>) -> anyhow::Result<Self> {
        Ok(Self {
            request: materialize(&sc.request_payload(), artifacts)?,
            response: materialize(&sc.response_payload(), artifacts)?,
        })
    }
}
fn materialize(
    profile: &PayloadProfile,
    artifacts: &HashMap<Uuid, CompletedArtifact>,
) -> anyhow::Result<Arc<[u8]>> {
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
        PayloadKind::File => {
            let id = profile.artifact_id.context("file payload artifact ID")?;
            return artifacts
                .get(&id)
                .and_then(|artifact| match artifact {
                    CompletedArtifact::Payload(bytes) => Some(bytes.clone()),
                    _ => None,
                })
                .with_context(|| format!("artifact {id} was not transferred"));
        }
    };
    info!(kind=?profile.kind, bytes=bytes.len(), sha256=%format!("{:x}", Sha256::digest(&bytes)), "payload prepared");
    Ok(bytes.into())
}

struct IncomingArtifact {
    path: std::path::PathBuf,
    file: tokio::fs::File,
    received: u64,
    digest: Sha256,
    total_size: u64,
    sha256: String,
    kind: String,
}

enum CompletedArtifact {
    Payload(Arc<[u8]>),
    Capture(std::path::PathBuf),
}

async fn accept_artifact_chunk(
    incoming: &mut HashMap<Uuid, IncomingArtifact>,
    completed: &mut HashMap<Uuid, CompletedArtifact>,
    chunk: proxy_tester_proto::v1::ArtifactChunk,
) -> anyhow::Result<()> {
    let id = Uuid::parse_str(&chunk.artifact_id)?;
    let limit = if chunk.artifact_kind == "pcap" {
        512 * 1024 * 1024
    } else {
        proxy_tester_domain::MAX_PAYLOAD_BYTES as u64
    };
    if chunk.total_size > limit {
        bail!("artifact {id} exceeds the agent limit");
    }
    if completed.contains_key(&id) {
        bail!("artifact {id} was transferred more than once");
    }
    if !incoming.contains_key(&id) && chunk.offset != 0 {
        bail!("artifact {id} first chunk offset must be zero");
    }
    if !incoming.contains_key(&id) {
        let temporary_dir = std::env::temp_dir();
        tokio::fs::create_dir_all(&temporary_dir)
            .await
            .with_context(|| format!("create temporary directory {temporary_dir:?}"))?;
        let path = temporary_dir.join(format!("proxy-tester-{id}.artifact.part"));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .with_context(|| format!("create temporary artifact {path:?}"))?;
        incoming.insert(
            id,
            IncomingArtifact {
                path,
                file,
                received: 0,
                digest: Sha256::new(),
                total_size: chunk.total_size,
                sha256: chunk.sha256.clone(),
                kind: chunk.artifact_kind.clone(),
            },
        );
    }
    let state = incoming.get_mut(&id).context("artifact transfer state")?;
    if state.total_size != chunk.total_size
        || state.sha256 != chunk.sha256
        || state.kind != chunk.artifact_kind
    {
        bail!("artifact {id} metadata changed during transfer");
    }
    if chunk.offset != state.received {
        bail!("artifact {id} chunk offset mismatch");
    }
    if state.received.saturating_add(chunk.data.len() as u64) > state.total_size {
        bail!("artifact {id} exceeds declared size");
    }
    state.file.write_all(&chunk.data).await?;
    state.digest.update(&chunk.data);
    state.received += chunk.data.len() as u64;
    if chunk.eof {
        if state.received != state.total_size {
            bail!("artifact {id} ended before declared size");
        }
        state.file.flush().await?;
        let finished = incoming.remove(&id).context("artifact transfer state")?;
        let actual = format!("{:x}", finished.digest.finalize());
        if actual != finished.sha256 {
            let _ = tokio::fs::remove_file(&finished.path).await;
            bail!("artifact {id} SHA-256 mismatch");
        }
        if finished.kind == "pcap" {
            completed.insert(id, CompletedArtifact::Capture(finished.path));
        } else {
            let bytes = tokio::fs::read(&finished.path).await?;
            tokio::fs::remove_file(&finished.path).await?;
            completed.insert(id, CompletedArtifact::Payload(bytes.into()));
        }
    }
    Ok(())
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
        match run_connection(&args.control, &id, &instance_id, role, network.clone()).await {
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
    role: Role,
    network: NetworkManager,
) -> anyhow::Result<()> {
    let mut client = AgentControlClient::connect(control.to_owned()).await?;
    let (tx, rx) = mpsc::channel(128);
    let inventory = network.inventory().await.unwrap_or_default();
    tx.send(AgentMessage {
        body: Some(agent_message::Body::Hello(AgentHello {
            agent_id: id.into(),
            role: role.proto(),
            hostname: hostname(),
            version: env!("CARGO_PKG_VERSION").into(),
            interfaces: list_interfaces(),
            instance_id: instance_id.into(),
            inventory_json: serde_json::to_string(&inventory)?,
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
                let mut scenario: Scenario =
                    serde_json::from_str::<Scenario>(&p.scenario_json)?.migrate();
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
                    let replay = ReplayPlan::from_capture(&bytes, &scenario, endpoint_role)?;
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
                let mut detail = serde_json::Value::Null;
                let outcome = match command.action.as_str() {
                    "plan" => {
                        let value: serde_json::Value = serde_json::from_str(&command.payload_json)?;
                        let draft: proxy_tester_domain::NetworkProfileDraft =
                            serde_json::from_value(value["draft"].clone())?;
                        let revision = value["profile_revision_id"]
                            .as_str()
                            .context("profile_revision_id")?;
                        let inventory = network.inventory().await?;
                        match network.plan(id, revision, &draft, &inventory) {
                            Ok(plan) => {
                                detail = serde_json::to_value(plan)?;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    }
                    "stage" => {
                        let plan: NetworkPlan = serde_json::from_str(&command.payload_json)?;
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
                        result
                    }
                    "commit" => network.commit().await,
                    "rollback" | "teardown" => network.recover().await,
                    "reconcile" => network.recover().await,
                    action => Err(anyhow::anyhow!("unknown network action {action}")),
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
                            stage: command.action.clone(),
                            status: if ok {
                                "completed".into()
                            } else {
                                "failed".into()
                            },
                            detail_json: if ok {
                                detail.to_string()
                            } else {
                                serde_json::json!({"error":error}).to_string()
                            },
                        },
                    )),
                })
                .await?;
                send_ack(&tx, &command.command_id, "", &command.action, ok, &error).await?;
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
        tokio::spawn(monitor_interfaces(
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
            run_client(
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
            run_server(
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
        if tx
            .send(AgentMessage {
                body: Some(agent_message::Body::Telemetry(Telemetry {
                    run_id: run_id.to_string(),
                    metrics_json: serde_json::to_string(&m).unwrap(),
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
    let request = (role == Role::Client && scenario.request_payload().kind == PayloadKind::Random)
        .then(|| format!("{:x}", Sha256::digest(&payloads.request)));
    let response = (role == Role::Server
        && scenario.response_payload().kind == PayloadKind::Random)
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
    replay: Option<Arc<ReplayPlan>>,
    c: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    run_client_concurrency(sc, payloads, replay, c, running, paused).await
}

async fn run_client_concurrency(
    sc: &Scenario,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
    c: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let generating = Arc::new(AtomicBool::new(true));
    let desired_clients = Arc::new(AtomicU32::new(0));
    let next_flow = Arc::new(AtomicU64::new(0));
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
                    &sc,
                    &payloads,
                    replay.as_deref(),
                    &next_flow,
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
    replay: Option<&ReplayPlan>,
    next_flow: &AtomicU64,
    c: &Counters,
    running: &AtomicBool,
    paused: &AtomicBool,
    generating: &AtomicBool,
    desired_clients: &AtomicU32,
    worker_index: u32,
) {
    let connection_index = c.attempted.fetch_add(1, Ordering::Relaxed);
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
    match transact(sc, payloads, flow.as_deref(), c, &gate, connection_index).await {
        Ok(()) => {}
        Err(error) => {
            c.failed.fetch_add(1, Ordering::Relaxed);
            c.transaction_errors.fetch_add(1, Ordering::Relaxed);
            c.record_failure(&error);
        }
    }
}

async fn transact(
    sc: &Scenario,
    payloads: &PreparedPayloads,
    replay_turns: Option<&[ReplayTurn]>,
    c: &Counters,
    gate: &WorkerGate<'_>,
    connection_index: u64,
) -> anyhow::Result<()> {
    let target_addr = sc.target_addr();
    let connect_addr = sc.proxy_addr().unwrap_or(&target_addr);
    let connect_started = Instant::now();
    let connect_future = async {
        let remote = tokio::net::lookup_host(connect_addr)
            .await?
            .next()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "target address did not resolve",
                )
            })?;
        let socket = if remote.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        if let Some(ip) = sc
            .runtime_source_ips
            .get(connection_index as usize % sc.runtime_source_ips.len().max(1))
        {
            socket.bind(SocketAddr::new(*ip, 0))?;
        }
        socket.connect(remote).await
    };
    let mut tcp_stream = tokio::time::timeout(
        Duration::from_millis(sc.timeouts.connect_ms),
        connect_future,
    )
    .await?
    .context("TCP connect failed")?;
    c.tcp_connect_latencies_us
        .lock()
        .await
        .push(connect_started.elapsed().as_micros() as u64);
    c.connection_established();
    let _active_connection = c.connection_opened();
    let needs_tunnel = sc.is_explicit_proxy() && (sc.tls.enabled || sc.protocol != Protocol::Http1);
    if needs_tunnel {
        connect_tunnel(&mut tcp_stream, sc, c)
            .await
            .context("HTTP CONNECT failed")?;
    }
    let mut stream: Box<dyn IoStream> = if sc.tls.enabled {
        Box::new(
            connect_tls(tcp_stream, sc)
                .await
                .context("TLS handshake failed")?,
        )
    } else {
        Box::new(tcp_stream)
    };
    let result = if sc.protocol == Protocol::Http2 {
        http2_transactions(stream, sc, payloads, replay_turns, c, gate).await
    } else if let Some(turns) = replay_turns {
        replay_client(&mut *stream, turns, sc, c).await
    } else {
        match sc.protocol {
            Protocol::Http1 => {
                let absolute = sc.is_explicit_proxy() && !sc.tls.enabled;
                http_transactions(&mut *stream, sc, payloads, absolute, c, gate).await
            }
            _ => tcp_transaction(&mut *stream, sc, payloads, c).await,
        }
    };
    if result.is_ok() && !matches!(sc.protocol, Protocol::Http1 | Protocol::Http2) {
        c.transaction_completed();
    }
    result
}

async fn connect_tunnel(stream: &mut TcpStream, sc: &Scenario, c: &Counters) -> anyhow::Result<()> {
    let req = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nProxy-Connection: keep-alive\r\n\r\n",
        sc.target_addr(),
        sc.target_addr()
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
        bail!(
            "CONNECT rejected with response {}",
            String::from_utf8_lossy(&head[..head.len().min(128)])
        )
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

async fn replay_client(
    stream: &mut (impl IoStream + ?Sized),
    turns: &[ReplayTurn],
    sc: &Scenario,
    c: &Counters,
) -> anyhow::Result<()> {
    let mut transaction_started = None;
    for turn in turns {
        match turn.direction {
            Direction::ClientToServer => {
                if sc.protocol == Protocol::Http1 {
                    transaction_started = Some(Instant::now());
                }
                stream.write_all(&turn.payload).await?;
                c.tx.fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
            }
            Direction::ServerToClient => {
                tokio::time::timeout(
                    Duration::from_millis(sc.timeouts.response_ms),
                    read_exact_count(stream, turn.payload.len()),
                )
                .await??;
                c.rx.fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
                if sc.protocol == Protocol::Http1 {
                    c.transaction_completed();
                    if let Some(started) = transaction_started.take() {
                        c.http_latencies_us
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

async fn replay_server(
    stream: &mut (impl IoStream + ?Sized),
    turns: &[ReplayTurn],
    sc: &Scenario,
    c: &Counters,
) -> anyhow::Result<()> {
    for turn in turns {
        match turn.direction {
            Direction::ClientToServer => {
                tokio::time::timeout(
                    Duration::from_millis(sc.timeouts.response_ms),
                    read_exact_count(stream, turn.payload.len()),
                )
                .await??;
                c.rx.fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
            }
            Direction::ServerToClient => {
                stream.write_all(&turn.payload).await?;
                c.tx.fetch_add(turn.payload.len() as u64, Ordering::Relaxed);
                if sc.protocol == Protocol::Http1 {
                    c.transaction_completed();
                }
            }
        }
    }
    Ok(())
}

async fn replay_server_detect(
    stream: &mut (impl IoStream + ?Sized),
    plan: &ReplayPlan,
    sc: &Scenario,
    c: &Counters,
) -> anyhow::Result<()> {
    let mut received = Vec::new();
    let mut candidates: Vec<usize> = (0..plan.flows.len()).collect();
    loop {
        let mut byte = [0_u8; 1];
        tokio::time::timeout(
            Duration::from_millis(sc.timeouts.response_ms),
            stream.read_exact(&mut byte),
        )
        .await??;
        received.push(byte[0]);
        candidates.retain(|index| plan.flows[*index][0].payload.starts_with(&received));
        if candidates.is_empty() {
            bail!("first client turn does not match any capture flow");
        }
        if candidates.len() == 1 {
            let flow = &plan.flows[candidates[0]];
            if flow[0].payload.len() == received.len() {
                c.rx.fetch_add(received.len() as u64, Ordering::Relaxed);
                return replay_server(stream, &flow[1..], sc, c).await;
            }
        }
    }
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
            format!("http://{}{}", sc.target_addr(), sc.request.path)
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
            let status = response
                .split(|byte| *byte == b' ')
                .nth(1)
                .unwrap_or_default();
            bail!("HTTP error response {}", String::from_utf8_lossy(status))
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

async fn send_h2_body(send: &mut h2::SendStream<Bytes>, payload: &[u8]) -> anyhow::Result<()> {
    if payload.is_empty() {
        send.send_data(Bytes::new(), true)?;
        return Ok(());
    }
    let mut offset = 0;
    while offset < payload.len() {
        send.reserve_capacity((payload.len() - offset).min(16 * 1024));
        let capacity = futures::future::poll_fn(|cx| send.poll_capacity(cx))
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

async fn http2_request_parts(
    sender: h2::client::SendRequest<Bytes>,
    sc: &Scenario,
    request_payload: &[u8],
    response_len: usize,
) -> anyhow::Result<(u64, u64, u64)> {
    let started = Instant::now();
    let uri = format!("https://{}{}", sc.request.host, sc.request.path);
    let request = http::Request::builder()
        .method(sc.request.method.as_str())
        .uri(uri)
        .version(http::Version::HTTP_2)
        .header("x-response-bytes", response_len.to_string())
        .body(())?;
    let mut ready = sender.ready().await?;
    let (response, mut send) = ready.send_request(request, request_payload.is_empty())?;
    if !request_payload.is_empty() {
        send_h2_body(&mut send, request_payload).await?;
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

async fn http2_transactions(
    stream: Box<dyn IoStream>,
    sc: &Scenario,
    payloads: &PreparedPayloads,
    replay_turns: Option<&[ReplayTurn]>,
    c: &Counters,
    gate: &WorkerGate<'_>,
) -> anyhow::Result<()> {
    let mut builder = h2::client::Builder::new();
    builder.initial_max_send_streams(sc.http2.max_concurrent_streams as usize);
    let (sender, connection) = builder.handshake(stream).await?;
    let driver = tokio::spawn(async move { connection.await });
    let maximum = sc.request.transactions_per_connection as u64;
    let concurrency = sc.http2.max_concurrent_streams as usize;
    let mut launched = 0_u64;
    let mut pending = FuturesUnordered::new();
    while (gate.enabled() && (maximum == 0 || launched < maximum)) || !pending.is_empty() {
        while gate.enabled() && pending.len() < concurrency && (maximum == 0 || launched < maximum)
        {
            let request = replay_turns
                .and_then(|turns| turns.get((launched as usize * 2) % turns.len()))
                .map(|turn| turn.payload.as_slice())
                .unwrap_or(&payloads.request);
            let response_len = replay_turns
                .and_then(|turns| turns.get((launched as usize * 2 + 1) % turns.len()))
                .map(|turn| turn.payload.len())
                .unwrap_or(payloads.response.len());
            pending.push(http2_request_parts(
                sender.clone(),
                sc,
                request,
                response_len,
            ));
            launched += 1;
        }
        let Some(result) = pending.next().await else {
            break;
        };
        let (latency, sent, received) = result?;
        c.tx.fetch_add(sent, Ordering::Relaxed);
        c.rx.fetch_add(received, Ordering::Relaxed);
        c.transaction_completed();
        c.http_latencies_us.lock().await.push(latency);
        if sc.request.think_time_ms > 0 {
            tokio::time::sleep(Duration::from_millis(sc.request.think_time_ms)).await;
        }
    }
    drop(sender);
    driver.abort();
    let _ = driver.await;
    Ok(())
}

async fn connect_tls(
    stream: TcpStream,
    sc: &Scenario,
) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let builder = ClientConfig::builder_with_provider(tls_crypto_provider(sc)?)
        .with_protocol_versions(tls_protocol_versions(sc.tls.version))?;
    let mut config = if sc.tls.verify_peer {
        let mut roots = RootCertStore::empty();
        let pem = sc.tls.ca_pem.as_deref().context("CA PEM required")?;
        let certificates = rustls_pemfile::certs(&mut Cursor::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let (added, _) = roots.add_parsable_certificates(certificates);
        if added == 0 {
            bail!("CA PEM contains no usable certificates")
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth()
    };
    if sc.protocol == Protocol::Http2 {
        config.alpn_protocols = vec![b"h2".to_vec()];
    }
    let server_name = ServerName::try_from(sc.tls.server_name.clone())?;
    let stream = TlsConnector::from(Arc::new(config))
        .connect(server_name, stream)
        .await?;
    if sc.protocol == Protocol::Http2 && stream.get_ref().1.alpn_protocol() != Some(b"h2") {
        bail!("TLS ALPN negotiation did not select h2");
    }
    Ok(stream)
}

fn tls_crypto_provider(sc: &Scenario) -> anyhow::Result<Arc<CryptoProvider>> {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    if let Some(cipher) = sc.tls.cipher_suite.as_deref() {
        provider
            .cipher_suites
            .retain(|suite| format!("{:?}", suite.suite()) == cipher);
        if provider.cipher_suites.is_empty() {
            bail!("configured TLS cipher suite is unavailable: {cipher}");
        }
    }
    Ok(Arc::new(provider))
}

fn tls_protocol_versions(version: TlsVersion) -> &'static [&'static SupportedProtocolVersion] {
    static TLS12: [&SupportedProtocolVersion; 1] = [&version::TLS12];
    static TLS13: [&SupportedProtocolVersion; 1] = [&version::TLS13];
    match version {
        TlsVersion::Tls12 => &TLS12,
        TlsVersion::Tls13 => &TLS13,
    }
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
    let mut config = ServerConfig::builder_with_provider(tls_crypto_provider(sc)?)
        .with_protocol_versions(tls_protocol_versions(sc.tls.version))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    if sc.protocol == Protocol::Http2 {
        config.alpn_protocols = vec![b"h2".to_vec()];
    }
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn run_server(
    sc: &Scenario,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
    c: Arc<Counters>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let port = sc.server_port();
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
        let replay = replay.clone();
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
            let r = if sc.protocol == Protocol::Http2 {
                serve_http2_connection(stream, payloads, replay, c2.clone()).await
            } else if let Some(plan) = replay {
                replay_server_detect(&mut *stream, &plan, &sc, &c2).await
            } else if sc.protocol == Protocol::Http1 {
                serve_http_connection(&mut *stream, &payloads, &c2).await
            } else {
                serve_tcp(&mut *stream, &payloads, &c2).await
            };
            if r.is_ok() {
                if !matches!(sc.protocol, Protocol::Http1 | Protocol::Http2) {
                    c2.transaction_completed();
                }
            } else {
                c2.transaction_errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    Ok(())
}
async fn serve_http2_connection(
    stream: Box<dyn IoStream>,
    payloads: Arc<PreparedPayloads>,
    replay: Option<Arc<ReplayPlan>>,
    c: Arc<Counters>,
) -> anyhow::Result<()> {
    let mut connection = h2::server::handshake(stream).await?;
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(result) = connection.accept().await {
        let (request, mut respond) = result?;
        let payloads = payloads.clone();
        let c = c.clone();
        let replay_response = replay
            .as_ref()
            .and_then(|plan| plan.flows.first())
            .and_then(|flow| flow.get(1))
            .map(|turn| turn.payload.clone());
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
                c.rx.fetch_add(received, Ordering::Relaxed);
                let response = http::Response::builder()
                    .status(200)
                    .version(http::Version::HTTP_2)
                    .header("content-length", response_len.to_string())
                    .body(())?;
                let mut send = respond.send_response(response, response_len == 0)?;
                if response_len > 0 {
                    if replay_response
                        .as_ref()
                        .is_some_and(|body| body.len() == response_len)
                    {
                        send_h2_body(&mut send, replay_response.as_ref().unwrap()).await?;
                    } else if response_len == payloads.response.len() {
                        send_h2_body(&mut send, &payloads.response).await?;
                    } else {
                        send_h2_body(&mut send, &vec![0; response_len]).await?;
                    }
                }
                c.tx.fetch_add(response_len as u64, Ordering::Relaxed);
                c.transaction_completed();
                Ok(())
            }
            .await;
            if outcome.is_err() {
                c.transaction_errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use proxy_tester_proto::v1::ArtifactChunk;

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
            replay_server(&mut server_stream, &turns, &scenario, &server_counters),
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
        let plan = ReplayPlan::from_capture(capture, &Scenario::default(), Role::Client).unwrap();
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
        let plan = ReplayPlan::from_capture(capture, &scenario, Role::Client).unwrap();
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
        let server_plan = ReplayPlan::from_capture(capture, &scenario, Role::Server).unwrap();
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
            replay_server_detect(&mut server_stream, &plan, &scenario, &server_counters),
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
