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
use proxy_tester_domain::{
    MetricsSnapshot, NetworkProfileDraft, NetworkProfileRevision, PayloadKind, PayloadMode,
    Protocol, Scenario, ScenarioPath,
};
use proxy_tester_proto::v1::{
    AgentMessage, ArtifactChunk, ControlMessage, NetworkCommand, NetworkProgress, PrepareRun,
    SetPaused, StartRun, StopRun,
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
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
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
    #[arg(long, env = "PROXY_TESTER_RETENTION_DAYS", default_value_t = 90)]
    retention_days: i64,
    #[arg(long, env = "PROXY_TESTER_AGENT_GRACE_SECS", default_value_t = 10)]
    agent_grace_secs: u64,
    #[arg(long, env = "PROXY_TESTER_COMMAND_TIMEOUT_SECS", default_value_t = 10)]
    command_timeout_secs: u64,
}

#[derive(Clone)]
struct AgentSession {
    instance_id: String,
    generation: Uuid,
    role: i32,
    hostname: String,
    interfaces: Vec<String>,
    inventory_json: String,
    last_seen_ms: i64,
    tx: mpsc::Sender<Result<ControlMessage, Status>>,
}

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    database_url: Arc<String>,
    database_fallback: bool,
    agents: Arc<RwLock<HashMap<String, AgentSession>>>,
    events: broadcast::Sender<String>,
    active_run: Arc<Mutex<Option<Uuid>>>,
    run_agents: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
    completed_agents: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
    expected_endpoints: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
    pending_acks: Arc<Mutex<HashMap<String, oneshot::Sender<CommandAckResult>>>>,
    pending_network: Arc<Mutex<HashMap<String, oneshot::Sender<NetworkProgress>>>>,
    agent_grace_secs: u64,
    command_timeout_secs: u64,
    artifact_dir: Arc<std::path::PathBuf>,
}

#[derive(Debug)]
struct CommandAckResult {
    agent_id: String,
    ok: bool,
    error: String,
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
    let (db, database_url, database_fallback) = open_database(&args.database_url).await?;
    migrate(&db).await?;
    sqlx::query("UPDATE runs SET status='failed',finished_at=?,error='control_restarted' WHERE status IN ('preparing','running','paused','degraded')")
        .bind(Utc::now().to_rfc3339()).execute(&db).await?;
    if args.retention_days > 0 {
        apply_retention(&db, args.retention_days).await?;
        cleanup_orphan_artifacts(&db, args.retention_days).await?;
    }
    tokio::fs::create_dir_all(&args.artifact_dir).await?;
    let (events, _) = broadcast::channel(1024);
    let state = AppState {
        db,
        database_url: Arc::new(database_url),
        database_fallback,
        agents: Default::default(),
        events,
        active_run: Default::default(),
        run_agents: Default::default(),
        completed_agents: Default::default(),
        expected_endpoints: Default::default(),
        pending_acks: Default::default(),
        pending_network: Default::default(),
        agent_grace_secs: args.agent_grace_secs,
        command_timeout_secs: args.command_timeout_secs,
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
        .route(
            "/api/network/profiles",
            get(list_network_profiles).post(save_network_profile),
        )
        .route(
            "/api/network/profiles/{id}/plan",
            post(plan_network_profile),
        )
        .route(
            "/api/network/profiles/{id}/archive",
            post(archive_network_profile),
        )
        .route(
            "/api/network/operations/{id}/apply",
            post(apply_network_profile),
        )
        .route(
            "/api/network/revisions/{id}/teardown",
            post(teardown_network_profile),
        )
        .route("/api/network/audit", get(network_audit))
        .route("/api/network/diagnose", post(diagnose_network))
        .route("/api/network/nodes/{id}/reconcile", post(reconcile_node))
        .route("/api/network/revisions", get(list_network_revisions))
        .route("/api/scenarios", get(list_scenarios).post(save_scenario))
        .route("/api/scenarios/validate", post(validate_scenario))
        .route("/api/preflight", post(preflight))
        .route("/api/tls/certificates", post(generate_tls_certificate))
        .route("/api/artifacts", get(list_artifacts).post(upload_artifact))
        .route("/api/runs", get(list_runs).post(start_run))
        .route("/api/runs/page", get(list_runs_page))
        .route("/api/runs/active", get(active_run))
        .route("/api/runs/{id}", get(run_detail))
        .route("/api/runs/{id}/summary", get(run_summary_detail))
        .route("/api/runs/{id}/samples", get(run_samples))
        .route("/api/runs/{id}/export", get(export_run))
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
    let schema: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_metadata'",
    )
    .fetch_optional(db)
    .await?;
    let version: Option<i64> = if schema.is_some() {
        sqlx::query_scalar("SELECT version FROM schema_metadata LIMIT 1")
            .fetch_optional(db)
            .await?
    } else {
        None
    };
    if version.is_some_and(|version| version != 4) {
        anyhow::bail!("unsupported database schema version {version:?}");
    }
    for sql in [
        "PRAGMA journal_mode=WAL",
        "CREATE TABLE IF NOT EXISTS schema_metadata(version INTEGER NOT NULL)",
        "INSERT INTO schema_metadata(version) SELECT 4 WHERE NOT EXISTS(SELECT 1 FROM schema_metadata)",
        "CREATE TABLE IF NOT EXISTS network_profiles(id TEXT PRIMARY KEY,name TEXT NOT NULL,draft_json TEXT NOT NULL,status TEXT NOT NULL,archived INTEGER NOT NULL DEFAULT 0,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS network_profile_revisions(id TEXT PRIMARY KEY,profile_id TEXT NOT NULL,revision INTEGER NOT NULL,sha256 TEXT NOT NULL UNIQUE,body_json TEXT NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL,UNIQUE(profile_id,revision))",
        "CREATE TABLE IF NOT EXISTS network_operations(id TEXT PRIMARY KEY,profile_revision_id TEXT NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,plan_token_hash TEXT,expires_at TEXT,detail_json TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS network_operation_events(id INTEGER PRIMARY KEY AUTOINCREMENT,operation_id TEXT NOT NULL,node_id TEXT NOT NULL,stage TEXT NOT NULL,status TEXT NOT NULL,detail_json TEXT NOT NULL,created_at TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS idx_network_events_operation ON network_operation_events(operation_id,id)",
        "CREATE TABLE IF NOT EXISTS scenarios(id TEXT PRIMARY KEY,name TEXT NOT NULL,body TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,scenario_id TEXT NOT NULL,status TEXT NOT NULL,started_at TEXT,finished_at TEXT,error TEXT,scenario_json TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS metric_samples(id INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,agent_id TEXT NOT NULL,role INTEGER NOT NULL,unix_ms INTEGER NOT NULL,metrics_json TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS idx_metrics_run_time ON metric_samples(run_id,unix_ms)",
        "CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT,unix_ms INTEGER NOT NULL,level TEXT NOT NULL,message TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY,name TEXT NOT NULL,sha256 TEXT NOT NULL UNIQUE,size_bytes INTEGER NOT NULL,format TEXT NOT NULL,packet_count INTEGER NOT NULL,captured_bytes INTEGER NOT NULL,path TEXT NOT NULL,created_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS run_participants(run_id TEXT NOT NULL,agent_id TEXT NOT NULL,instance_id TEXT NOT NULL,role INTEGER NOT NULL,phase TEXT NOT NULL,last_command_id TEXT,error TEXT,updated_at TEXT NOT NULL,PRIMARY KEY(run_id,agent_id))",
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

async fn open_database(configured_url: &str) -> anyhow::Result<(SqlitePool, String, bool)> {
    let configured = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(configured_url)
        .await
        .context("open configured sqlite database")?;
    let schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_metadata')",
    )
    .fetch_one(&configured)
    .await?;
    let has_application_tables: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name IN ('runs','scenarios','artifacts','network_profiles'))",
    )
    .fetch_one(&configured)
    .await?;
    let version = if schema_exists {
        sqlx::query_scalar::<_, i64>("SELECT version FROM schema_metadata LIMIT 1")
            .fetch_optional(&configured)
            .await?
    } else {
        None
    };
    if version == Some(4) || (!schema_exists && !has_application_tables) {
        return Ok((configured, configured_url.to_owned(), false));
    }
    configured.close().await;
    let fallback_url = schema_fallback_url(configured_url)?;
    warn!(configured_url, %fallback_url, ?version, "unsupported database preserved; using schema-specific database");
    let fallback = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&fallback_url)
        .await
        .context("open schema-specific sqlite database")?;
    Ok((fallback, fallback_url, true))
}

