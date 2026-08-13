use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use thiserror::Error;
use uuid::Uuid;

pub const SCENARIO_VERSION: u32 = 4;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    ExplicitProxy,
    TransparentProxy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScenarioPath {
    ManagedDirect {
        profile_revision_id: Uuid,
        server_port: u16,
    },
    ExplicitProxy {
        client_node_id: String,
        client_bind_ip: IpAddr,
        server_node_id: String,
        server_listen_ip: IpAddr,
        server_port: u16,
        proxy_addr: String,
    },
}
impl Default for ScenarioPath {
    fn default() -> Self {
        Self::ManagedDirect {
            profile_revision_id: Uuid::new_v4(),
            server_port: 8080,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointProfile {
    pub node_id: String,
    pub interface_name: String,
    /// First address and prefix, for example `10.20.0.10/24`.
    pub start_cidr: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkProfileDraft {
    pub id: Uuid,
    pub name: String,
    pub client_endpoint: EndpointProfile,
    pub server_endpoint: EndpointProfile,
    pub mtu: u32,
    pub diagnostic_port: u16,
    pub path_probe_enabled: bool,
}
impl Default for NetworkProfileDraft {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "Managed direct network".into(),
            client_endpoint: EndpointProfile {
                node_id: "node-1".into(),
                interface_name: "eth1".into(),
                start_cidr: "10.20.0.10/24".into(),
                count: 16,
            },
            server_endpoint: EndpointProfile {
                node_id: "node-1".into(),
                interface_name: "eth2".into(),
                start_cidr: "10.20.0.100/24".into(),
                count: 16,
            },
            mtu: 1370,
            diagnostic_port: 39000,
            path_probe_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkProfileRevision {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub revision: u32,
    pub sha256: String,
    pub body: NetworkProfileDraft,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tcp,
    Http1,
    Http2,
    Connect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Scenario {
    pub version: u32,
    pub id: Uuid,
    pub name: String,
    pub path: ScenarioPath,
    pub protocol: Protocol,
    pub virtual_clients: u32,
    pub duration_secs: u64,
    pub warmup_secs: u64,
    pub load_stages: Vec<LoadStage>,
    pub request: HttpRequestProfile,
    pub http2: Http2Profile,
    pub tcp: TcpPayloadProfile,
    /// Scenario v2 payloads. `None` is retained solely for v1 JSON migration.
    pub request_payload: Option<PayloadProfile>,
    pub response_payload: Option<PayloadProfile>,
    pub payload_mode: PayloadMode,
    pub capture_artifact_id: Option<Uuid>,
    pub tls: TlsProfile,
    pub timeouts: TimeoutProfile,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            version: SCENARIO_VERSION,
            id: Uuid::new_v4(),
            name: "기본 시험".into(),
            path: ScenarioPath::default(),
            protocol: Protocol::Tcp,
            virtual_clients: 1,
            duration_secs: 10,
            warmup_secs: 0,
            load_stages: vec![],
            request: HttpRequestProfile::default(),
            http2: Http2Profile::default(),
            tcp: TcpPayloadProfile::default(),
            request_payload: Some(PayloadProfile::fixed(64)),
            response_payload: Some(PayloadProfile::fixed(64)),
            payload_mode: PayloadMode::Manual,
            capture_artifact_id: None,
            tls: TlsProfile::default(),
            timeouts: TimeoutProfile::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Http2Profile {
    pub max_concurrent_streams: u32,
}
impl Default for Http2Profile {
    fn default() -> Self {
        Self {
            max_concurrent_streams: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PayloadMode {
    #[default]
    Manual,
    CaptureReplay,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Empty,
    #[default]
    Fixed,
    Text,
    File,
    Random,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RandomFormat {
    #[default]
    Binary,
    PrintableAscii,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PayloadProfile {
    pub kind: PayloadKind,
    pub size_bytes: usize,
    pub text: String,
    pub artifact_id: Option<Uuid>,
    pub random_format: RandomFormat,
}
impl Default for PayloadProfile {
    fn default() -> Self {
        Self::fixed(0)
    }
}
impl PayloadProfile {
    pub fn fixed(size_bytes: usize) -> Self {
        Self {
            kind: PayloadKind::Fixed,
            size_bytes,
            text: String::new(),
            artifact_id: None,
            random_format: RandomFormat::Binary,
        }
    }
    pub fn byte_len(&self) -> usize {
        match self.kind {
            PayloadKind::Empty => 0,
            PayloadKind::Text => self.text.len(),
            _ => self.size_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadStageMode {
    Ramp,
    Hold,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadStage {
    pub name: String,
    pub mode: LoadStageMode,
    pub duration_secs: u64,
    pub target_virtual_clients: u32,
    pub include_in_results: bool,
}

impl Default for LoadStage {
    fn default() -> Self {
        Self {
            name: "Hold".into(),
            mode: LoadStageMode::Hold,
            duration_secs: 10,
            target_virtual_clients: 1,
            include_in_results: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpRequestProfile {
    pub method: String,
    pub path: String,
    pub host: String,
    pub request_body_bytes: usize,
    pub response_body_bytes: usize,
    pub keep_alive: bool,
    pub transactions_per_connection: u32,
    pub think_time_ms: u64,
}
impl Default for HttpRequestProfile {
    fn default() -> Self {
        Self {
            method: "GET".into(),
            path: "/".into(),
            host: "proxy-tester.local".into(),
            request_body_bytes: 0,
            response_body_bytes: 128,
            keep_alive: true,
            transactions_per_connection: 1,
            think_time_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TcpPayloadProfile {
    pub tx_bytes: usize,
    pub rx_bytes: usize,
}
impl Default for TcpPayloadProfile {
    fn default() -> Self {
        Self {
            tx_bytes: 64,
            rx_bytes: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsProfile {
    pub enabled: bool,
    pub verify_peer: bool,
    pub version: TlsVersion,
    pub cipher_suite: Option<String>,
    pub server_name: String,
    pub ca_pem: Option<String>,
    pub server_cert_pem: Option<String>,
    pub server_key_pem: Option<String>,
}
impl Default for TlsProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            verify_peer: false,
            version: TlsVersion::Tls13,
            cipher_suite: None,
            server_name: "proxy-tester.local".into(),
            ca_pem: None,
            server_cert_pem: None,
            server_key_pem: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TlsVersion {
    Tls12,
    #[default]
    Tls13,
}

impl TlsProfile {
    pub fn cipher_is_compatible(&self) -> bool {
        let Some(cipher) = self.cipher_suite.as_deref() else {
            return true;
        };
        matches!(
            (self.version, cipher),
            (
                TlsVersion::Tls13,
                "TLS13_AES_256_GCM_SHA384"
                    | "TLS13_AES_128_GCM_SHA256"
                    | "TLS13_CHACHA20_POLY1305_SHA256"
            ) | (
                TlsVersion::Tls12,
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"
                    | "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"
                    | "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
                    | "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"
                    | "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
                    | "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"
            )
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeoutProfile {
    pub connect_ms: u64,
    pub proxy_connect_ms: u64,
    pub response_ms: u64,
}
impl Default for TimeoutProfile {
    fn default() -> Self {
        Self {
            connect_ms: 3000,
            proxy_connect_ms: 3000,
            response_ms: 5000,
        }
    }
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("{0}")]
    Invalid(String),
}

impl Scenario {
    pub fn migrate(self) -> Self {
        self
    }

    pub fn is_explicit_proxy(&self) -> bool {
        matches!(self.path, ScenarioPath::ExplicitProxy { .. })
    }
    pub fn client_node_id(&self) -> Option<&str> {
        match &self.path {
            ScenarioPath::ManagedDirect { .. } => None,
            ScenarioPath::ExplicitProxy { client_node_id, .. } => Some(client_node_id),
        }
    }
    pub fn server_node_id(&self) -> Option<&str> {
        match &self.path {
            ScenarioPath::ManagedDirect { .. } => None,
            ScenarioPath::ExplicitProxy { server_node_id, .. } => Some(server_node_id),
        }
    }
    pub fn proxy_addr(&self) -> Option<&str> {
        match &self.path {
            ScenarioPath::ExplicitProxy { proxy_addr, .. } => Some(proxy_addr),
            _ => None,
        }
    }
    pub fn server_port(&self) -> u16 {
        match self.path {
            ScenarioPath::ManagedDirect { server_port, .. }
            | ScenarioPath::ExplicitProxy { server_port, .. } => server_port,
        }
    }
    pub fn target_addr(&self) -> String {
        match &self.path {
            ScenarioPath::ExplicitProxy {
                server_listen_ip,
                server_port,
                ..
            } => format!("{server_listen_ip}:{server_port}"),
            ScenarioPath::ManagedDirect { server_port, .. } => format!("127.0.0.1:{server_port}"),
        }
    }

    pub fn request_payload(&self) -> PayloadProfile {
        self.request_payload.clone().unwrap_or_else(|| {
            PayloadProfile::fixed(
                if matches!(self.protocol, Protocol::Http1 | Protocol::Http2) {
                    self.request.request_body_bytes
                } else {
                    self.tcp.tx_bytes
                },
            )
        })
    }
    pub fn response_payload(&self) -> PayloadProfile {
        self.response_payload.clone().unwrap_or_else(|| {
            PayloadProfile::fixed(
                if matches!(self.protocol, Protocol::Http1 | Protocol::Http2) {
                    self.request.response_body_bytes
                } else {
                    self.tcp.rx_bytes
                },
            )
        })
    }
    pub fn effective_duration_secs(&self) -> u64 {
        if self.load_stages.is_empty() {
            self.duration_secs
        } else {
            self.load_stages
                .iter()
                .map(|stage| stage.duration_secs)
                .sum()
        }
    }

    pub fn maximum_virtual_clients(&self) -> u32 {
        if self.load_stages.is_empty() {
            self.virtual_clients
        } else {
            self.load_stages
                .iter()
                .map(|stage| stage.target_virtual_clients)
                .max()
                .unwrap_or(0)
        }
    }

    pub fn load_at(&self, elapsed_ms: u64) -> (usize, u32, bool) {
        if self.load_stages.is_empty() {
            return (0, self.virtual_clients, true);
        }
        let mut start_ms = 0_u64;
        let mut previous_target = 0_u32;
        for (index, stage) in self.load_stages.iter().enumerate() {
            let duration_ms = stage.duration_secs.saturating_mul(1000);
            let end_ms = start_ms.saturating_add(duration_ms);
            if elapsed_ms < end_ms || index + 1 == self.load_stages.len() {
                let target = match stage.mode {
                    LoadStageMode::Hold => stage.target_virtual_clients,
                    LoadStageMode::Ramp if duration_ms > 0 => {
                        let progress =
                            elapsed_ms.saturating_sub(start_ms) as f64 / duration_ms as f64;
                        (previous_target as f64
                            + (stage.target_virtual_clients as f64 - previous_target as f64)
                                * progress)
                            .round()
                            .max(0.0) as u32
                    }
                    LoadStageMode::Ramp => stage.target_virtual_clients,
                };
                return (index, target, stage.include_in_results);
            }
            start_ms = end_ms;
            previous_target = stage.target_virtual_clients;
        }
        (self.load_stages.len() - 1, previous_target, false)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.version != SCENARIO_VERSION {
            return Err(ValidationError::Invalid(format!(
                "지원하지 않는 scenario version: {}",
                self.version
            )));
        }
        if self.payload_mode == PayloadMode::CaptureReplay && self.capture_artifact_id.is_none() {
            return Err(ValidationError::Invalid(
                "PCAP session replay requires an analyzed capture artifact".into(),
            ));
        }
        for (direction, payload) in [
            ("request", self.request_payload()),
            ("response", self.response_payload()),
        ] {
            if payload.byte_len() > MAX_PAYLOAD_BYTES {
                return Err(ValidationError::Invalid(format!(
                    "{direction} payload exceeds 64 MiB"
                )));
            }
            if payload.kind == PayloadKind::File && payload.artifact_id.is_none() {
                return Err(ValidationError::Invalid(format!(
                    "{direction} file payload requires artifact_id"
                )));
            }
            if payload.kind != PayloadKind::File && payload.artifact_id.is_some() {
                return Err(ValidationError::Invalid(format!(
                    "{direction} artifact_id is only valid for file payload"
                )));
            }
        }
        if self.name.trim().is_empty() {
            return Err(ValidationError::Invalid(
                "name은 비어 있을 수 없습니다".into(),
            ));
        }
        if self.load_stages.is_empty() && self.duration_secs == 0 {
            return Err(ValidationError::Invalid(
                "duration_secs는 1 이상이어야 합니다".into(),
            ));
        }
        if self.load_stages.is_empty() && self.virtual_clients == 0 {
            return Err(ValidationError::Invalid(
                "virtual_clients는 1 이상이어야 합니다".into(),
            ));
        }
        if self.load_stages.len() > 50 {
            return Err(ValidationError::Invalid(
                "load stage는 최대 50개까지 설정할 수 있습니다".into(),
            ));
        }
        for stage in &self.load_stages {
            if stage.name.trim().is_empty() || stage.duration_secs == 0 {
                return Err(ValidationError::Invalid(
                    "각 load stage에는 이름과 1초 이상의 시간이 필요합니다".into(),
                ));
            }
        }
        if !self.load_stages.is_empty() && self.maximum_virtual_clients() == 0 {
            return Err(ValidationError::Invalid(
                "load stage 중 하나는 1명 이상의 가상 클라이언트를 사용해야 합니다".into(),
            ));
        }
        if self.tls.enabled {
            if !self.tls.cipher_is_compatible() {
                return Err(ValidationError::Invalid(format!(
                    "cipher suite {:?} is not supported by {:?}",
                    self.tls.cipher_suite, self.tls.version
                )));
            }
            if self.tls.server_name.trim().is_empty() {
                return Err(ValidationError::Invalid(
                    "TLS server_name이 필요합니다".into(),
                ));
            }
            if self
                .tls
                .server_cert_pem
                .as_deref()
                .is_none_or(str::is_empty)
                || self.tls.server_key_pem.as_deref().is_none_or(str::is_empty)
            {
                return Err(ValidationError::Invalid(
                    "TLS server certificate와 private key가 필요합니다".into(),
                ));
            }
            if self.tls.verify_peer && self.tls.ca_pem.as_deref().is_none_or(str::is_empty) {
                return Err(ValidationError::Invalid(
                    "TLS 인증서 검증을 사용하려면 CA PEM이 필요합니다".into(),
                ));
            }
        }
        if self.warmup_secs > 0 {
            return Err(ValidationError::Invalid(
                "warmup 구간은 아직 구현되지 않았습니다".into(),
            ));
        }
        if self.protocol == Protocol::Connect {
            return Err(ValidationError::Invalid(
                "CONNECT는 시험 프로토콜이 아닙니다. explicit proxy와 TCP를 선택하세요".into(),
            ));
        }
        if self.protocol == Protocol::Http2 {
            if !self.tls.enabled {
                return Err(ValidationError::Invalid(
                    "HTTP/2 requires TLS; h2c is not supported".into(),
                ));
            }
            if !(1..=1000).contains(&self.http2.max_concurrent_streams) {
                return Err(ValidationError::Invalid(
                    "HTTP/2 max_concurrent_streams must be between 1 and 1000".into(),
                ));
            }
        }
        if self.protocol == Protocol::Http1
            && !self.request.keep_alive
            && self.request.transactions_per_connection != 1
        {
            return Err(ValidationError::Invalid(
                "여러 HTTP transaction을 한 연결에서 실행하려면 keep-alive가 필요합니다".into(),
            ));
        }
        match &self.path {
            ScenarioPath::ManagedDirect {
                profile_revision_id,
                server_port,
            } => {
                if profile_revision_id.is_nil() {
                    return Err(ValidationError::Invalid(
                        "managed_direct requires profile_revision_id".into(),
                    ));
                }
                if *server_port == 0 {
                    return Err(ValidationError::Invalid(
                        "server_port must be non-zero".into(),
                    ));
                }
            }
            ScenarioPath::ExplicitProxy {
                client_node_id,
                client_bind_ip,
                server_node_id,
                server_listen_ip,
                server_port,
                proxy_addr,
            } => {
                if client_node_id.trim().is_empty() || server_node_id.trim().is_empty() {
                    return Err(ValidationError::Invalid(
                        "explicit_proxy requires client and server node IDs".into(),
                    ));
                }
                if !client_bind_ip.is_ipv4() || !server_listen_ip.is_ipv4() {
                    return Err(ValidationError::Invalid(
                        "explicit_proxy endpoint addresses must be IPv4".into(),
                    ));
                }
                if *server_port == 0 || !valid_host_port(proxy_addr) {
                    return Err(ValidationError::Invalid(
                        "explicit_proxy requires server port and proxy host:port".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl NetworkProfileDraft {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.name.trim().is_empty() {
            return Err(ValidationError::Invalid(
                "network profile name is required".into(),
            ));
        }
        if !(576..=9216).contains(&self.mtu) {
            return Err(ValidationError::Invalid(
                "MTU must be between 576 and 9216".into(),
            ));
        }
        if self.diagnostic_port == 0 {
            return Err(ValidationError::Invalid(
                "diagnostic_port must be non-zero".into(),
            ));
        }
        let client = ipv4_range(&self.client_endpoint)?;
        let server = ipv4_range(&self.server_endpoint)?;
        if client.2 != server.2 || client.3 != server.3 {
            return Err(ValidationError::Invalid(
                "client and server pools must use the same IPv4 subnet".into(),
            ));
        }
        if client.0 <= server.1 && server.0 <= client.1 {
            return Err(ValidationError::Invalid(
                "client and server pools must not overlap".into(),
            ));
        }
        if self.client_endpoint.node_id == self.server_endpoint.node_id
            && self.client_endpoint.interface_name == self.server_endpoint.interface_name
        {
            return Err(ValidationError::Invalid(
                "client and server endpoints require different interfaces on the same node".into(),
            ));
        }
        Ok(())
    }
}

fn ipv4_range(endpoint: &EndpointProfile) -> Result<(u32, u32, u32, u8), ValidationError> {
    if endpoint.node_id.trim().is_empty() || endpoint.interface_name.trim().is_empty() {
        return Err(ValidationError::Invalid(
            "endpoint node and interface are required".into(),
        ));
    }
    if !(1..=4096).contains(&endpoint.count) {
        return Err(ValidationError::Invalid(
            "endpoint pool count must be between 1 and 4096".into(),
        ));
    }
    let (address, prefix) = endpoint
        .start_cidr
        .split_once('/')
        .ok_or_else(|| ValidationError::Invalid("start_cidr must be IPv4/prefix".into()))?;
    let address: std::net::Ipv4Addr = address
        .parse()
        .map_err(|_| ValidationError::Invalid("start_cidr must be IPv4/prefix".into()))?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| ValidationError::Invalid("invalid IPv4 prefix".into()))?;
    if !(1..=30).contains(&prefix) {
        return Err(ValidationError::Invalid(
            "IPv4 prefix must be between 1 and 30".into(),
        ));
    }
    let start = u32::from(address);
    let mask = u32::MAX << (32 - prefix);
    let network = start & mask;
    let broadcast = network | !mask;
    let end = start
        .checked_add(endpoint.count - 1)
        .ok_or_else(|| ValidationError::Invalid("IP pool overflows".into()))?;
    if start <= network || end >= broadcast {
        return Err(ValidationError::Invalid(
            "IP pool includes network/broadcast or leaves subnet".into(),
        ));
    }
    Ok((start, end, network, prefix))
}

fn valid_host_port(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub run_id: Uuid,
    pub unix_ms: i64,
    pub elapsed_ms: u64,
    pub load_stage_index: u32,
    pub desired_virtual_clients: u32,
    pub included_in_results: bool,
    pub connections_attempted: u64,
    pub connections_established: u64,
    pub connections_failed: u64,
    pub active_connections: u64,
    #[serde(default)]
    pub active_connections_avg: f64,
    #[serde(default)]
    pub active_connections_min: u64,
    #[serde(default)]
    pub active_connections_max: u64,
    pub transactions: u64,
    pub transaction_errors: u64,
    #[serde(default)]
    pub timeout_errors: u64,
    #[serde(default)]
    pub reset_errors: u64,
    #[serde(default)]
    pub tls_handshake_errors: u64,
    #[serde(default)]
    pub proxy_connect_errors: u64,
    #[serde(default)]
    pub http_error_responses: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_random_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_random_sha256: Option<String>,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub packets_tx: u64,
    pub packets_rx: u64,
    pub cps: f64,
    pub tps: f64,
    pub tx_bps: f64,
    pub rx_bps: f64,
    pub tx_pps: f64,
    pub rx_pps: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,
    pub tcp_connect_latency_p50_ms: f64,
    pub tcp_connect_latency_p95_ms: f64,
    pub tcp_connect_latency_p99_ms: f64,
    pub http_latency_p50_ms: f64,
    pub http_latency_p95_ms: f64,
    pub http_latency_p99_ms: f64,
    pub wire_tx_bytes: u64,
    pub wire_rx_bytes: u64,
    pub wire_tx_bps: f64,
    pub wire_rx_bps: f64,
    pub wire_tx_pps: f64,
    pub wire_rx_pps: f64,
    pub tcp_retransmissions: u64,
    pub tcp_retransmissions_per_sec: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_is_valid() {
        assert!(Scenario::default().validate().is_ok());
    }

    #[test]
    fn http2_requires_tls_and_bounded_stream_concurrency() {
        let mut scenario = Scenario::default();
        scenario.protocol = Protocol::Http2;
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("requires TLS")
        );
        scenario.tls.enabled = true;
        scenario.tls.server_cert_pem = Some("certificate".into());
        scenario.tls.server_key_pem = Some("key".into());
        scenario.http2.max_concurrent_streams = 0;
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("max_concurrent_streams")
        );
        scenario.http2.max_concurrent_streams = 100;
        assert!(scenario.validate().is_ok());
    }

    #[test]
    fn scenario_v4_rejects_unsealed_managed_profile() {
        let mut scenario = Scenario::default();
        scenario.path = ScenarioPath::ManagedDirect {
            profile_revision_id: Uuid::nil(),
            server_port: 8080,
        };
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("profile_revision_id")
        );
    }

    #[test]
    fn text_uses_utf8_length_and_payload_limit_is_enforced() {
        let mut scenario = Scenario::default();
        scenario.request_payload = Some(PayloadProfile {
            kind: PayloadKind::Text,
            text: "가".into(),
            ..Default::default()
        });
        assert_eq!(scenario.request_payload().byte_len(), 3);
        scenario.response_payload = Some(PayloadProfile::fixed(MAX_PAYLOAD_BYTES + 1));
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
    }
    #[test]
    fn capture_replay_requires_an_artifact_reference() {
        let mut scenario = Scenario::default();
        scenario.payload_mode = PayloadMode::CaptureReplay;
        assert!(scenario.validate().is_err());
        scenario.capture_artifact_id = Some(Uuid::new_v4());
        assert!(scenario.validate().is_ok());
    }
    #[test]
    fn explicit_requires_proxy() {
        let s = Scenario {
            path: ScenarioPath::ExplicitProxy {
                client_node_id: "client-node".into(),
                client_bind_ip: "192.0.2.10".parse().unwrap(),
                server_node_id: "server-node".into(),
                server_listen_ip: "192.0.2.20".parse().unwrap(),
                server_port: 8080,
                proxy_addr: String::new(),
            },
            ..Scenario::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn tls_version_and_cipher_must_be_compatible() {
        let mut profile = TlsProfile::default();
        assert_eq!(profile.version, TlsVersion::Tls13);
        assert!(!profile.verify_peer);
        profile.cipher_suite = Some("TLS13_AES_128_GCM_SHA256".into());
        assert!(profile.cipher_is_compatible());
        profile.version = TlsVersion::Tls12;
        assert!(!profile.cipher_is_compatible());
        profile.cipher_suite = Some("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256".into());
        assert!(profile.cipher_is_compatible());
        profile.cipher_suite = Some("TLS_RSA_WITH_3DES_EDE_CBC_SHA".into());
        assert!(!profile.cipher_is_compatible());
    }

    #[test]
    fn unsupported_features_fail_instead_of_being_ignored() {
        let mut scenario = Scenario::default();
        scenario.tls.enabled = true;
        assert!(scenario.validate().unwrap_err().to_string().contains("TLS"));

        let scenario = Scenario {
            warmup_secs: 1,
            ..Scenario::default()
        };
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("warmup")
        );
    }

    #[test]
    fn network_profile_validates_subnet_pool_and_interfaces() {
        let profile = NetworkProfileDraft::default();
        assert!(profile.validate().is_ok());
        let mut overlap = profile.clone();
        overlap.server_endpoint.start_cidr = "10.20.0.20/24".into();
        assert!(
            overlap
                .validate()
                .unwrap_err()
                .to_string()
                .contains("overlap")
        );
        let mut too_large = profile;
        too_large.client_endpoint.count = 4097;
        assert!(
            too_large
                .validate()
                .unwrap_err()
                .to_string()
                .contains("4096")
        );
    }

    #[test]
    fn multiple_http_transactions_require_keep_alive() {
        let mut scenario = Scenario {
            protocol: Protocol::Http1,
            ..Scenario::default()
        };
        scenario.request.keep_alive = false;
        scenario.request.transactions_per_connection = 2;
        assert!(scenario.validate().is_err());

        scenario.request.keep_alive = true;
        assert!(scenario.validate().is_ok());
        scenario.request.transactions_per_connection = 0;
        assert!(scenario.validate().is_ok());
    }

    #[test]
    fn staged_load_interpolates_and_holds() {
        let scenario = Scenario {
            load_stages: vec![
                LoadStage {
                    name: "ramp".into(),
                    mode: LoadStageMode::Ramp,
                    duration_secs: 10,
                    target_virtual_clients: 100,
                    include_in_results: false,
                },
                LoadStage {
                    name: "hold".into(),
                    mode: LoadStageMode::Hold,
                    duration_secs: 20,
                    target_virtual_clients: 100,
                    include_in_results: true,
                },
            ],
            ..Scenario::default()
        };
        assert_eq!(scenario.effective_duration_secs(), 30);
        assert_eq!(scenario.maximum_virtual_clients(), 100);
        assert_eq!(scenario.load_at(5_000), (0, 50, false));
        assert_eq!(scenario.load_at(15_000), (1, 100, true));
        assert!(scenario.validate().is_ok());
    }
}
