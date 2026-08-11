use anyhow::Context;
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Multipart, Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use clap::Parser;
use futures::{Stream, StreamExt};
use proxy_tester_capture::{CaptureFormat, analyze_capture as analyze_tcp_capture};
use proxy_tester_domain::{MetricsSnapshot, PayloadKind, Scenario};
use proxy_tester_proto::v1::{
    AgentMessage, ArtifactChunk, ControlMessage, PrepareRun, SetPaused, StartRun, StopRun,
    agent_control_server::{AgentControl, AgentControlServer},
    agent_message, control_message,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions};
use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, RwLock, broadcast, mpsc},
};
use tonic::{Request, Response as GrpcResponse, Status};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "PROXY_TESTER_HTTP_ADDR", default_value = "0.0.0.0:8080")]
    http_addr: String,
    #[arg(long, env = "PROXY_TESTER_GRPC_ADDR", default_value = "0.0.0.0:50051")]
    grpc_addr: String,
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "sqlite://data/proxy-tester.db?mode=rwc"
    )]
    database_url: String,
    #[arg(long, env = "PROXY_TESTER_STATIC_DIR", default_value = "frontend/dist")]
    static_dir: String,
    #[arg(
        long,
        env = "PROXY_TESTER_ARTIFACT_DIR",
        default_value = "data/artifacts"
    )]
    artifact_dir: String,
}

#[derive(Clone)]
struct AgentSession {
    role: i32,
    hostname: String,
    interfaces: Vec<String>,
    last_seen_ms: i64,
    tx: mpsc::Sender<Result<ControlMessage, Status>>,
}

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    agents: Arc<RwLock<HashMap<String, AgentSession>>>,
    events: broadcast::Sender<String>,
    active_run: Arc<Mutex<Option<Uuid>>>,
    artifact_dir: Arc<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("proxy_control=info".parse()?),
        )
        .init();
    let args = Args::parse();
    if let Some(parent) = std::path::Path::new(
        args.database_url
            .trim_start_matches("sqlite://")
            .split('?')
            .next()
            .unwrap_or(""),
    )
    .parent()
    {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let db = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&args.database_url)
        .await
        .context("open sqlite")?;
    migrate(&db).await?;
    apply_retention(&db, 90).await?;
    tokio::fs::create_dir_all(&args.artifact_dir).await?;
    let (events, _) = broadcast::channel(1024);
    let state = AppState {
        db,
        agents: Default::default(),
        events,
        active_run: Default::default(),
        artifact_dir: Arc::new(args.artifact_dir.into()),
    };

    let grpc_state = state.clone();
    let grpc_addr = args.grpc_addr.parse()?;
    tokio::spawn(async move {
        info!(%grpc_addr,"agent gRPC listening");
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(AgentControlServer::new(ControlSvc(grpc_state)))
            .serve(grpc_addr)
            .await
        {
            error!(%e,"gRPC stopped");
        }
    });

    let static_dir = args.static_dir.clone();
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/agents", get(agents))
        .route("/api/scenarios", get(list_scenarios).post(save_scenario))
        .route("/api/scenarios/validate", post(validate_scenario))
        .route("/api/preflight", post(preflight))
        .route("/api/tls/certificates", post(generate_tls_certificate))
        .route("/api/artifacts", get(list_artifacts).post(upload_artifact))
        .route("/api/runs", get(list_runs).post(start_run))
        .route("/api/runs/active", get(active_run))
        .route("/api/runs/{id}", get(run_detail))
        .route("/api/runs/{id}/stop", post(stop_run))
        .route("/api/runs/{id}/pause", post(pause_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/events/ws", get(events_ws))
        .fallback_service(
            ServeDir::new(&static_dir)
                .not_found_service(ServeFile::new(format!("{static_dir}/index.html"))),
        )
        .layer(DefaultBodyLimit::max(513 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&args.http_addr).await?;
    info!(addr=%args.http_addr,"control HTTP listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Deserialize)]
struct CertificateRequest {
    server_name: String,
}

async fn generate_tls_certificate(
    Json(request): Json<CertificateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let server_name = request.server_name.trim();
    if server_name.is_empty() || server_name.len() > 253 {
        return Err(ApiError::bad("유효한 TLS server name이 필요합니다"));
    }
    let mut ca_params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    ca_params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(30);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Proxy Tester Local CA");
    let ca_key = KeyPair::generate().map_err(|e| ApiError::internal(e.to_string()))?;
    let ca = ca_params
        .self_signed(&ca_key)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let mut server_params = CertificateParams::new(vec![server_name.to_owned()])
        .map_err(|e| ApiError::bad(e.to_string()))?;
    server_params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    server_params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(30);
    server_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, server_name);
    let server_key = KeyPair::generate().map_err(|e| ApiError::internal(e.to_string()))?;
    let server = server_params
        .signed_by(&server_key, &ca, &ca_key)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ca_pem": ca.pem(),
        "server_cert_pem": server.pem(),
        "server_key_pem": server_key.serialize_pem(),
        "validity_days": 30
    })))
}