fn schema_fallback_url(url: &str) -> anyhow::Result<String> {
    let (prefix, suffix) = url.split_once('?').map_or((url, ""), |(a, b)| (a, b));
    let path = prefix
        .strip_prefix("sqlite://")
        .context("DATABASE_URL must use sqlite://")?;
    if path == ":memory:" || path.is_empty() {
        anyhow::bail!("cannot create a schema-specific path for an in-memory database");
    }
    let path = std::path::Path::new(path);
    let stem = path
        .file_stem()
        .and_then(|v| v.to_str())
        .context("invalid database filename")?;
    let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("db");
    let fallback = path.with_file_name(format!("{stem}.schema-4.{extension}"));
    Ok(format!(
        "sqlite://{}{}",
        fallback.to_string_lossy().replace('\\', "/"),
        if suffix.is_empty() {
            String::new()
        } else {
            format!("?{suffix}")
        }
    ))
}

#[derive(Deserialize)]
struct ProfileListQuery {
    profile_id: Option<Uuid>,
}
async fn list_network_profiles(
    State(s): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows=sqlx::query("SELECT id,name,draft_json,status,archived,created_at,updated_at FROM network_profiles ORDER BY updated_at DESC").fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().map(|r|serde_json::json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"draft":serde_json::from_str::<serde_json::Value>(r.get("draft_json")).unwrap_or_default(),"status":r.get::<String,_>("status"),"archived":r.get::<i64,_>("archived")!=0,"created_at":r.get::<String,_>("created_at"),"updated_at":r.get::<String,_>("updated_at")})).collect()))
}
async fn save_network_profile(
    State(s): State<AppState>,
    Json(draft): Json<NetworkProfileDraft>,
) -> Result<Json<NetworkProfileDraft>, ApiError> {
    draft.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    let now = Utc::now().to_rfc3339();
    let body = serde_json::to_string(&draft)?;
    sqlx::query("INSERT INTO network_profiles(id,name,draft_json,status,created_at,updated_at) VALUES(?,?,?,'draft',?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,draft_json=excluded.draft_json,status=CASE WHEN network_profiles.status='prepared' THEN 'prepared' ELSE 'draft' END,updated_at=excluded.updated_at").bind(draft.id.to_string()).bind(&draft.name).bind(body).bind(&now).bind(&now).execute(&s.db).await?;
    Ok(Json(draft))
}

async fn archive_network_profile(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if s.active_run.lock().await.is_some() {
        return Err(ApiError::conflict(
            "profiles cannot be archived during a run",
        ));
    }
    let prepared: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM network_profile_revisions WHERE profile_id=? AND status='prepared'",
    )
    .bind(id.to_string())
    .fetch_one(&s.db)
    .await?;
    if prepared > 0 {
        return Err(ApiError::conflict(
            "teardown the prepared revision before archiving the profile",
        ));
    }
    let result = sqlx::query(
        "UPDATE network_profiles SET archived=1,status='archived',updated_at=? WHERE id=?",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(id.to_string())
    .execute(&s.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("network profile not found"));
    }
    Ok(Json(serde_json::json!({"id":id,"status":"archived"})))
}
async fn list_network_revisions(
    State(s): State<AppState>,
    Query(q): Query<ProfileListQuery>,
) -> Result<Json<Vec<NetworkProfileRevision>>, ApiError> {
    let rows = if let Some(id) = q.profile_id {
        sqlx::query("SELECT id,profile_id,revision,sha256,body_json FROM network_profile_revisions WHERE profile_id=? ORDER BY revision DESC").bind(id.to_string()).fetch_all(&s.db).await?
    } else {
        sqlx::query("SELECT id,profile_id,revision,sha256,body_json FROM network_profile_revisions ORDER BY created_at DESC").fetch_all(&s.db).await?
    };
    Ok(Json(
        rows.into_iter()
            .filter_map(|r| {
                let body = serde_json::from_str::<NetworkProfileDraft>(r.get("body_json")).ok()?;
                Some(NetworkProfileRevision {
                    id: Uuid::parse_str(r.get::<String, _>("id").as_str()).ok()?,
                    profile_id: Uuid::parse_str(r.get::<String, _>("profile_id").as_str()).ok()?,
                    revision: r.get::<i64, _>("revision") as u32,
                    sha256: r.get("sha256"),
                    body,
                })
            })
            .collect(),
    ))
}
async fn plan_network_profile(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = sqlx::query("SELECT draft_json FROM network_profiles WHERE id=? AND archived=0")
        .bind(id.to_string())
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(|| ApiError::not_found("network profile not found"))?;
    let draft: NetworkProfileDraft =
        serde_json::from_str(row.get::<String, _>("draft_json").as_str())
            .map_err(|e| ApiError::bad(e.to_string()))?;
    draft.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    let body = serde_json::to_string(&draft)?;
    let sha = format!("{:x}", Sha256::digest(body.as_bytes()));
    let existing = sqlx::query("SELECT id,revision FROM network_profile_revisions WHERE sha256=?")
        .bind(&sha)
        .fetch_optional(&s.db)
        .await?;
    let (revision_id, revision) = if let Some(r) = existing {
        (r.get::<String, _>("id"), r.get::<i64, _>("revision"))
    } else {
        let revision: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision),0)+1 FROM network_profile_revisions WHERE profile_id=?",
        )
        .bind(id.to_string())
        .fetch_one(&s.db)
        .await?;
        let revision_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO network_profile_revisions(id,profile_id,revision,sha256,body_json,status,created_at) VALUES(?,?,?,?,?,'unprepared',?)").bind(&revision_id).bind(id.to_string()).bind(revision).bind(&sha).bind(&body).bind(Utc::now().to_rfc3339()).execute(&s.db).await?;
        (revision_id, revision)
    };
    let token = Uuid::new_v4().to_string();
    let operation_id = Uuid::new_v4();
    let now = Utc::now();
    let expires = now + chrono::Duration::minutes(5);
    let mut nodes = vec![
        draft.client_endpoint.node_id.clone(),
        draft.server_endpoint.node_id.clone(),
    ];
    nodes.sort();
    nodes.dedup();
    let mut detail = serde_json::json!({"nodes":nodes,"plans":{}});
    sqlx::query("INSERT INTO network_operations(id,profile_revision_id,kind,status,plan_token_hash,expires_at,detail_json,created_at,updated_at) VALUES(?,?,'plan','planned',?,?,?,?,?)").bind(operation_id.to_string()).bind(&revision_id).bind(format!("{:x}",Sha256::digest(token.as_bytes()))).bind(expires.to_rfc3339()).bind(detail.to_string()).bind(now.to_rfc3339()).bind(now.to_rfc3339()).execute(&s.db).await?;
    let mut plans = serde_json::Map::new();
    for node in &nodes {
        let progress = network_command(
            &s,
            node,
            operation_id,
            "plan",
            serde_json::json!({"profile_revision_id":revision_id,"draft":draft}),
            expires.timestamp_millis(),
        )
        .await?;
        plans.insert(
            node.clone(),
            serde_json::from_str(&progress.detail_json)
                .map_err(|e| ApiError::internal(e.to_string()))?,
        );
    }
    detail["plans"] = serde_json::Value::Object(plans);
    sqlx::query(
        "UPDATE network_operations SET status='planned',detail_json=?,updated_at=? WHERE id=?",
    )
    .bind(detail.to_string())
    .bind(Utc::now().to_rfc3339())
    .bind(operation_id.to_string())
    .execute(&s.db)
    .await?;
    Ok(Json(
        serde_json::json!({"operation_id":operation_id,"profile_revision_id":revision_id,"revision":revision,"sha256":sha,"plan_token":token,"expires_at":expires,"detail":detail}),
    ))
}

