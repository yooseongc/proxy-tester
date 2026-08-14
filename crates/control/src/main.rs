use anyhow::Context;
use axum::{
    Json,
    extract::{
        Multipart, Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use clap::Parser;
use futures::{Stream, StreamExt};
use proxy_tester_capture::{CaptureFormat, analyze_capture as analyze_tcp_capture};
use proxy_tester_domain::{
    MetricsSnapshot, NetworkProfileDraft, NetworkProfileRevision, PayloadKind, PayloadMode,
    Scenario,
};
use proxy_tester_proto::v1::{
    AgentMessage, ControlMessage, NetworkCommand, NetworkProgress, PrepareRun, SetPaused, StartRun,
    StopRun,
    agent_control_server::{AgentControl, AgentControlServer},
    agent_message, control_message,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::{collections::HashSet, pin::Pin, sync::Arc};
use tokio::{
    io::AsyncWriteExt,
    sync::{broadcast, mpsc, oneshot, watch},
};
use tonic::{Request, Response as GrpcResponse, Status};
use tracing::{info, warn};
use uuid::Uuid;
mod database;
mod error;
mod orchestration;
mod repository;
mod routes;
mod state;
mod wire;

#[cfg(test)]
use database::schema_fallback_url;
use database::{apply_retention, cleanup_orphan_artifacts, migrate, open_database};
use error::ApiError;
use state::{AgentSession, AppState, CommandAckResult};

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

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let grpc_state = state.clone();
    let grpc_addr = args.grpc_addr.parse()?;
    let grpc_shutdown = shutdown_rx.clone();
    let mut grpc_task = tokio::spawn(async move {
        info!(%grpc_addr,"agent gRPC listening");
        tonic::transport::Server::builder()
            .add_service(AgentControlServer::new(ControlSvc(grpc_state)))
            .serve_with_shutdown(grpc_addr, wait_for_shutdown(grpc_shutdown))
            .await
    });

    let app = routes::build(state, &args.static_dir);
    let listener = tokio::net::TcpListener::bind(&args.http_addr).await?;
    info!(addr=%args.http_addr,"control HTTP listening");
    let http_shutdown = shutdown_rx;
    let mut http_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(wait_for_shutdown(http_shutdown))
            .await
    });

    let mut grpc_finished = false;
    let mut http_finished = false;
    let first_error = tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for shutdown signal")?;
            info!("shutdown signal received");
            None
        }
        result = &mut grpc_task => {
            grpc_finished = true;
            Some(join_server_result("gRPC", result))
        }
        result = &mut http_task => {
            http_finished = true;
            Some(join_server_result("HTTP", result))
        }
    };

    let _ = shutdown_tx.send(true);
    if !grpc_finished {
        join_server_result("gRPC", grpc_task.await)?;
    }
    if !http_finished {
        join_server_result("HTTP", http_task.await)?;
    }
    if let Some(result) = first_error {
        result?;
        anyhow::bail!("server stopped unexpectedly");
    }
    info!("control shutdown complete");
    Ok(())
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