async fn migrate(db: &SqlitePool) -> anyhow::Result<()> {
    for sql in [
        "PRAGMA journal_mode=WAL",
        "CREATE TABLE IF NOT EXISTS scenarios(id TEXT PRIMARY KEY,name TEXT NOT NULL,body TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,scenario_id TEXT NOT NULL,status TEXT NOT NULL,started_at TEXT,finished_at TEXT,error TEXT,scenario_json TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS metric_samples(id INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,agent_id TEXT NOT NULL,role INTEGER NOT NULL,unix_ms INTEGER NOT NULL,metrics_json TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS idx_metrics_run_time ON metric_samples(run_id,unix_ms)",
        "CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT,unix_ms INTEGER NOT NULL,level TEXT NOT NULL,message TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY,name TEXT NOT NULL,sha256 TEXT NOT NULL UNIQUE,size_bytes INTEGER NOT NULL,format TEXT NOT NULL,packet_count INTEGER NOT NULL,captured_bytes INTEGER NOT NULL,path TEXT NOT NULL,created_at TEXT NOT NULL)",
    ] {
        sqlx::query(sql).execute(db).await?;
    }
    // SQLite has no portable ADD COLUMN IF NOT EXISTS; duplicate-column errors
    // are expected for databases that already contain this migration.
    let _ = sqlx::query("ALTER TABLE runs ADD COLUMN run_name TEXT")
        .execute(db)
        .await;
    let _ = sqlx::query("ALTER TABLE artifacts ADD COLUMN kind TEXT NOT NULL DEFAULT 'pcap'")
        .execute(db)
        .await;
    let _ = sqlx::query("ALTER TABLE artifacts ADD COLUMN analysis_json TEXT")
        .execute(db)
        .await;
    Ok(())
}

async fn apply_retention(db: &SqlitePool, days: i64) -> anyhow::Result<()> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    sqlx::query("DELETE FROM metric_samples WHERE run_id IN (SELECT id FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?)").bind(&cutoff).execute(db).await?;
    sqlx::query("DELETE FROM events WHERE run_id IN (SELECT id FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?)").bind(&cutoff).execute(db).await?;
    sqlx::query("DELETE FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?")
        .bind(&cutoff)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Serialize)]
struct AgentView {
    id: String,
    role: i32,
    hostname: String,
    interfaces: Vec<String>,
    last_seen_ms: i64,
    online: bool,
}
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}))
}
async fn agents(State(s): State<AppState>) -> Json<Vec<AgentView>> {
    let now = Utc::now().timestamp_millis();
    Json(
        s.agents
            .read()
            .await
            .iter()
            .map(|(id, a)| AgentView {
                id: id.clone(),
                role: a.role,
                hostname: a.hostname.clone(),
                interfaces: a.interfaces.clone(),
                last_seen_ms: a.last_seen_ms,
                online: now - a.last_seen_ms < 15_000,
            })
            .collect(),
    )
}

