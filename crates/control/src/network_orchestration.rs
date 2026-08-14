use chrono::Utc;
use proxy_tester_domain::NetworkProfileDraft;
use proxy_tester_proto::v1::{ControlMessage, NetworkCommand, NetworkProgress, control_message};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{error::ApiError, repository, state::AppState, wire};

pub(crate) async fn command(
    state: &AppState,
    node_id: &str,
    operation_id: Uuid,
    action: &str,
    payload: serde_json::Value,
    lease_ms: i64,
) -> Result<NetworkProgress, ApiError> {
    let agent = state
        .agents
        .read()
        .await
        .get(node_id)
        .cloned()
        .ok_or_else(|| ApiError::bad(format!("node {node_id} is offline")))?;
    let command_id = Uuid::new_v4().to_string();
    let (sender, receiver) = oneshot::channel();
    state
        .pending_network
        .lock()
        .await
        .insert(command_id.clone(), sender);
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
        std::time::Duration::from_secs(state.command_timeout_secs.max(180)),
        receiver,
    )
    .await
    {
        Ok(Ok(progress)) if progress.ok => Ok(progress),
        Ok(Ok(progress)) => Err(ApiError::internal(format!(
            "node {node_id} {action} failed: {}",
            progress.error
        ))),
        _ => {
            state.pending_network.lock().await.remove(&command_id);
            Err(ApiError::internal(format!(
                "node {node_id} {action} timed out"
            )))
        }
    }
}