fn join_server_result<E>(
    name: &str,
    result: Result<Result<(), E>, tokio::task::JoinError>,
) -> anyhow::Result<()>
where
    E: std::error::Error + Send + Sync + 'static,
{
    result
        .with_context(|| format!("{name} server task failed"))?
        .with_context(|| format!("{name} server stopped with an error"))
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
            wire::plan_to_json(
                progress
                    .plan
                    .ok_or_else(|| ApiError::internal("agent plan response is missing plan"))?,
            ),
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
                lease_expires_unix_ms: lease_ms,
                action: Some(wire::network_action(action, payload)?),
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
        Ok(Ok(progress)) if progress.ok => Ok(progress),
        Ok(Ok(progress)) => Err(ApiError::internal(format!(
            "node {node_id} {action} failed: {}",
            progress.error
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
    orchestration::validate_artifacts(&s.db, &sc).await?;
    let (client_id, server_id, profile_ready) = orchestration::scenario_nodes(&s.db, &sc).await?;
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
    Ok(Json(repository::artifacts::list(&s.db).await?))
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
    orchestration::validate_artifacts(&s.db, &sc).await?;
    let body = serde_json::to_string(&sc)?;
    repository::scenarios::upsert(&s.db, sc.id, &sc.name, &body).await?;
    Ok(Json(sc))
}
async fn list_scenarios(State(s): State<AppState>) -> Result<Json<Vec<Scenario>>, ApiError> {
    Ok(Json(
        repository::scenarios::list_bodies(&s.db)
            .await?
            .into_iter()
            .filter_map(|body| serde_json::from_str::<Scenario>(&body).ok())
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
    let (client_id, server_id, profile_ready) = orchestration::scenario_nodes(&s.db, &sc).await?;
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
    let (client_runtime, server_runtime) = orchestration::scenario_runtime(&s.db, &sc).await?;
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
    repository::runs::create(
        &s.db,
        repository::runs::NewRun {
            id: &run_id.to_string(),
            scenario_id: &sc.id.to_string(),
            scenario_json: &json,
            run_name: &run_name,
        },
    )
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
        orchestration::send_artifacts(&s.db, &client, &artifact_ids).await?;
        if client_id != server_id {
            orchestration::send_artifacts(&s.db, &server, &artifact_ids).await?;
        }
        orchestration::command_agent(&s, &client_id, &client, run_id, "prepare", prepare.clone())
            .await?;
        let mut server_prepare = prepare;
        if let Some(control_message::Body::Prepare(value)) = server_prepare.body.as_mut() {
            value.endpoint_role = 2;
            value.target_addr = server_runtime.0;
            value.interface_name = server_runtime.1;
            value.namespace = server_runtime.2;
            value.source_ips = server_runtime.3;
        }
        orchestration::command_agent(&s, &server_id, &server, run_id, "prepare", server_prepare)
            .await?;
        Ok::<_, ApiError>(())
    }
    .await;
    if let Err(error) = preparation {
        repository::runs::finish(&s.db, &run_id.to_string(), "failed", Some(&error.message))
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
        orchestration::command_agent(&s, &client_id, &client, run_id, "start", start.clone())
            .await?;
        let mut server_start = start;
        if let Some(control_message::Body::Start(value)) = server_start.body.as_mut() {
            value.endpoint_role = 2;
        }
        orchestration::command_agent(&s, &server_id, &server, run_id, "start", server_start)
            .await?;
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
        repository::runs::finish(&s.db, &run_id.to_string(), "failed", Some(&error.message))
            .await?;
        return Err(error);
    }
    repository::runs::mark_running(&s.db, &run_id.to_string(), &started_at.to_rfc3339()).await?;
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
            let _ = orchestration::command_agent(
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
    orchestration::finish_run(&s, id, "cancelled", None).await?;
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
        orchestration::command_agent(
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
    repository::runs::set_status(&s.db, &id.to_string(), status).await?;
    let _ = s
        .events
        .send(serde_json::json!({"type":"run_state","run_id":id,"status":status}).to_string());
    Ok(Json(serde_json::json!({"id":id,"status":status})))
}
async fn active_run(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"run_id":*s.active_run.lock().await}))
}
async fn list_runs(State(s): State<AppState>) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let rows = repository::runs::list_recent(&s.db, 100).await?;
    Ok(Json(rows.into_iter().map(run_list_json).collect()))
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
    let rows = repository::runs::list_page(&s.db, cursor, limit + 1).await?;
    let next_cursor = (rows.len() as i64 > limit)
        .then(|| rows[limit as usize - 1].rowid)
        .flatten();
    let items = rows
        .into_iter()
        .take(limit as usize)
        .map(run_list_json)
        .collect::<Vec<_>>();
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
    let rows = repository::runs::samples(&s.db, &id.to_string(), from, to).await?;
    let stride = (rows.len().saturating_add(maximum - 1) / maximum).max(1);
    let samples = rows
        .into_iter()
        .enumerate()
        .filter(|(index, _)| index % stride == 0)
        .map(|(_, row)| metric_sample_json(row))
        .collect::<Vec<_>>();
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
fn run_list_json(run: repository::runs::RunRecord) -> serde_json::Value {
    serde_json::json!({
        "id": run.id,
        "scenario_id": run.scenario_id,
        "run_name": run.run_name,
        "status": run.status,
        "started_at": run.started_at,
        "finished_at": run.finished_at,
        "error": run.error,
        "scenario": redacted_scenario(&run.scenario_json)
    })
}
fn metric_sample_json(sample: repository::runs::MetricSampleRecord) -> serde_json::Value {
    serde_json::json!({
        "agent_id": sample.agent_id,
        "role": sample.role,
        "unix_ms": sample.unix_ms,
        "metrics": serde_json::from_str::<serde_json::Value>(&sample.metrics_json)
            .unwrap_or_default()
    })
}
async fn run_detail(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let run = repository::runs::find(&s.db, &id.to_string())
        .await?
        .ok_or_else(|| ApiError::not_found("run not found"))?;
    let samples = repository::runs::samples(&s.db, &id.to_string(), i64::MIN, i64::MAX)
        .await?
        .into_iter()
        .map(metric_sample_json)
        .collect::<Vec<_>>();
    let payload_metadata = result_payload_metadata(&run.scenario_json, &samples);
    Ok(Json(
        serde_json::json!({"id":run.id,"run_name":run.run_name,"started_at":run.started_at,"finished_at":run.finished_at,"status":run.status,"error":run.error,"scenario":redacted_scenario(&run.scenario_json),"payload_metadata":payload_metadata,"samples":samples }),
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
        let id = hello.node_id.clone();
        let role = 0;
        let inventory = hello
            .inventory
            .ok_or_else(|| Status::invalid_argument("node inventory required"))?;
        let interfaces = inventory
            .interfaces
            .iter()
            .map(|v| v.name.clone())
            .collect();
        let inventory_json = wire::inventory_to_json(inventory).to_string();
        let generation = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(64);
        self.0.agents.write().await.insert(
            id.clone(),
            AgentSession {
                instance_id: hello.instance_id,
                generation,
                role,
                hostname: hello.hostname,
                interfaces,
                inventory_json,
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
                            .bind(&progress.operation_id).bind(&id).bind(&progress.stage).bind(if progress.ok {"completed"} else {"failed"})
                            .bind(if let Some(plan)=progress.plan.clone(){wire::plan_to_json(plan).to_string()}else{serde_json::json!({"error":progress.error}).to_string()}).bind(Utc::now().to_rfc3339()).execute(&state.db).await;
                        if let Some(waiter) = state
                            .pending_network
                            .lock()
                            .await
                            .remove(&progress.command_id)
                        {
                            let _ = waiter.send(progress.clone());
                        }
                        let _=state.events.send(serde_json::json!({"type":"network_progress","node_id":id,"operation_id":progress.operation_id,"stage":progress.stage,"status":if progress.ok{"completed"}else{"failed"},"error":progress.error}).to_string());
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
                            let _ =
                                orchestration::finish_run(&state, run_id, "failed", Some(&reason))
                                    .await;
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
                                let _ =
                                    orchestration::finish_run(&state, run_id, "completed", None)
                                        .await;
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
        let _ = orchestration::finish_run(
            &state,
            run_id,
            "failed",
            Some(&format!("{reason}: {agent_id}")),
        )
        .await;
    });
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