async fn validate_scenario(Json(sc): Json<Scenario>) -> Result<Json<serde_json::Value>, ApiError> {
    let sc = sc.migrate();
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(serde_json::json!({"valid":true})))
}
async fn preflight(
    State(s): State<AppState>,
    Json(sc): Json<Scenario>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sc = sc.migrate();
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    validate_payload_artifacts(&s.db, &sc).await?;
    let agents = s.agents.read().await;
    let client = agents.get(&sc.client_agent_id);
    let server = agents.get(&sc.server_agent_id);
    let mut checks = vec![
        serde_json::json!({"name":"client_agent","ok":client.is_some(),"detail":sc.client_agent_id}),
        serde_json::json!({"name":"server_agent","ok":server.is_some(),"detail":sc.server_agent_id}),
    ];
    for iface in &sc.observation_interfaces {
        let client_present = client.is_some_and(|a| a.interfaces.contains(iface));
        let server_present = server.is_some_and(|a| a.interfaces.contains(iface));
        checks.push(serde_json::json!({"name":"client_observation_interface","ok":client_present,"detail":iface}));
        checks.push(serde_json::json!({"name":"server_observation_interface","ok":server_present,"detail":iface}));
    }
    let ok = checks.iter().all(|v| v["ok"].as_bool() == Some(true));
    Ok(Json(
        serde_json::json!({"ok":ok,"checks":checks,"warnings":["route, MTU, offload와 물리 경로는 Linux 실장비에서 별도 확인해야 합니다"]}),
    ))
}

#[derive(Deserialize)]
struct ArtifactUpload {
    kind: Option<String>,
}

async fn upload_artifact(
    State(s): State<AppState>,
    Query(upload): Query<ArtifactUpload>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let kind = upload.kind.as_deref().unwrap_or("pcap");
    let limit = match kind {
        "payload" => 64 * 1024 * 1024_u64,
        "pcap" => 512 * 1024 * 1024_u64,
        _ => return Err(ApiError::bad("kind must be payload or pcap")),
    };
    let mut name = "capture.pcap".to_string();
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?
    {
        if field.name() == Some("file") {
            name = field.file_name().unwrap_or("capture.pcap").to_string();
            let temporary_path = s.artifact_dir.join(format!(".upload-{}", Uuid::new_v4()));
            let mut temporary = tokio::fs::File::create(&temporary_path)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            let mut field = field;
            let mut size = 0_u64;
            let mut digest = Sha256::new();
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| ApiError::bad(e.to_string()))?
            {
                size = size.saturating_add(chunk.len() as u64);
                if size > limit {
                    drop(temporary);
                    let _ = tokio::fs::remove_file(&temporary_path).await;
                    return Err(ApiError::bad(format!(
                        "{kind} artifact exceeds {} MiB",
                        limit / 1024 / 1024
                    )));
                }
                digest.update(&chunk);
                temporary
                    .write_all(&chunk)
                    .await
                    .map_err(|e| ApiError::internal(e.to_string()))?;
            }
            temporary
                .flush()
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            data = Some((temporary_path, size, format!("{:x}", digest.finalize())));
            break;
        }
    }
    let data = data.ok_or_else(|| ApiError::bad("file field가 필요합니다"))?;
    let (temporary_path, size, sha) = data;
    let (analysis, analysis_json): ((String, u64, u64), serde_json::Value) = match kind {
        "payload" => (
            ("raw".into(), 0, size),
            serde_json::json!({"supported_flow_count":0,"exclusions":{}}),
        ),
        "pcap" => {
            let capture = tokio::fs::read(&temporary_path)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            let (format, analyzed) = match analyze_tcp_capture(&capture) {
                Ok(analyzed) => analyzed,
                Err(error) => {
                    let _ = tokio::fs::remove_file(&temporary_path).await;
                    return Err(ApiError::bad(error.to_string()));
                }
            };
            let format = match format {
                CaptureFormat::Pcap => "pcap",
                CaptureFormat::PcapNg => "pcapng",
            };
            let summary = serde_json::json!({
                "supported_flow_count": analyzed.flows.len(),
                "retransmitted_bytes": analyzed.flows.iter().map(|flow|flow.retransmitted_bytes).sum::<u64>(),
                "exclusions": {
                    "non_tcp_packets": analyzed.exclusions.non_tcp_packets,
                    "fragmented_packets": analyzed.exclusions.fragmented_packets,
                    "truncated_packets": analyzed.exclusions.truncated_packets,
                    "unsupported_link_packets": analyzed.exclusions.unsupported_link_packets,
                    "incomplete_flows": analyzed.exclusions.incomplete_flows,
                    "encrypted_tls_flows": analyzed.exclusions.encrypted_tls_flows,
                }
            });
            (
                (
                    format.into(),
                    analyzed.packet_count,
                    analyzed.captured_bytes,
                ),
                summary,
            )
        }
        _ => unreachable!(),
    };
    let id = Uuid::new_v4();
    let path = s.artifact_dir.join(&sha);
    if tokio::fs::metadata(&path).await.is_err() {
        tokio::fs::rename(&temporary_path, &path)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
    } else {
        tokio::fs::remove_file(&temporary_path)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
    }
    let inserted = sqlx::query("INSERT INTO artifacts(id,name,sha256,size_bytes,format,packet_count,captured_bytes,path,created_at,kind,analysis_json) VALUES(?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(sha256) DO NOTHING")
        .bind(id.to_string()).bind(&name).bind(&sha).bind(size as i64).bind(&analysis.0).bind(analysis.1 as i64).bind(analysis.2 as i64).bind(path.to_string_lossy().to_string()).bind(Utc::now().to_rfc3339()).bind(kind).bind(analysis_json.to_string()).execute(&s.db).await?;
    let id = if inserted.rows_affected() == 0 {
        let existing = sqlx::query("SELECT id,kind FROM artifacts WHERE sha256=?")
            .bind(&sha)
            .fetch_one(&s.db)
            .await?;
        let existing_kind: String = existing.get("kind");
        if existing_kind != kind {
            return Err(ApiError::conflict(
                "identical bytes already exist under a different artifact kind",
            ));
        }
        Uuid::parse_str(existing.get::<String, _>("id").as_str())
            .map_err(|e| ApiError::internal(e.to_string()))?
    } else {
        id
    };
    Ok((
        StatusCode::CREATED,
        Json(
            serde_json::json!({"id":id,"kind":kind,"name":name,"sha256":sha,"size_bytes":size,"format":analysis.0,"packet_count":analysis.1,"captured_bytes":analysis.2,"analysis":analysis_json,"status":"validated"}),
        ),
    ))
}