#[derive(Deserialize)]
struct ApplyNetworkRequest {
    plan_token: String,
}
async fn network_command(
    s: &AppState,
    node_id: &str,
    operation_id: Uuid,
    action: &str,
    payload: serde_json::Value,
    lease_ms: i64,
) -> Result<NetworkProgress, ApiError> {
    let agent = s
        .agents
        .read()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| ApiError::bad(format!("node {node_id} is offline")))?;
    let command_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    s.pending_network
        .lock()
        .await
        .insert(command_id.clone(), tx);
    agent
        .tx
        .send(Ok(ControlMessage {
            body: Some(control_message::Body::Network(NetworkCommand {
                command_id: command_id.clone(),
                operation_id: operation_id.to_string(),
                action: action.into(),
                payload_json: payload.to_string(),
                lease_expires_unix_ms: lease_ms,
            })),
        }))
        .await
        .map_err(|_| ApiError::internal("node command channel closed"))?;
    match tokio::time::timeout(
        std::time::Duration::from_secs(s.command_timeout_secs.max(180)),
        rx,
    )
    .await
    {
        Ok(Ok(progress)) if progress.status == "completed" => Ok(progress),
        Ok(Ok(progress)) => Err(ApiError::internal(format!(
            "node {node_id} {action} failed: {}",
            progress.detail_json
        ))),
        _ => {
            s.pending_network.lock().await.remove(&command_id);
            Err(ApiError::internal(format!(
                "node {node_id} {action} timed out"
            )))
        }
    }
}

async fn revision_nodes(
    db: &SqlitePool,
    revision_id: Uuid,
) -> Result<(NetworkProfileDraft, Vec<String>, Uuid), ApiError> {
    let row = sqlx::query("SELECT profile_id,body_json FROM network_profile_revisions WHERE id=?")
        .bind(revision_id.to_string())
        .fetch_optional(db)
        .await?
        .ok_or_else(|| ApiError::not_found("network profile revision not found"))?;
    let draft: NetworkProfileDraft =
        serde_json::from_str(row.get::<String, _>("body_json").as_str())?;
    let mut nodes = vec![
        draft.client_endpoint.node_id.clone(),
        draft.server_endpoint.node_id.clone(),
    ];
    nodes.sort();
    nodes.dedup();
    Ok((
        draft,
        nodes,
        Uuid::parse_str(row.get::<String, _>("profile_id").as_str())
            .map_err(|e| ApiError::internal(e.to_string()))?,
    ))
}

async fn apply_network_profile(
    State(s): State<AppState>,
    Path(operation_id): Path<Uuid>,
    Json(request): Json<ApplyNetworkRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if s.active_run.lock().await.is_some() {
        return Err(ApiError::conflict(
            "network profile cannot be applied during a run",
        ));
    }
    let row=sqlx::query("SELECT profile_revision_id,status,plan_token_hash,expires_at,detail_json FROM network_operations WHERE id=? AND kind='plan'").bind(operation_id.to_string()).fetch_optional(&s.db).await?.ok_or_else(||ApiError::not_found("network plan not found"))?;
    if row.get::<String, _>("status") != "planned" {
        return Err(ApiError::conflict("network plan token is single-use"));
    }
    let expected: String = row.get("plan_token_hash");
    if expected != format!("{:x}", Sha256::digest(request.plan_token.as_bytes())) {
        return Err(ApiError::bad("invalid plan token"));
    }
    let expires: chrono::DateTime<Utc> = row
        .get::<String, _>("expires_at")
        .parse()
        .map_err(|_| ApiError::bad("invalid plan expiry"))?;
    if expires < Utc::now() {
        return Err(ApiError::conflict("network plan expired"));
    }
    let revision_id = Uuid::parse_str(row.get::<String, _>("profile_revision_id").as_str())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let (_draft, nodes, profile_id) = revision_nodes(&s.db, revision_id).await?;
    let lease = Utc::now().timestamp_millis() + 180_000;
    sqlx::query("UPDATE network_operations SET status='applying',updated_at=? WHERE id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(&s.db)
        .await?;
    let mut staged: Vec<String> = Vec::new();
    let saved: serde_json::Value =
        serde_json::from_str(row.get::<String, _>("detail_json").as_str())?;
    let plans = saved["plans"]
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::internal("network plan has no node plans"))?;
    for node in &nodes {
        let plan = plans
            .get(node)
            .cloned()
            .ok_or_else(|| ApiError::internal(format!("network plan missing node {node}")))?;
        if let Err(error) = network_command(&s, node, operation_id, "stage", plan, lease).await {
            for applied in &staged {
                let _ = network_command(
                    &s,
                    applied,
                    operation_id,
                    "rollback",
                    serde_json::Value::Null,
                    lease,
                )
                .await;
            }
            sqlx::query("UPDATE network_operations SET status='failed',detail_json=?,updated_at=? WHERE id=?").bind(serde_json::json!({"error":error.message,"plans":plans}).to_string()).bind(Utc::now().to_rfc3339()).bind(operation_id.to_string()).execute(&s.db).await?;
            return Err(error);
        }
        staged.push(node.clone());
    }
    for node in &nodes {
        if let Err(error) = network_command(
            &s,
            node,
            operation_id,
            "commit",
            serde_json::Value::Null,
            lease,
        )
        .await
        {
            for applied in &staged {
                let _ = network_command(
                    &s,
                    applied,
                    operation_id,
                    "rollback",
                    serde_json::Value::Null,
                    lease,
                )
                .await;
            }
            return Err(error);
        }
    }
    sqlx::query("UPDATE network_operations SET status='completed',detail_json=?,plan_token_hash=NULL,updated_at=? WHERE id=?").bind(serde_json::json!({"plans":plans}).to_string()).bind(Utc::now().to_rfc3339()).bind(operation_id.to_string()).execute(&s.db).await?;
    sqlx::query("UPDATE network_profile_revisions SET status='prepared' WHERE id=?")
        .bind(revision_id.to_string())
        .execute(&s.db)
        .await?;
    sqlx::query("UPDATE network_profiles SET status='prepared',updated_at=? WHERE id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(profile_id.to_string())
        .execute(&s.db)
        .await?;
    Ok(Json(
        serde_json::json!({"operation_id":operation_id,"profile_revision_id":revision_id,"status":"prepared"}),
    ))
}

async fn teardown_network_profile(
    State(s): State<AppState>,
    Path(revision_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if s.active_run.lock().await.is_some() {
        return Err(ApiError::conflict("active run must finish before teardown"));
    }
    let (_draft, nodes, profile_id) = revision_nodes(&s.db, revision_id).await?;
    let operation = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO network_operations(id,profile_revision_id,kind,status,detail_json,created_at,updated_at) VALUES(?,?,'teardown','tearing_down','{}',?,?)").bind(operation.to_string()).bind(revision_id.to_string()).bind(&now).bind(&now).execute(&s.db).await?;
    for node in &nodes {
        if let Err(error) = network_command(
            &s,
            node,
            operation,
            "teardown",
            serde_json::Value::Null,
            Utc::now().timestamp_millis() + 180_000,
        )
        .await
        {
            sqlx::query("UPDATE network_operations SET status='quarantined',detail_json=?,updated_at=? WHERE id=?").bind(serde_json::json!({"node":node,"error":error.message}).to_string()).bind(Utc::now().to_rfc3339()).bind(operation.to_string()).execute(&s.db).await?;
            return Err(error);
        }
    }
    sqlx::query("UPDATE network_profile_revisions SET status='unprepared' WHERE id=?")
        .bind(revision_id.to_string())
        .execute(&s.db)
        .await?;
    sqlx::query("UPDATE network_profiles SET status='unprepared',updated_at=? WHERE id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(profile_id.to_string())
        .execute(&s.db)
        .await?;
    sqlx::query("UPDATE network_operations SET status='completed',updated_at=? WHERE id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(operation.to_string())
        .execute(&s.db)
        .await?;
    Ok(Json(
        serde_json::json!({"operation_id":operation,"status":"unprepared"}),
    ))
}

async fn network_audit(
    State(s): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows=sqlx::query("SELECT id,profile_revision_id,kind,status,detail_json,created_at,updated_at FROM network_operations ORDER BY created_at DESC LIMIT 500").fetch_all(&s.db).await?;
    let mut result = Vec::with_capacity(rows.len());
    for r in rows {
        let operation_id: String = r.get("id");
        let event_rows = sqlx::query("SELECT node_id,stage,status,detail_json,created_at FROM network_operation_events WHERE operation_id=? ORDER BY id")
            .bind(&operation_id).fetch_all(&s.db).await?;
        let events: Vec<_> = event_rows.into_iter().map(|event| serde_json::json!({
            "node_id":event.get::<String,_>("node_id"),"stage":event.get::<String,_>("stage"),
            "status":event.get::<String,_>("status"),"detail":serde_json::from_str::<serde_json::Value>(event.get("detail_json")).unwrap_or_default(),
            "created_at":event.get::<String,_>("created_at")
        })).collect();
        result.push(serde_json::json!({"id":operation_id,"profile_revision_id":r.get::<String,_>("profile_revision_id"),"kind":r.get::<String,_>("kind"),"status":r.get::<String,_>("status"),"detail":serde_json::from_str::<serde_json::Value>(r.get("detail_json")).unwrap_or_default(),"events":events,"created_at":r.get::<String,_>("created_at"),"updated_at":r.get::<String,_>("updated_at")}));
    }
    Ok(Json(result))
}

#[derive(Deserialize)]
struct DiagnoseNetworkRequest {
    profile_revision_id: Uuid,
}

async fn diagnose_network(
    State(s): State<AppState>,
    Json(request): Json<DiagnoseNetworkRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (draft, nodes, _) = revision_nodes(&s.db, request.profile_revision_id).await?;
    let agents = s.agents.read().await;
    let mut checks = Vec::new();
    for node in &nodes {
        match agents.get(node) {
            Some(agent) => {
                let inventory = serde_json::from_str::<serde_json::Value>(&agent.inventory_json)
                    .unwrap_or_else(|_| serde_json::json!({}));
                checks.push(serde_json::json!({"name":"node_online","node_id":node,"ok":true,"detail":agent.hostname}));
                checks.push(serde_json::json!({"name":"inventory_available","node_id":node,"ok":inventory.get("interfaces").and_then(|v|v.as_array()).is_some_and(|v|!v.is_empty()),"detail":inventory}));
            }
            None => checks.push(serde_json::json!({"name":"node_online","node_id":node,"ok":false,"detail":"agent is offline"})),
        }
    }
    checks.push(serde_json::json!({"name":"profile_valid","ok":draft.validate().is_ok()}));
    let ok = checks
        .iter()
        .all(|check| check["ok"].as_bool().unwrap_or(false));
    Ok(Json(
        serde_json::json!({"profile_revision_id":request.profile_revision_id,"ok":ok,"checks":checks,"note":"link reachability is verified again by run preflight from the selected namespace"}),
    ))
}
async fn reconcile_node(
    State(s): State<AppState>,
    Path(node_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let operation = Uuid::new_v4();
    network_command(
        &s,
        &node_id,
        operation,
        "reconcile",
        serde_json::Value::Null,
        Utc::now().timestamp_millis() + 180_000,
    )
    .await?;
    Ok(Json(
        serde_json::json!({"node_id":node_id,"status":"unprepared"}),
    ))
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

async fn cleanup_orphan_artifacts(db: &SqlitePool, days: i64) -> anyhow::Result<()> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let rows = sqlx::query("SELECT id,path FROM artifacts WHERE created_at < ?")
        .bind(cutoff)
        .fetch_all(db)
        .await?;
    for row in rows {
        let id: String = row.get("id");
        let pattern = format!("%{id}%");
        let scenario_refs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM scenarios WHERE body LIKE ?")
                .bind(&pattern)
                .fetch_one(db)
                .await?;
        let run_refs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE scenario_json LIKE ?")
                .bind(&pattern)
                .fetch_one(db)
                .await?;
        if scenario_refs + run_refs > 0 {
            continue;
        }
        let path: String = row.get("path");
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(artifact_id=%id, %error, "failed to remove orphan artifact file");
                continue;
            }
        }
        sqlx::query("DELETE FROM artifacts WHERE id=?")
            .bind(&id)
            .execute(db)
            .await?;
        info!(artifact_id=%id, "removed orphan artifact");
    }
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
    inventory: serde_json::Value,
}
async fn health(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(
        serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"schema_version":4,"database_url":s.database_url.as_str(),"database_fallback":s.database_fallback}),
    )
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
                inventory: serde_json::from_str(&a.inventory_json).unwrap_or_default(),
            })
            .collect(),
    )
}