pub(crate) async fn plan(
    state: &AppState,
    profile_id: Uuid,
) -> Result<serde_json::Value, ApiError> {
    let row = sqlx::query("SELECT draft_json FROM network_profiles WHERE id=? AND archived=0")
        .bind(profile_id.to_string())
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| ApiError::not_found("network profile not found"))?;
    let draft: NetworkProfileDraft =
        serde_json::from_str(row.get::<String, _>("draft_json").as_str())
            .map_err(|error| ApiError::bad(error.to_string()))?;
    draft
        .validate()
        .map_err(|error| ApiError::bad(error.to_string()))?;
    let body = serde_json::to_string(&draft)?;
    let sha = format!("{:x}", Sha256::digest(body.as_bytes()));
    let existing = sqlx::query("SELECT id,revision FROM network_profile_revisions WHERE sha256=?")
        .bind(&sha)
        .fetch_optional(&state.db)
        .await?;
    let (revision_id, revision) = if let Some(row) = existing {
        (row.get::<String, _>("id"), row.get::<i64, _>("revision"))
    } else {
        let revision: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision),0)+1 FROM network_profile_revisions WHERE profile_id=?",
        )
        .bind(profile_id.to_string())
        .fetch_one(&state.db)
        .await?;
        let revision_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO network_profile_revisions(id,profile_id,revision,sha256,body_json,status,created_at) VALUES(?,?,?,?,?,'unprepared',?)")
            .bind(&revision_id)
            .bind(profile_id.to_string())
            .bind(revision)
            .bind(&sha)
            .bind(&body)
            .bind(Utc::now().to_rfc3339())
            .execute(&state.db)
            .await?;
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
    sqlx::query("INSERT INTO network_operations(id,profile_revision_id,kind,status,plan_token_hash,expires_at,detail_json,created_at,updated_at) VALUES(?,?,'plan','planned',?,?,?,?,?)")
        .bind(operation_id.to_string())
        .bind(&revision_id)
        .bind(format!("{:x}", Sha256::digest(token.as_bytes())))
        .bind(expires.to_rfc3339())
        .bind(detail.to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&state.db)
        .await?;
    let mut plans = serde_json::Map::new();
    for node in &nodes {
        let progress = command(
            state,
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
    .execute(&state.db)
    .await?;
    Ok(
        serde_json::json!({"operation_id":operation_id,"profile_revision_id":revision_id,"revision":revision,"sha256":sha,"plan_token":token,"expires_at":expires,"detail":detail}),
    )
}

pub(crate) async fn apply(
    state: &AppState,
    operation_id: Uuid,
    plan_token: &str,
) -> Result<serde_json::Value, ApiError> {
    if state.active_run.lock().await.is_some() {
        return Err(ApiError::conflict(
            "network profile cannot be applied during a run",
        ));
    }
    let row=sqlx::query("SELECT profile_revision_id,status,plan_token_hash,expires_at,detail_json FROM network_operations WHERE id=? AND kind='plan'")
        .bind(operation_id.to_string()).fetch_optional(&state.db).await?
        .ok_or_else(||ApiError::not_found("network plan not found"))?;
    if row.get::<String, _>("status") != "planned" {
        return Err(ApiError::conflict("network plan token is single-use"));
    }
    let expected: String = row.get("plan_token_hash");
    if expected != format!("{:x}", Sha256::digest(plan_token.as_bytes())) {
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
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let (_, nodes, profile_id) = revision_context(state, revision_id).await?;
    let lease = Utc::now().timestamp_millis() + 180_000;
    sqlx::query("UPDATE network_operations SET status='applying',updated_at=? WHERE id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(&state.db)
        .await?;
    let mut staged = Vec::new();
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
        if let Err(error) = command(state, node, operation_id, "stage", plan, lease).await {
            rollback_nodes(state, operation_id, &staged, lease).await;
            sqlx::query("UPDATE network_operations SET status='failed',detail_json=?,updated_at=? WHERE id=?")
                .bind(serde_json::json!({"error":error.message,"plans":plans}).to_string())
                .bind(Utc::now().to_rfc3339())
                .bind(operation_id.to_string())
                .execute(&state.db)
                .await?;
            return Err(error);
        }
        staged.push(node.clone());
    }
    for node in &nodes {
        if let Err(error) = command(
            state,
            node,
            operation_id,
            "commit",
            serde_json::Value::Null,
            lease,
        )
        .await
        {
            rollback_nodes(state, operation_id, &staged, lease).await;
            return Err(error);
        }
    }
    sqlx::query("UPDATE network_operations SET status='completed',detail_json=?,plan_token_hash=NULL,updated_at=? WHERE id=?")
        .bind(serde_json::json!({"plans":plans}).to_string())
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(&state.db)
        .await?;
    set_prepared(state, revision_id, profile_id, true).await?;
    Ok(
        serde_json::json!({"operation_id":operation_id,"profile_revision_id":revision_id,"status":"prepared"}),
    )
}

async fn rollback_nodes(state: &AppState, operation_id: Uuid, nodes: &[String], lease: i64) {
    for node in nodes {
        let _ = command(
            state,
            node,
            operation_id,
            "rollback",
            serde_json::Value::Null,
            lease,
        )
        .await;
    }
}

pub(crate) async fn teardown(
    state: &AppState,
    revision_id: Uuid,
) -> Result<serde_json::Value, ApiError> {
    if state.active_run.lock().await.is_some() {
        return Err(ApiError::conflict("active run must finish before teardown"));
    }
    let (_, nodes, profile_id) = revision_context(state, revision_id).await?;
    let operation_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO network_operations(id,profile_revision_id,kind,status,detail_json,created_at,updated_at) VALUES(?,?,'teardown','tearing_down','{}',?,?)")
        .bind(operation_id.to_string()).bind(revision_id.to_string()).bind(&now).bind(&now)
        .execute(&state.db).await?;
    for node in &nodes {
        if let Err(error) = command(
            state,
            node,
            operation_id,
            "teardown",
            serde_json::Value::Null,
            Utc::now().timestamp_millis() + 180_000,
        )
        .await
        {
            sqlx::query("UPDATE network_operations SET status='quarantined',detail_json=?,updated_at=? WHERE id=?")
                .bind(serde_json::json!({"node":node,"error":error.message}).to_string())
                .bind(Utc::now().to_rfc3339()).bind(operation_id.to_string())
                .execute(&state.db).await?;
            return Err(error);
        }
    }
    set_prepared(state, revision_id, profile_id, false).await?;
    sqlx::query("UPDATE network_operations SET status='completed',updated_at=? WHERE id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(&state.db)
        .await?;
    Ok(serde_json::json!({"operation_id":operation_id,"status":"unprepared"}))
}

async fn revision_context(
    state: &AppState,
    revision_id: Uuid,
) -> Result<(NetworkProfileDraft, Vec<String>, Uuid), ApiError> {
    repository::network_profiles::revision_context(&state.db, revision_id)
        .await?
        .ok_or_else(|| ApiError::not_found("network profile revision not found"))
}

async fn set_prepared(
    state: &AppState,
    revision_id: Uuid,
    profile_id: Uuid,
    prepared: bool,
) -> Result<(), ApiError> {
    let status = if prepared { "prepared" } else { "unprepared" };
    sqlx::query("UPDATE network_profile_revisions SET status=? WHERE id=?")
        .bind(status)
        .bind(revision_id.to_string())
        .execute(&state.db)
        .await?;
    sqlx::query("UPDATE network_profiles SET status=?,updated_at=? WHERE id=?")
        .bind(status)
        .bind(Utc::now().to_rfc3339())
        .bind(profile_id.to_string())
        .execute(&state.db)
        .await?;
    Ok(())
}