async fn list_artifacts(
    State(s): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows = sqlx::query("SELECT id,kind,name,sha256,size_bytes,format,packet_count,captured_bytes,analysis_json,created_at FROM artifacts ORDER BY created_at DESC").fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().map(|r| { let analysis=r.get::<Option<String>,_>("analysis_json").and_then(|v|serde_json::from_str::<serde_json::Value>(&v).ok()).unwrap_or_default(); serde_json::json!({"id":r.get::<String,_>("id"),"kind":r.get::<String,_>("kind"),"name":r.get::<String,_>("name"),"sha256":r.get::<String,_>("sha256"),"size_bytes":r.get::<i64,_>("size_bytes"),"format":r.get::<String,_>("format"),"packet_count":r.get::<i64,_>("packet_count"),"captured_bytes":r.get::<i64,_>("captured_bytes"),"analysis":analysis,"created_at":r.get::<String,_>("created_at")})}).collect()))
}

#[cfg(test)]
fn analyze_capture(data: &[u8]) -> Result<(String, u64, u64), ApiError> {
    if data.len() < 12 {
        return Err(ApiError::bad("capture 파일이 너무 작습니다"));
    }
    let magic = &data[..4];
    if magic == [0x0a, 0x0d, 0x0d, 0x0a] {
        let (mut off, mut packets, mut bytes) = (0usize, 0u64, 0u64);
        while off + 12 <= data.len() {
            let block_type = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            let len = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
            if len < 12 || off + len > data.len() {
                break;
            }
            if block_type == 6 && len >= 32 {
                packets += 1;
                bytes += u32::from_le_bytes(data[off + 20..off + 24].try_into().unwrap()) as u64;
            }
            off += len;
        }
        return Ok(("pcapng".into(), packets, bytes));
    }
    let little = magic == [0xd4, 0xc3, 0xb2, 0xa1] || magic == [0x4d, 0x3c, 0xb2, 0xa1];
    let big = magic == [0xa1, 0xb2, 0xc3, 0xd4] || magic == [0xa1, 0xb2, 0x3c, 0x4d];
    if !little && !big {
        return Err(ApiError::bad("PCAP 또는 PCAPNG 형식이 아닙니다"));
    }
    if data.len() < 24 {
        return Err(ApiError::bad("PCAP global header가 잘렸습니다"));
    }
    let read = |b: &[u8]| {
        if little {
            u32::from_le_bytes(b.try_into().unwrap())
        } else {
            u32::from_be_bytes(b.try_into().unwrap())
        }
    };
    let (mut off, mut packets, mut bytes) = (24usize, 0u64, 0u64);
    while off + 16 <= data.len() {
        let incl = read(&data[off + 8..off + 12]) as usize;
        let orig = read(&data[off + 12..off + 16]) as u64;
        if off + 16 + incl > data.len() {
            return Err(ApiError::bad("PCAP packet record가 잘렸습니다"));
        }
        packets += 1;
        bytes += orig;
        off += 16 + incl;
    }
    Ok(("pcap".into(), packets, bytes))
}
async fn save_scenario(
    State(s): State<AppState>,
    Json(sc): Json<Scenario>,
) -> Result<Json<Scenario>, ApiError> {
    let sc = sc.migrate();
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    validate_payload_artifacts(&s.db, &sc).await?;
    let body = serde_json::to_string(&sc)?;
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO scenarios(id,name,body,created_at,updated_at) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,body=excluded.body,updated_at=excluded.updated_at")
        .bind(sc.id.to_string()).bind(&sc.name).bind(body).bind(&now).bind(&now).execute(&s.db).await?;
    Ok(Json(sc))
}
async fn list_scenarios(State(s): State<AppState>) -> Result<Json<Vec<Scenario>>, ApiError> {
    let rows = sqlx::query("SELECT body FROM scenarios ORDER BY updated_at DESC")
        .fetch_all(&s.db)
        .await?;
    Ok(Json(
        rows.into_iter()
            .filter_map(|r| {
                serde_json::from_str::<Scenario>(r.get("body"))
                    .ok()
                    .map(Scenario::migrate)
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StartRunPayload {
    Wrapped {
        scenario: Scenario,
        run_name: Option<String>,
    },
    Legacy(Scenario),
}

async fn start_run(
    State(s): State<AppState>,
    Json(payload): Json<StartRunPayload>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let (sc, requested_name) = match payload {
        StartRunPayload::Wrapped { scenario, run_name } => (scenario, run_name),
        StartRunPayload::Legacy(scenario) => (scenario, None),
    };
    let sc = sc.migrate();
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    let artifact_ids: HashSet<Uuid> = [sc.request_payload(), sc.response_payload()]
        .into_iter()
        .filter(|payload| payload.kind == PayloadKind::File)
        .filter_map(|payload| payload.artifact_id)
        .collect();
    let mut active = s.active_run.lock().await;
    if active.is_some() {
        return Err(ApiError::conflict("이미 실행 중인 시험이 있습니다"));
    }
    let sessions = s.agents.read().await;
    let client = sessions
        .get(&sc.client_agent_id)
        .ok_or_else(|| ApiError::bad("client agent가 연결되지 않았습니다"))?
        .clone();
    let server = sessions
        .get(&sc.server_agent_id)
        .ok_or_else(|| ApiError::bad("server agent가 연결되지 않았습니다"))?
        .clone();
    drop(sessions);
    let run_id = Uuid::new_v4();
    let json = serde_json::to_string(&sc)?;
    let started_at = Utc::now();
    let run_name = requested_name
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{} · {}",
                sc.name,
                started_at.format("%Y-%m-%d %H:%M:%S UTC")
            )
        });
    sqlx::query("INSERT INTO runs(id,scenario_id,status,scenario_json,run_name) VALUES(?,?,?,?,?)")
        .bind(run_id.to_string())
        .bind(sc.id.to_string())
        .bind("preparing")
        .bind(&json)
        .bind(&run_name)
        .execute(&s.db)
        .await?;
    let prepare = ControlMessage {
        body: Some(control_message::Body::Prepare(PrepareRun {
            run_id: run_id.to_string(),
            scenario_json: json,
        })),
    };
    send_artifacts(&s.db, &client, &artifact_ids).await?;
    send_artifacts(&s.db, &server, &artifact_ids).await?;
    client
        .tx
        .send(Ok(prepare.clone()))
        .await
        .map_err(|_| ApiError::internal("client channel closed"))?;
    server
        .tx
        .send(Ok(prepare))
        .await
        .map_err(|_| ApiError::internal("server channel closed"))?;
    let start_at = Utc::now().timestamp_millis() + 1000;
    let start = ControlMessage {
        body: Some(control_message::Body::Start(StartRun {
            run_id: run_id.to_string(),
            start_unix_ms: start_at,
        })),
    };
    client
        .tx
        .send(Ok(start.clone()))
        .await
        .map_err(|_| ApiError::internal("client channel closed"))?;
    server
        .tx
        .send(Ok(start))
        .await
        .map_err(|_| ApiError::internal("server channel closed"))?;
    sqlx::query("UPDATE runs SET status='running',started_at=? WHERE id=?")
        .bind(started_at.to_rfc3339())
        .bind(run_id.to_string())
        .execute(&s.db)
        .await?;
    *active = Some(run_id);
    let _ = s
        .events
        .send(serde_json::json!({"type":"run_started","run_id":run_id}).to_string());
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"id":run_id,"status":"running"})),
    ))
}