async fn validate_scenario(Json(sc): Json<Scenario>) -> Result<Json<serde_json::Value>, ApiError> {
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(serde_json::json!({"valid":true})))
}
async fn preflight(
    State(s): State<AppState>,
    Json(sc): Json<Scenario>,
) -> Result<Json<serde_json::Value>, ApiError> {
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    validate_payload_artifacts(&s.db, &sc).await?;
    validate_capture_artifact(&s.db, &sc).await?;
    let (client_id, server_id, profile_ready) = scenario_nodes(&s.db, &sc).await?;
    let agents = s.agents.read().await;
    let client = agents.get(&client_id);
    let server = agents.get(&server_id);
    let checks = vec![
        serde_json::json!({"name":"client_node","ok":client.is_some(),"detail":client_id}),
        serde_json::json!({"name":"server_node","ok":server.is_some(),"detail":server_id}),
        serde_json::json!({"name":"network_profile","ok":profile_ready,"detail":if profile_ready {"prepared"} else {"not prepared"}}),
    ];
    let ok = checks.iter().all(|v| v["ok"].as_bool() == Some(true));
    Ok(Json(
        serde_json::json!({"ok":ok,"checks":checks,"warnings":["route, MTU, offload와 물리 경로는 Linux 실장비에서 별도 확인해야 합니다"]}),
    ))
}

async fn scenario_nodes(
    db: &SqlitePool,
    sc: &Scenario,
) -> Result<(String, String, bool), ApiError> {
    match &sc.path {
        ScenarioPath::ExplicitProxy {
            client_node_id,
            server_node_id,
            ..
        } => Ok((client_node_id.clone(), server_node_id.clone(), true)),
        ScenarioPath::ManagedDirect {
            profile_revision_id,
            ..
        } => {
            let row =
                sqlx::query("SELECT body_json,status FROM network_profile_revisions WHERE id=?")
                    .bind(profile_revision_id.to_string())
                    .fetch_optional(db)
                    .await?
                    .ok_or_else(|| ApiError::bad("network profile revision not found"))?;
            let body: NetworkProfileDraft =
                serde_json::from_str(row.get::<String, _>("body_json").as_str())
                    .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok((
                body.client_endpoint.node_id,
                body.server_endpoint.node_id,
                row.get::<String, _>("status") == "prepared",
            ))
        }
    }
}

async fn scenario_runtime(
    db: &SqlitePool,
    sc: &Scenario,
) -> Result<
    (
        (String, String, String, Vec<String>),
        (String, String, String, Vec<String>),
    ),
    ApiError,
