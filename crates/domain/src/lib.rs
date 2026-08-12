use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::IpAddr};
use thiserror::Error;
use uuid::Uuid;

pub const SCENARIO_VERSION: u32 = 3;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    ExplicitProxy,
    TransparentProxy,
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
    pub topology: Topology,
    pub protocol: Protocol,
    pub client_agent_id: String,
    pub server_agent_id: String,
    pub proxy_addr: Option<String>,
    pub target_addr: String,
    pub source_ips: Vec<IpAddr>,
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
    pub observation_interfaces: Vec<String>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            version: SCENARIO_VERSION,
            id: Uuid::new_v4(),
            name: "기본 시험".into(),
            topology: Topology::TransparentProxy,
            protocol: Protocol::Tcp,
            client_agent_id: "client-1".into(),
            server_agent_id: "server-1".into(),
            proxy_addr: None,
            target_addr: "server:8080".into(),
            source_ips: vec![],
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
            observation_interfaces: vec![],
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
    pub fn migrate(mut self) -> Self {
        if self.version <= 1 {
            let (request, response) = match self.protocol {
                Protocol::Http1 | Protocol::Http2 => (
                    self.request.request_body_bytes,
                    self.request.response_body_bytes,
                ),
                _ => (self.tcp.tx_bytes, self.tcp.rx_bytes),
            };
            self.request_payload = Some(PayloadProfile::fixed(request));
            self.response_payload = Some(PayloadProfile::fixed(response));
            self.payload_mode = PayloadMode::Manual;
            self.capture_artifact_id = None;
            self.version = SCENARIO_VERSION;
        } else if self.version == 2 {
            self.version = SCENARIO_VERSION;
        }
        self
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
        if self.version != 1 && self.version != 2 && self.version != SCENARIO_VERSION {
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
        if self.client_agent_id == self.server_agent_id {
            return Err(ValidationError::Invalid(
                "client와 server agent는 달라야 합니다".into(),
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
        if !self.source_ips.is_empty() {
            return Err(ValidationError::Invalid(
                "source IP binding은 아직 구현되지 않았습니다".into(),
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
        if self.target_addr.parse::<std::net::SocketAddr>().is_err()
            && !valid_host_port(&self.target_addr)
        {
            return Err(ValidationError::Invalid(
                "target_addr는 host:port 형식이어야 합니다".into(),
            ));
        }
        if self.topology == Topology::ExplicitProxy
            && self
                .proxy_addr
                .as_deref()
                .filter(|a| valid_host_port(a))
                .is_none()
        {
            return Err(ValidationError::Invalid(
                "explicit_proxy에는 proxy_addr가 필요합니다".into(),
            ));
        }
        let unique: HashSet<_> = self.observation_interfaces.iter().collect();
        if unique.len() != self.observation_interfaces.len() {
            return Err(ValidationError::Invalid(
                "observation interface가 중복되었습니다".into(),
            ));
        }
        Ok(())
    }
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
    fn scenario_v2_migrates_to_v3_with_http2_defaults() {
        let scenario = Scenario {
            version: 2,
            ..Scenario::default()
        }
        .migrate();
        assert_eq!(scenario.version, 3);
        assert_eq!(scenario.http2.max_concurrent_streams, 100);
    }
    #[test]
    fn v1_payload_sizes_are_migrated_by_protocol() {
        let legacy = Scenario {
            version: 1,
            protocol: Protocol::Http1,
            request_payload: None,
            response_payload: None,
            request: HttpRequestProfile {
                request_body_bytes: 7,
                response_body_bytes: 11,
                ..Default::default()
            },
            ..Scenario::default()
        };
        let migrated = legacy.migrate();
        assert_eq!(migrated.version, SCENARIO_VERSION);
        assert_eq!(migrated.request_payload().byte_len(), 7);
        assert_eq!(migrated.response_payload().byte_len(), 11);
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
            topology: Topology::ExplicitProxy,
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

        let mut scenario = Scenario::default();
        scenario.source_ips.push("127.0.0.2".parse().unwrap());
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("source IP")
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