const ARTIFACT_CHUNK_BYTES: usize = 256 * 1024;

async fn validate_payload_artifacts(db: &SqlitePool, scenario: &Scenario) -> Result<(), ApiError> {
    for (direction, payload) in [
        ("request", scenario.request_payload()),
        ("response", scenario.response_payload()),
    ] {
        if payload.kind != PayloadKind::File {
            continue;
        }
        let id = payload.artifact_id.ok_or_else(|| {
            ApiError::bad(format!("{direction} file payload requires artifact_id"))
        })?;
        let row = sqlx::query("SELECT kind,size_bytes FROM artifacts WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(db)
            .await?
            .ok_or_else(|| {
                ApiError::bad(format!("{direction} payload artifact {id} does not exist"))
            })?;
        if row.get::<String, _>("kind") != "payload" {
            return Err(ApiError::bad(format!(
                "{direction} artifact {id} is not a payload artifact"
            )));
        }
        let stored_size = row.get::<i64, _>("size_bytes") as usize;
        if stored_size != payload.size_bytes {
            return Err(ApiError::bad(format!(
                "{direction} artifact size changed: scenario={}, stored={stored_size}",
                payload.size_bytes
            )));
        }
    }
    Ok(())
}

async fn send_artifacts(
    db: &SqlitePool,
    agent: &AgentSession,
    artifact_ids: &HashSet<Uuid>,
) -> Result<(), ApiError> {
    for artifact_id in artifact_ids {
        let row = sqlx::query("SELECT kind,sha256,size_bytes,path FROM artifacts WHERE id=?")
            .bind(artifact_id.to_string())
            .fetch_optional(db)
            .await?
            .ok_or_else(|| {
                ApiError::bad(format!("payload artifact {artifact_id} does not exist"))
            })?;
        let kind: String = row.get("kind");
        if kind != "payload" {
            return Err(ApiError::bad(format!(
                "artifact {artifact_id} is not a payload artifact"
            )));
        }
        let total_size = row.get::<i64, _>("size_bytes") as u64;
        if total_size > 64 * 1024 * 1024 {
            return Err(ApiError::bad(format!(
                "payload artifact {artifact_id} exceeds 64 MiB"
            )));
        }
        let sha256: String = row.get("sha256");
        let path: String = row.get("path");
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| ApiError::internal(format!("open artifact {artifact_id}: {e}")))?;
        let mut offset = 0_u64;
        loop {
            let mut data = vec![0; ARTIFACT_CHUNK_BYTES];
            let read = file
                .read(&mut data)
                .await
                .map_err(|e| ApiError::internal(format!("read artifact {artifact_id}: {e}")))?;
            data.truncate(read);
            let eof = offset + read as u64 == total_size;
            agent
                .tx
                .send(Ok(ControlMessage {
                    body: Some(control_message::Body::ArtifactChunk(ArtifactChunk {
                        artifact_id: artifact_id.to_string(),
                        offset,
                        data,
                        total_size,
                        sha256: sha256.clone(),
                        eof,
                    })),
                }))
                .await
                .map_err(|_| ApiError::internal("agent channel closed during artifact transfer"))?;
            offset += read as u64;
            if eof {
                break;
            }
            if read == 0 {
                return Err(ApiError::internal(format!(
                    "artifact {artifact_id} is shorter than database metadata"
                )));
            }
        }
    }
    Ok(())
}
async fn stop_run(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let current = *s.active_run.lock().await;
    if current != Some(id) {
        return Err(ApiError::bad("실행 중인 run이 아닙니다"));
    }
    for a in s.agents.read().await.values() {
        let _ =
            a.tx.send(Ok(ControlMessage {
                body: Some(control_message::Body::Stop(StopRun {
                    run_id: id.to_string(),
                })),
            }))
            .await;
    }
    finish_run(&s, id, "cancelled", None).await?;
    Ok(Json(serde_json::json!({"id":id,"status":"cancelled"})))
}
async fn pause_run(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    set_run_paused(&s, id, true).await
}
async fn resume_run(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    set_run_paused(&s, id, false).await
}
async fn set_run_paused(
    s: &AppState,
    id: Uuid,
    paused: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    if *s.active_run.lock().await != Some(id) {
        return Err(ApiError::bad("실행 중인 run이 아닙니다"));
    }
    let message = ControlMessage {
        body: Some(control_message::Body::SetPaused(SetPaused {
            run_id: id.to_string(),
            paused,
        })),
    };
    for agent in s.agents.read().await.values() {
        agent
            .tx
            .send(Ok(message.clone()))
            .await
            .map_err(|_| ApiError::internal("agent channel closed"))?;
    }
    let status = if paused { "paused" } else { "running" };
    sqlx::query("UPDATE runs SET status=? WHERE id=?")
        .bind(status)
        .bind(id.to_string())
        .execute(&s.db)
        .await?;
    let _ = s
        .events
        .send(serde_json::json!({"type":"run_state","run_id":id,"status":status}).to_string());
    Ok(Json(serde_json::json!({"id":id,"status":status})))
}
async fn finish_run(
    s: &AppState,
    id: Uuid,
    status: &str,
    error: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE runs SET status=?,finished_at=?,error=? WHERE id=?")
        .bind(status)
        .bind(Utc::now().to_rfc3339())
        .bind(error)
        .bind(id.to_string())
        .execute(&s.db)
        .await?;
    let mut active = s.active_run.lock().await;
    if *active == Some(id) {
        *active = None;
    }
    let _ = s
        .events
        .send(serde_json::json!({"type":"run_finished","run_id":id,"status":status}).to_string());
    Ok(())
}
async fn active_run(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"run_id":*s.active_run.lock().await}))
}
async fn list_runs(State(s): State<AppState>) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows=sqlx::query("SELECT id,scenario_id,run_name,status,started_at,finished_at,error,scenario_json FROM runs ORDER BY rowid DESC LIMIT 100").fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().map(|r|serde_json::json!({"id":r.get::<String,_>("id"),"scenario_id":r.get::<String,_>("scenario_id"),"run_name":r.get::<Option<String>,_>("run_name"),"status":r.get::<String,_>("status"),"started_at":r.get::<Option<String>,_>("started_at"),"finished_at":r.get::<Option<String>,_>("finished_at"),"error":r.get::<Option<String>,_>("error"),"scenario":redacted_scenario(r.get("scenario_json"))})).collect()))
}
fn redacted_scenario(body: &str) -> serde_json::Value {
    let mut value = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    if let Some(tls) = value
        .get_mut("tls")
        .and_then(serde_json::Value::as_object_mut)
    {
        tls.insert("server_key_pem".into(), serde_json::Value::Null);
    }
    for direction in ["request_payload", "response_payload"] {
        if let Some(payload) = value
            .get_mut(direction)
            .and_then(serde_json::Value::as_object_mut)
        {
            payload.insert("text".into(), serde_json::Value::String(String::new()));
        }
    }
    value
}
async fn run_detail(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let run = sqlx::query("SELECT * FROM runs WHERE id=?")
        .bind(id.to_string())
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    let samples=sqlx::query("SELECT agent_id,role,unix_ms,metrics_json FROM metric_samples WHERE run_id=? ORDER BY unix_ms").bind(id.to_string()).fetch_all(&s.db).await?;
    Ok(Json(
        serde_json::json!({"id":run.get::<String,_>("id"),"run_name":run.get::<Option<String>,_>("run_name"),"started_at":run.get::<Option<String>,_>("started_at"),"status":run.get::<String,_>("status"),"scenario":redacted_scenario(run.get("scenario_json")),"samples":samples.into_iter().map(|r|serde_json::json!({"agent_id":r.get::<String,_>("agent_id"),"role":r.get::<i64,_>("role"),"unix_ms":r.get::<i64,_>("unix_ms"),"metrics":serde_json::from_str::<serde_json::Value>(r.get("metrics_json")).unwrap_or_default()})).collect::<Vec<_>>() }),
    ))
}