> {
    match &sc.path {
        ScenarioPath::ExplicitProxy {
            client_bind_ip,
            server_listen_ip,
            server_port,
            ..
        } => {
            let target = format!("{server_listen_ip}:{server_port}");
            Ok((
                (
                    target.clone(),
                    String::new(),
                    String::new(),
                    vec![client_bind_ip.to_string()],
                ),
                (target, String::new(), String::new(), Vec::new()),
            ))
        }
        ScenarioPath::ManagedDirect {
            profile_revision_id,
            server_port,
        } => {
            let row = sqlx::query("SELECT body_json FROM network_profile_revisions WHERE id=?")
                .bind(profile_revision_id.to_string())
                .fetch_one(db)
                .await?;
            let draft: NetworkProfileDraft =
                serde_json::from_str(row.get::<String, _>("body_json").as_str())?;
            let address = draft
                .server_endpoint
                .start_cidr
                .split_once('/')
                .map(|v| v.0)
                .ok_or_else(|| ApiError::bad("invalid server pool"))?;
            let revision = profile_revision_id.to_string();
            let short = &revision[..8];
            let (start, prefix) = draft
                .client_endpoint
                .start_cidr
                .split_once('/')
                .ok_or_else(|| ApiError::bad("invalid client pool"))?;
            let start: u32 = start
                .parse::<std::net::Ipv4Addr>()
                .map_err(|_| ApiError::bad("invalid client pool"))?
                .into();
            let sources = (0..draft.client_endpoint.count)
                .map(|offset| std::net::Ipv4Addr::from(start + offset).to_string())
                .collect();
            let _ = prefix;
            Ok((
                (
                    format!("{address}:{server_port}"),
                    draft.client_endpoint.interface_name,
                    format!("pt-{short}-client"),
                    sources,
                ),
                (
                    format!("{address}:{server_port}"),
                    draft.server_endpoint.interface_name,
                    format!("pt-{short}-server"),
                    Vec::new(),
                ),
            ))
        }
    }
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
                "http_flow_count": analyzed.flows.iter().filter(|flow| !flow.http_transactions.is_empty()).count(),
                "http_transaction_count": analyzed.flows.iter().map(|flow| flow.http_transactions.len()).sum::<usize>(),
                "http2_flow_count": analyzed.flows.iter().filter(|flow| !flow.http2_transactions.is_empty()).count(),
                "http2_transaction_count": analyzed.flows.iter().map(|flow| flow.http2_transactions.len()).sum::<usize>(),
                "retransmitted_bytes": analyzed.flows.iter().map(|flow|flow.retransmitted_bytes).sum::<u64>(),
                "exclusions": {
                    "non_tcp_packets": analyzed.exclusions.non_tcp_packets,
                    "fragmented_packets": analyzed.exclusions.fragmented_packets,
                    "truncated_packets": analyzed.exclusions.truncated_packets,
                    "unsupported_link_packets": analyzed.exclusions.unsupported_link_packets,
                    "incomplete_flows": analyzed.exclusions.incomplete_flows,
                    "encrypted_tls_flows": analyzed.exclusions.encrypted_tls_flows,
                    "non_http_flows": analyzed.exclusions.non_http_flows,
                    "unsupported_http_flows": analyzed.exclusions.unsupported_http_flows,
                    "http_upgrade_flows": analyzed.exclusions.http_upgrade_flows,
                    "unsupported_http2_flows": analyzed.exclusions.unsupported_http2_flows,
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
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    validate_payload_artifacts(&s.db, &sc).await?;
    validate_capture_artifact(&s.db, &sc).await?;
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
            .filter_map(|r| serde_json::from_str::<Scenario>(r.get("body")).ok())
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
    sc.validate().map_err(|e| ApiError::bad(e.to_string()))?;
    let artifact_ids: HashSet<Uuid> = [sc.request_payload.clone(), sc.response_payload.clone()]
        .into_iter()
        .filter(|payload| payload.kind == PayloadKind::File)
        .filter_map(|payload| payload.artifact_id)
        .collect();
    let mut artifact_ids = artifact_ids;
    if let Some(capture_id) = sc
        .capture_artifact_id
        .filter(|_| sc.payload_mode == PayloadMode::CaptureReplay)
    {
        artifact_ids.insert(capture_id);
    }
    let mut active = s.active_run.lock().await;
    if active.is_some() {
        return Err(ApiError::conflict("이미 실행 중인 시험이 있습니다"));
    }
    let (client_id, server_id, profile_ready) = scenario_nodes(&s.db, &sc).await?;
    if !profile_ready {
        return Err(ApiError::conflict(
            "referenced network profile revision is not prepared",
        ));
    }
    let sessions = s.agents.read().await;
    let online_cutoff = Utc::now().timestamp_millis() - 15_000;
    let client = sessions
        .get(&client_id)
        .filter(|agent| agent.last_seen_ms >= online_cutoff)
        .ok_or_else(|| ApiError::bad("client agent가 연결되지 않았습니다"))?
        .clone();
    let server = sessions
        .get(&server_id)
        .filter(|agent| agent.last_seen_ms >= online_cutoff)
        .ok_or_else(|| ApiError::bad("server agent가 연결되지 않았습니다"))?
        .clone();
    drop(sessions);
    let run_id = Uuid::new_v4();
    let json = serde_json::to_string(&sc)?;
    let (client_runtime, server_runtime) = scenario_runtime(&s.db, &sc).await?;
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
            command_id: String::new(),
            endpoint_role: 1,
            target_addr: client_runtime.0,
            interface_name: client_runtime.1,
            namespace: client_runtime.2,
            source_ips: client_runtime.3,
        })),
    };
    let preparation = async {
        send_artifacts(&s.db, &client, &artifact_ids).await?;
        if client_id != server_id {
            send_artifacts(&s.db, &server, &artifact_ids).await?;
        }
        command_agent(&s, &client_id, &client, run_id, "prepare", prepare.clone()).await?;
        let mut server_prepare = prepare;
        if let Some(control_message::Body::Prepare(value)) = server_prepare.body.as_mut() {
            value.endpoint_role = 2;
            value.target_addr = server_runtime.0;
            value.interface_name = server_runtime.1;
            value.namespace = server_runtime.2;
            value.source_ips = server_runtime.3;
        }
        command_agent(&s, &server_id, &server, run_id, "prepare", server_prepare).await?;
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(error) = preparation {
        sqlx::query("UPDATE runs SET status='failed',finished_at=?,error=? WHERE id=?")
            .bind(Utc::now().to_rfc3339())
            .bind(&error.message)
            .bind(run_id.to_string())
            .execute(&s.db)
            .await?;
        return Err(error);
    }
    let start_at = Utc::now().timestamp_millis() + 1000;
    let start = ControlMessage {
        body: Some(control_message::Body::Start(StartRun {
            run_id: run_id.to_string(),
            start_unix_ms: start_at,
            command_id: String::new(),
            endpoint_role: 1,
        })),
    };
    let starting = async {
        command_agent(&s, &client_id, &client, run_id, "start", start.clone()).await?;
        let mut server_start = start;
        if let Some(control_message::Body::Start(value)) = server_start.body.as_mut() {
            value.endpoint_role = 2;
        }
        command_agent(&s, &server_id, &server, run_id, "start", server_start).await?;
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(error) = starting {
        for agent in [&client, &server] {
            let _ = agent
                .tx
                .send(Ok(ControlMessage {
                    body: Some(control_message::Body::Stop(StopRun {
                        run_id: run_id.to_string(),
                        command_id: Uuid::new_v4().to_string(),
                        endpoint_role: 0,
                    })),
                }))
                .await;
        }
        sqlx::query("UPDATE runs SET status='failed',finished_at=?,error=? WHERE id=?")
            .bind(Utc::now().to_rfc3339())
            .bind(&error.message)
            .bind(run_id.to_string())
            .execute(&s.db)
            .await?;
        return Err(error);
    }
    sqlx::query("UPDATE runs SET status='running',started_at=? WHERE id=?")
        .bind(started_at.to_rfc3339())
        .bind(run_id.to_string())
        .execute(&s.db)
        .await?;
    *active = Some(run_id);
    s.run_agents.lock().await.insert(
        run_id,
        HashSet::from([client_id.clone(), server_id.clone()]),
    );
    s.expected_endpoints.lock().await.insert(
        run_id,
        HashSet::from([format!("{client_id}:1"), format!("{server_id}:2")]),
    );
    let _ = s
        .events
        .send(serde_json::json!({"type":"run_started","run_id":run_id}).to_string());
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"id":run_id,"status":"running"})),
    ))
}

fn with_command_id(mut message: ControlMessage, command_id: &str) -> ControlMessage {
    match message.body.as_mut() {
        Some(control_message::Body::Prepare(value)) => value.command_id = command_id.into(),
        Some(control_message::Body::Start(value)) => value.command_id = command_id.into(),
        Some(control_message::Body::Stop(value)) => value.command_id = command_id.into(),
        Some(control_message::Body::SetPaused(value)) => value.command_id = command_id.into(),
        _ => {}
    }
    message
}

async fn command_agent(
    s: &AppState,
    agent_id: &str,
    agent: &AgentSession,
    run_id: Uuid,
    phase: &str,
    message: ControlMessage,
) -> Result<(), ApiError> {
    let command_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    s.pending_acks.lock().await.insert(command_id.clone(), tx);
    sqlx::query("INSERT INTO run_participants(run_id,agent_id,instance_id,role,phase,last_command_id,error,updated_at) VALUES(?,?,?,?,?,?,NULL,?) ON CONFLICT(run_id,agent_id) DO UPDATE SET instance_id=excluded.instance_id,phase=excluded.phase,last_command_id=excluded.last_command_id,error=NULL,updated_at=excluded.updated_at")
        .bind(run_id.to_string()).bind(agent_id).bind(&agent.instance_id).bind(agent.role).bind(phase).bind(&command_id).bind(Utc::now().to_rfc3339()).execute(&s.db).await?;
    if agent
        .tx
        .send(Ok(with_command_id(message, &command_id)))
        .await
        .is_err()
    {
        s.pending_acks.lock().await.remove(&command_id);
        return Err(ApiError::internal(format!(
            "{agent_id} channel closed during {phase}"
        )));
    }
    let ack = match tokio::time::timeout(std::time::Duration::from_secs(s.command_timeout_secs), rx)
        .await
    {
        Ok(Ok(ack)) => ack,
        Ok(Err(_)) => {
            return Err(ApiError::internal(format!(
                "{agent_id} {phase} acknowledgement channel closed"
            )));
        }
        Err(_) => {
            s.pending_acks.lock().await.remove(&command_id);
            return Err(ApiError::internal(format!(
                "{agent_id} {phase} acknowledgement timed out"
            )));
        }
    };
    if !ack.ok {
        return Err(ApiError::internal(format!(
            "{} {phase} failed: {}",
            ack.agent_id, ack.error
        )));
    }
    sqlx::query("UPDATE run_participants SET phase=?,updated_at=? WHERE run_id=? AND agent_id=?")
        .bind(format!("{phase}_acked"))
        .bind(Utc::now().to_rfc3339())
        .bind(run_id.to_string())
        .bind(agent_id)
        .execute(&s.db)
        .await?;
    Ok(())
}

const ARTIFACT_CHUNK_BYTES: usize = 256 * 1024;

async fn validate_payload_artifacts(db: &SqlitePool, scenario: &Scenario) -> Result<(), ApiError> {
    for (direction, payload) in [
        ("request", scenario.request_payload.clone()),
        ("response", scenario.response_payload.clone()),
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

async fn validate_capture_artifact(db: &SqlitePool, scenario: &Scenario) -> Result<(), ApiError> {
    if scenario.payload_mode != PayloadMode::CaptureReplay {
        return Ok(());
    }
    let id = scenario
        .capture_artifact_id
        .ok_or_else(|| ApiError::bad("capture replay requires capture_artifact_id"))?;
    let row = sqlx::query("SELECT kind,analysis_json FROM artifacts WHERE id=?")
        .bind(id.to_string())
        .fetch_optional(db)
        .await?
        .ok_or_else(|| ApiError::bad(format!("capture artifact {id} does not exist")))?;
    if row.get::<String, _>("kind") != "pcap" {
        return Err(ApiError::bad(format!(
            "artifact {id} is not a PCAP artifact"
        )));
    }
    let analysis = row
        .get::<Option<String>, _>("analysis_json")
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .unwrap_or_default();
    let count_key = match scenario.protocol {
        Protocol::Http1 => "http_flow_count",
        Protocol::Http2 => "http2_flow_count",
        _ => "supported_flow_count",
    };
    if analysis[count_key].as_u64().unwrap_or(0) == 0 {
        return Err(ApiError::bad(match scenario.protocol {
            Protocol::Http1 => "capture has no supported HTTP/1.1 request/response transactions",
            Protocol::Http2 => "capture has no supported plaintext HTTP/2 transactions",
            _ => "capture has no supported bidirectional plaintext TCP flows",
        }));
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
        if kind != "payload" && kind != "pcap" {
            return Err(ApiError::bad(format!(
                "artifact {artifact_id} has unsupported kind {kind}"
            )));
        }
        let total_size = row.get::<i64, _>("size_bytes") as u64;
        let limit = if kind == "pcap" {
            512 * 1024 * 1024
        } else {
            64 * 1024 * 1024
        };
        if total_size > limit {
            return Err(ApiError::bad(format!(
                "{kind} artifact {artifact_id} exceeds its size limit"
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
                        artifact_kind: kind.clone(),
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
    let agents = s.agents.read().await.clone();
    let participant_ids = s
        .run_agents
        .lock()
        .await
        .get(&id)
        .cloned()
        .unwrap_or_default();
    for agent_id in participant_ids {
        if let Some(agent) = agents.get(&agent_id) {
            let _ = command_agent(
                &s,
                &agent_id,
                agent,
                id,
                "stop",
                ControlMessage {
                    body: Some(control_message::Body::Stop(StopRun {
                        run_id: id.to_string(),
                        command_id: String::new(),
                        endpoint_role: 0,
                    })),
                },
            )
            .await;
        }
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
            command_id: String::new(),
            endpoint_role: 0,
        })),
    };
    let agents = s.agents.read().await.clone();
    let participant_ids = s
        .run_agents
        .lock()
        .await
        .get(&id)
        .cloned()
        .unwrap_or_default();
    for agent_id in participant_ids {
        let agent = agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::internal(format!("{agent_id} is disconnected")))?;
        command_agent(
            s,
            &agent_id,
            agent,
            id,
            if paused { "pause" } else { "resume" },
            message.clone(),
        )
        .await?;
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
    drop(active);
    s.run_agents.lock().await.remove(&id);
    s.completed_agents.lock().await.remove(&id);
    s.expected_endpoints.lock().await.remove(&id);
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
#[derive(Deserialize)]
struct RunPageQuery {
    limit: Option<u32>,
    cursor: Option<i64>,
}
async fn list_runs_page(
    State(s): State<AppState>,
    Query(query): Query<RunPageQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = query.limit.unwrap_or(25).clamp(1, 100) as i64;
    let cursor = query.cursor.unwrap_or(i64::MAX);
    let rows=sqlx::query("SELECT rowid,id,scenario_id,run_name,status,started_at,finished_at,error,scenario_json FROM runs WHERE rowid < ? ORDER BY rowid DESC LIMIT ?")
        .bind(cursor).bind(limit + 1).fetch_all(&s.db).await?;
    let next_cursor =
        (rows.len() as i64 > limit).then(|| rows[limit as usize - 1].get::<i64, _>("rowid"));
    let items=rows.into_iter().take(limit as usize).map(|r|serde_json::json!({"id":r.get::<String,_>("id"),"scenario_id":r.get::<String,_>("scenario_id"),"run_name":r.get::<Option<String>,_>("run_name"),"status":r.get::<String,_>("status"),"started_at":r.get::<Option<String>,_>("started_at"),"finished_at":r.get::<Option<String>,_>("finished_at"),"error":r.get::<Option<String>,_>("error"),"scenario":redacted_scenario(r.get("scenario_json"))})).collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"items":items,"next_cursor":next_cursor}),
    ))
}

#[derive(Deserialize)]
struct SampleQuery {
    from_unix_ms: Option<i64>,
    to_unix_ms: Option<i64>,
    max_points: Option<u32>,
}
async fn run_samples(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<SampleQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let from = query.from_unix_ms.unwrap_or(i64::MIN);
    let to = query.to_unix_ms.unwrap_or(i64::MAX);
    let maximum = query.max_points.unwrap_or(2000).clamp(10, 10_000) as usize;
    let rows=sqlx::query("SELECT agent_id,role,unix_ms,metrics_json FROM metric_samples WHERE run_id=? AND unix_ms>=? AND unix_ms<=? ORDER BY unix_ms")
        .bind(id.to_string()).bind(from).bind(to).fetch_all(&s.db).await?;
    let stride = (rows.len().saturating_add(maximum - 1) / maximum).max(1);
    let samples=rows.into_iter().enumerate().filter(|(index,_)| index % stride == 0).map(|(_,r)|serde_json::json!({"agent_id":r.get::<String,_>("agent_id"),"role":r.get::<i64,_>("role"),"unix_ms":r.get::<i64,_>("unix_ms"),"metrics":serde_json::from_str::<serde_json::Value>(r.get("metrics_json")).unwrap_or_default()})).collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"samples":samples,"downsampled":stride>1,"stride":stride}),
    ))
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}
async fn export_run(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let detail = run_detail(State(s), Path(id)).await?.0;
    let format = query.format.as_deref().unwrap_or("json");
    let (content_type, body) = if format == "csv" {
        let mut output = String::from("agent_id,role,unix_ms,metrics_json\n");
        for sample in detail["samples"].as_array().into_iter().flatten() {
            output.push_str(&format!(
                "{},{},{},{}\n",
                sample["agent_id"].as_str().unwrap_or(""),
                sample["role"],
                sample["unix_ms"],
                serde_json::to_string(&sample["metrics"]).unwrap_or_default()
            ));
        }
        ("text/csv", output)
    } else {
        (
            "application/json",
            serde_json::to_string_pretty(&detail).unwrap_or_default(),
        )
    };
    Ok((
        [
            ("content-type", content_type),
            (
                "content-disposition",
                if format == "csv" {
                    "attachment; filename=run.csv"
                } else {
                    "attachment; filename=run.json"
                },
            ),
        ],
        body,
    )
        .into_response())
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
    let scenario_json: String = run.get("scenario_json");
    let samples=sqlx::query("SELECT agent_id,role,unix_ms,metrics_json FROM metric_samples WHERE run_id=? ORDER BY unix_ms").bind(id.to_string()).fetch_all(&s.db).await?;
    let samples = samples.into_iter().map(|r|serde_json::json!({"agent_id":r.get::<String,_>("agent_id"),"role":r.get::<i64,_>("role"),"unix_ms":r.get::<i64,_>("unix_ms"),"metrics":serde_json::from_str::<serde_json::Value>(r.get("metrics_json")).unwrap_or_default()})).collect::<Vec<_>>();
    let payload_metadata = result_payload_metadata(&scenario_json, &samples);
    Ok(Json(
        serde_json::json!({"id":run.get::<String,_>("id"),"run_name":run.get::<Option<String>,_>("run_name"),"started_at":run.get::<Option<String>,_>("started_at"),"finished_at":run.get::<Option<String>,_>("finished_at"),"status":run.get::<String,_>("status"),"error":run.get::<Option<String>,_>("error"),"scenario":redacted_scenario(&scenario_json),"payload_metadata":payload_metadata,"samples":samples }),
    ))
}
async fn run_summary_detail(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut detail = run_detail(State(s), Path(id)).await?.0;
    detail["samples"] = serde_json::json!([]);
    Ok(Json(detail))
}