async fn events_ws(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| stream_events(socket, s.events.subscribe()))
}
async fn stream_events(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    while let Ok(msg) = rx.recv().await {
        if socket.send(Message::Text(msg.into())).await.is_err() {
            break;
        }
    }
}

struct ControlSvc(AppState);
#[tonic::async_trait]
impl AgentControl for ControlSvc {
    type AgentStreamStream = Pin<Box<dyn Stream<Item = Result<ControlMessage, Status>> + Send>>;
    async fn agent_stream(
        &self,
        request: Request<tonic::Streaming<AgentMessage>>,
    ) -> Result<GrpcResponse<Self::AgentStreamStream>, Status> {
        let mut incoming = request.into_inner();
        let first = incoming
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("hello required"))??;
        let hello = match first.body {
            Some(agent_message::Body::Hello(v)) => v,
            _ => return Err(Status::invalid_argument("first message must be hello")),
        };
        let id = hello.agent_id.clone();
        let role = hello.role;
        let (tx, mut rx) = mpsc::channel(64);
        self.0.agents.write().await.insert(
            id.clone(),
            AgentSession {
                role,
                hostname: hello.hostname,
                interfaces: hello.interfaces,
                last_seen_ms: Utc::now().timestamp_millis(),
                tx,
            },
        );
        let state = self.0.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = incoming.next().await {
                match msg.body {
                    Some(agent_message::Body::Heartbeat(h)) => {
                        if let Some(a) = state.agents.write().await.get_mut(&id) {
                            a.last_seen_ms = h.unix_ms;
                        }
                    }
                    Some(agent_message::Body::Telemetry(t)) => {
                        if let Ok(m) = serde_json::from_str::<MetricsSnapshot>(&t.metrics_json) {
                            let _=sqlx::query("INSERT INTO metric_samples(run_id,agent_id,role,unix_ms,metrics_json) VALUES(?,?,?,?,?)").bind(&t.run_id).bind(&id).bind(role).bind(m.unix_ms).bind(&t.metrics_json).execute(&state.db).await;
                            let _=state.events.send(serde_json::json!({"type":"metrics","agent_id":id,"role":role,"data":m}).to_string());
                        }
                    }
                    Some(agent_message::Body::Event(e)) => {
                        let _ = sqlx::query(
                            "INSERT INTO events(run_id,unix_ms,level,message) VALUES(?,?,?,?)",
                        )
                        .bind(&e.run_id)
                        .bind(Utc::now().timestamp_millis())
                        .bind(&e.level)
                        .bind(&e.message)
                        .execute(&state.db)
                        .await;
                        if e.message == "run_completed"
                            && let Ok(run_id) = Uuid::parse_str(&e.run_id)
                            && *state.active_run.lock().await == Some(run_id)
                        {
                            let _ = finish_run(&state, run_id, "completed", None).await;
                        }
                        let _=state.events.send(serde_json::json!({"type":"agent_event","agent_id":id,"run_id":e.run_id,"level":e.level,"message":e.message}).to_string());
                    }
                    _ => {}
                }
            }
            state.agents.write().await.remove(&id);
            warn!(agent=%id,"agent disconnected");
        });
        let output = async_stream::stream! {while let Some(msg)=rx.recv().await{yield msg;}};
        Ok(GrpcResponse::new(Box::pin(output)))
    }
}

#[derive(Debug)]
struct ApiError(StatusCode, String);
impl ApiError {
    fn bad(s: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, s.into())
    }
    fn conflict(s: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, s.into())
    }
    fn not_found(s: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, s.into())
    }
    fn internal(s: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, s.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error":self.1}))).into_response()
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        error!(%e);
        Self::internal("database error")
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::bad(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::analyze_capture;

    #[test]
    fn validates_empty_classic_pcap() {
        let bytes = [
            0xd4, 0xc3, 0xb2, 0xa1, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 1, 0, 0,
            0,
        ];
        let result = analyze_capture(&bytes).expect("valid pcap");
        assert_eq!(result, ("pcap".to_string(), 0, 0));
    }

    #[test]
    fn rejects_unknown_capture() {
        assert!(analyze_capture(b"not a capture file").is_err());
    }
}