fn result_payload_metadata(body: &str, samples: &[serde_json::Value]) -> serde_json::Value {
    let Ok(scenario) = serde_json::from_str::<Scenario>(body) else {
        return serde_json::Value::Null;
    };
    if scenario.payload_mode == PayloadMode::CaptureReplay {
        return serde_json::json!({
            "mode": "capture_replay",
            "capture_artifact_id": scenario.capture_artifact_id
        });
    }
    let random_hash = |role: i64, field: &str| {
        samples.iter().find_map(|sample| {
            (sample["role"].as_i64() == Some(role))
                .then(|| sample["metrics"][field].as_str().map(str::to_owned))
                .flatten()
        })
    };
    let metadata =
        |direction: &str, payload: proxy_tester_domain::PayloadProfile, hash: Option<String>| {
            let kind = serde_json::to_value(payload.kind).unwrap_or_default();
            let mut value = serde_json::json!({
                "direction": direction,
                "kind": kind,
                "size_bytes": payload.byte_len()
            });
            if payload.kind == PayloadKind::File {
                value["artifact_id"] = serde_json::json!(payload.artifact_id);
            }
            if payload.kind == PayloadKind::Random {
                value["random_format"] =
                    serde_json::to_value(payload.random_format).unwrap_or_default();
                value["sha256"] = serde_json::json!(hash);
            }
            value
        };
    serde_json::json!({
        "mode": "manual",
        "request": metadata("client_to_server", scenario.request_payload.clone(), random_hash(1, "request_random_sha256")),
        "response": metadata("server_to_client", scenario.response_payload.clone(), random_hash(2, "response_random_sha256"))
    })
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
        let generation = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(64);
        self.0.agents.write().await.insert(
            id.clone(),
            AgentSession {
                instance_id: hello.instance_id,
                generation,
                role,
                hostname: hello.hostname,
                interfaces: hello.interfaces,
                inventory_json: hello.inventory_json,
                last_seen_ms: Utc::now().timestamp_millis(),
                tx,
            },
        );
        let state = self.0.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = incoming.next().await {
                match msg.body {
                    Some(agent_message::Body::Heartbeat(_)) => {
                        if let Some(a) = state.agents.write().await.get_mut(&id)
                            && a.generation == generation
                        {
                            a.last_seen_ms = Utc::now().timestamp_millis();
                        }
                    }
                    Some(agent_message::Body::CommandAck(ack)) => {
                        if let Some(waiter) =
                            state.pending_acks.lock().await.remove(&ack.command_id)
                        {
                            let _ = waiter.send(CommandAckResult {
                                agent_id: id.clone(),
                                ok: ack.ok,
                                error: ack.error,
                            });
                        }
                    }
                    Some(agent_message::Body::NetworkProgress(progress)) => {
                        let _=sqlx::query("INSERT INTO network_operation_events(operation_id,node_id,stage,status,detail_json,created_at) VALUES(?,?,?,?,?,?)")
                            .bind(&progress.operation_id).bind(&id).bind(&progress.stage).bind(&progress.status)
                            .bind(&progress.detail_json).bind(Utc::now().to_rfc3339()).execute(&state.db).await;
                        if let Some(waiter) = state
                            .pending_network
                            .lock()
                            .await
                            .remove(&progress.command_id)
                        {
                            let _ = waiter.send(progress.clone());
                        }
                        let _=state.events.send(serde_json::json!({"type":"network_progress","node_id":id,"operation_id":progress.operation_id,"stage":progress.stage,"status":progress.status,"detail":serde_json::from_str::<serde_json::Value>(&progress.detail_json).unwrap_or_default()}).to_string());
                    }
                    Some(agent_message::Body::Status(status)) => {
                        if !status.active_run_id.is_empty() {
                            warn!(agent=%id, run=%status.active_run_id, "agent reconnected with orphan run; it will not be resumed");
                        }
                    }
                    Some(agent_message::Body::Telemetry(t)) => {
                        if let Ok(m) = serde_json::from_str::<MetricsSnapshot>(&t.metrics_json) {
                            let endpoint_role = if t.endpoint_role == 0 {
                                role
                            } else {
                                t.endpoint_role
                            };
                            let _=sqlx::query("INSERT INTO metric_samples(run_id,agent_id,role,unix_ms,metrics_json) VALUES(?,?,?,?,?)").bind(&t.run_id).bind(&id).bind(endpoint_role).bind(m.unix_ms).bind(&t.metrics_json).execute(&state.db).await;
                            let _=state.events.send(serde_json::json!({"type":"metrics","agent_id":id,"role":endpoint_role,"data":m}).to_string());
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
                        if e.level == "error"
                            && let Ok(run_id) = Uuid::parse_str(&e.run_id)
                            && *state.active_run.lock().await == Some(run_id)
                        {
                            let peers: Vec<_> =
                                state.agents.read().await.values().cloned().collect();
                            for peer in peers {
                                let _ = peer
                                    .tx
                                    .send(Ok(ControlMessage {
                                        body: Some(control_message::Body::Stop(StopRun {
                                            run_id: run_id.to_string(),
                                            command_id: Uuid::new_v4().to_string(),
                                            endpoint_role: 0,
                                        })),
                                    }))
                                    .await;
                            }
                            let reason = format!("endpoint_worker_failed: {}", e.message);
                            let _ = finish_run(&state, run_id, "failed", Some(&reason)).await;
                        }
                        if e.message == "run_completed"
                            && let Ok(run_id) = Uuid::parse_str(&e.run_id)
                            && *state.active_run.lock().await == Some(run_id)
                        {
                            let expected = state
                                .expected_endpoints
                                .lock()
                                .await
                                .get(&run_id)
                                .cloned()
                                .unwrap_or_default();
                            let mut completed = state.completed_agents.lock().await;
                            completed
                                .entry(run_id)
                                .or_default()
                                .insert(format!("{}:{}", id, e.endpoint_role));
                            let all_completed = !expected.is_empty()
                                && completed
                                    .get(&run_id)
                                    .is_some_and(|items| expected.is_subset(items));
                            drop(completed);
                            if all_completed {
                                let _ = finish_run(&state, run_id, "completed", None).await;
                            }
                        }
                        let _=state.events.send(serde_json::json!({"type":"agent_event","agent_id":id,"run_id":e.run_id,"level":e.level,"message":e.message}).to_string());
                    }
                    _ => {}
                }
            }
            let removed = {
                let mut agents = state.agents.write().await;
                if agents
                    .get(&id)
                    .is_some_and(|session| session.generation == generation)
                {
                    agents.remove(&id);
                    true
                } else {
                    false
                }
            };
            if removed {
                schedule_disconnect_failure(state.clone(), id.clone()).await;
            }
            warn!(agent=%id,"agent disconnected");
        });
        let output = async_stream::stream! {while let Some(msg)=rx.recv().await{yield msg;}};
        Ok(GrpcResponse::new(Box::pin(output)))
    }
}

async fn schedule_disconnect_failure(state: AppState, agent_id: String) {
    let Some(run_id) = *state.active_run.lock().await else {
        return;
    };
    let participates = state
        .run_agents
        .lock()
        .await
        .get(&run_id)
        .is_some_and(|ids| ids.contains(&agent_id));
    if !participates {
        return;
    }
    let _ = sqlx::query("UPDATE runs SET status='degraded',error=? WHERE id=?")
        .bind(format!(
            "{agent_id} disconnected; waiting for reconnect grace"
        ))
        .bind(run_id.to_string())
        .execute(&state.db)
        .await;
    let _ = state.events.send(serde_json::json!({"type":"run_state","run_id":run_id,"status":"degraded","agent_id":agent_id}).to_string());
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(state.agent_grace_secs)).await;
        if *state.active_run.lock().await != Some(run_id) {
            return;
        }
        let reconnected = state.agents.read().await.contains_key(&agent_id);
        let reason = if reconnected {
            "agent_reconnected_no_resume"
        } else {
            "agent_disconnected"
        };
        let peers: Vec<_> = state.agents.read().await.values().cloned().collect();
        for peer in peers {
            let _ = peer
                .tx
                .send(Ok(ControlMessage {
                    body: Some(control_message::Body::Stop(StopRun {
                        run_id: run_id.to_string(),
                        command_id: Uuid::new_v4().to_string(),
                        endpoint_role: 0,
                    })),
                }))
                .await;
        }
        let _ = finish_run(
            &state,
            run_id,
            "failed",
            Some(&format!("{reason}: {agent_id}")),
        )
        .await;
    });
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}
impl ApiError {
    fn bad(s: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: s.into(),
        }
    }
    fn conflict(s: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: s.into(),
        }
    }
    fn not_found(s: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: s.into(),
        }
    }
    fn internal(s: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: s.into(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({"code":self.code,"message":self.message})),
        )
            .into_response()
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
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::{
        analyze_capture, cleanup_orphan_artifacts, migrate, result_payload_metadata,
        schema_fallback_url,
    };
    use chrono::Utc;
    use proxy_tester_domain::{PayloadKind, PayloadProfile, RandomFormat, Scenario};
    use sqlx::{Row, sqlite::SqlitePoolOptions};
    use uuid::Uuid;

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

    #[test]
    fn result_payload_metadata_redacts_content_and_keeps_random_digest() {
        let mut scenario = Scenario::default();
        scenario.request_payload = PayloadProfile {
            kind: PayloadKind::Text,
            text: "DLP-비밀".into(),
            ..Default::default()
        };
        scenario.response_payload = PayloadProfile {
            kind: PayloadKind::Random,
            size_bytes: 1024,
            random_format: RandomFormat::PrintableAscii,
            ..Default::default()
        };
        let samples = vec![serde_json::json!({
            "role": 2,
            "metrics": {"response_random_sha256": "abc123"}
        })];
        let metadata =
            result_payload_metadata(&serde_json::to_string(&scenario).unwrap(), &samples);
        assert_eq!(metadata["request"]["direction"], "client_to_server");
        assert_eq!(metadata["request"]["size_bytes"], 10);
        assert!(metadata["request"].get("text").is_none());
        assert_eq!(metadata["response"]["sha256"], "abc123");
        assert_eq!(metadata["response"]["random_format"], "printable_ascii");
    }

    #[tokio::test]
    async fn artifact_cleanup_removes_only_old_unreferenced_files() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        migrate(&db).await.unwrap();
        let directory =
            std::env::temp_dir().join(format!("proxy-tester-cleanup-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let orphan = directory.join("orphan");
        let referenced = directory.join("referenced");
        tokio::fs::write(&orphan, b"orphan").await.unwrap();
        tokio::fs::write(&referenced, b"referenced").await.unwrap();
        let old = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
        for (id, path) in [("orphan-id", &orphan), ("referenced-id", &referenced)] {
            sqlx::query("INSERT INTO artifacts(id,name,sha256,size_bytes,format,packet_count,captured_bytes,path,created_at,kind) VALUES(?,?,?,?,?,?,?,?,?,?)")
                .bind(id).bind(id).bind(format!("sha-{id}")).bind(1_i64).bind("raw").bind(0_i64).bind(1_i64).bind(path.to_string_lossy().as_ref()).bind(&old).bind("payload").execute(&db).await.unwrap();
        }
        sqlx::query("INSERT INTO scenarios(id,name,body,created_at,updated_at) VALUES('scenario','test',?,'now','now')")
            .bind(r#"{"artifact_id":"referenced-id"}"#).execute(&db).await.unwrap();
        cleanup_orphan_artifacts(&db, 90).await.unwrap();
        assert!(!orphan.exists());
        assert!(referenced.exists());
        let ids = sqlx::query("SELECT id FROM artifacts ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("id"))
            .collect::<Vec<_>>();
        assert_eq!(ids, ["referenced-id"]);
        tokio::fs::remove_file(referenced).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[tokio::test]
    async fn schema_v4_rejects_legacy_database_without_deleting_it() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE schema_metadata(version INTEGER NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO schema_metadata VALUES(3)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE scenarios(id TEXT PRIMARY KEY)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO scenarios VALUES('old')")
            .execute(&db)
            .await
            .unwrap();
        assert!(migrate(&db).await.is_err());
        let version: i64 = sqlx::query_scalar("SELECT version FROM schema_metadata")
            .fetch_one(&db)
            .await
            .unwrap();
        let old: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scenarios")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(version, 3);
        assert_eq!(old, 1);
        assert_eq!(
            schema_fallback_url("sqlite://data/proxy.db?mode=rwc").unwrap(),
            "sqlite://data/proxy.schema-4.db?mode=rwc"
        );
    }
}
