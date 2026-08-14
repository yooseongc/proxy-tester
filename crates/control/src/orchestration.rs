use std::collections::HashSet;

use proxy_tester_domain::{PayloadKind, PayloadMode, Protocol, Scenario};
use proxy_tester_proto::v1::{ArtifactChunk, ControlMessage, control_message};
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{
    error::ApiError,
    repository,
    state::{AgentSession, AppState},
};

const ARTIFACT_CHUNK_BYTES: usize = 256 * 1024;

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

pub(crate) async fn command_agent(
    state: &AppState,
    agent_id: &str,
    agent: &AgentSession,
    run_id: Uuid,
    phase: &str,
    message: ControlMessage,
) -> Result<(), ApiError> {
    let command_id = Uuid::new_v4().to_string();
    let (sender, receiver) = oneshot::channel();
    state
        .pending_acks
        .lock()
        .await
        .insert(command_id.clone(), sender);
    repository::runs::begin_participant_command(
        &state.db,
        repository::runs::ParticipantCommand {
            run_id: &run_id.to_string(),
            agent_id,
            instance_id: &agent.instance_id,
            role: agent.role,
            phase,
            command_id: &command_id,
        },
    )
    .await?;
    if agent
        .tx
        .send(Ok(with_command_id(message, &command_id)))
        .await
        .is_err()
    {
        state.pending_acks.lock().await.remove(&command_id);
        return Err(ApiError::internal(format!(
            "{agent_id} channel closed during {phase}"
        )));
    }
    let acknowledgement = match tokio::time::timeout(
        std::time::Duration::from_secs(state.command_timeout_secs),
        receiver,
    )
    .await
    {
        Ok(Ok(acknowledgement)) => acknowledgement,
        Ok(Err(_)) => {
            return Err(ApiError::internal(format!(
                "{agent_id} {phase} acknowledgement channel closed"
            )));
        }
        Err(_) => {
            state.pending_acks.lock().await.remove(&command_id);
            return Err(ApiError::internal(format!(
                "{agent_id} {phase} acknowledgement timed out"
            )));
        }
    };
    if !acknowledgement.ok {
        return Err(ApiError::internal(format!(
            "{} {phase} failed: {}",
            acknowledgement.agent_id, acknowledgement.error
        )));
    }
    repository::runs::acknowledge_participant_command(
        &state.db,
        &run_id.to_string(),
        agent_id,
        phase,
    )
    .await?;
    Ok(())
}

pub(crate) async fn validate_artifacts(
    db: &SqlitePool,
    scenario: &Scenario,
) -> Result<(), ApiError> {
    validate_payload_artifacts(db, scenario).await?;
    validate_capture_artifact(db, scenario).await
}

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
        let artifact = repository::artifacts::find(db, id).await?.ok_or_else(|| {
            ApiError::bad(format!("{direction} payload artifact {id} does not exist"))
        })?;
        if artifact.kind != "payload" {
            return Err(ApiError::bad(format!(
                "{direction} artifact {id} is not a payload artifact"
            )));
        }
        let stored_size = artifact.size_bytes as usize;
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
    let artifact = repository::artifacts::find(db, id)
        .await?
        .ok_or_else(|| ApiError::bad(format!("capture artifact {id} does not exist")))?;
    if artifact.kind != "pcap" {
        return Err(ApiError::bad(format!(
            "artifact {id} is not a PCAP artifact"
        )));
    }
    let count_key = match scenario.protocol {
        Protocol::Http1 => "http_flow_count",
        Protocol::Http2 => "http2_flow_count",
        _ => "supported_flow_count",
    };
    if artifact.analysis[count_key].as_u64().unwrap_or(0) == 0 {
        return Err(ApiError::bad(match scenario.protocol {
            Protocol::Http1 => "capture has no supported HTTP/1.1 request/response transactions",
            Protocol::Http2 => "capture has no supported plaintext HTTP/2 transactions",
            _ => "capture has no supported bidirectional plaintext TCP flows",
        }));
    }
    Ok(())
}

pub(crate) async fn send_artifacts(
    db: &SqlitePool,
    agent: &AgentSession,
    artifact_ids: &HashSet<Uuid>,
) -> Result<(), ApiError> {
    for artifact_id in artifact_ids {
        let artifact = repository::artifacts::find(db, *artifact_id)
            .await?
            .ok_or_else(|| {
                ApiError::bad(format!("payload artifact {artifact_id} does not exist"))
            })?;
        let kind = artifact.kind;
        if kind != "payload" && kind != "pcap" {
            return Err(ApiError::bad(format!(
                "artifact {artifact_id} has unsupported kind {kind}"
            )));
        }
        let total_size = artifact.size_bytes;
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
        let mut file = tokio::fs::File::open(&artifact.path)
            .await
            .map_err(|error| ApiError::internal(format!("open artifact {artifact_id}: {error}")))?;
        let mut offset = 0_u64;
        loop {
            let mut data = vec![0; ARTIFACT_CHUNK_BYTES];
            let read = file.read(&mut data).await.map_err(|error| {
                ApiError::internal(format!("read artifact {artifact_id}: {error}"))
            })?;
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
                        sha256: artifact.sha256.clone(),
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

pub(crate) async fn finish_run(
    state: &AppState,
    run_id: Uuid,
    status: &str,
    error: Option<&str>,
) -> Result<(), ApiError> {
    repository::runs::finish(&state.db, &run_id.to_string(), status, error).await?;
    let mut active = state.active_run.lock().await;
    if *active == Some(run_id) {
        *active = None;
    }
    drop(active);
    state.run_agents.lock().await.remove(&run_id);
    state.completed_agents.lock().await.remove(&run_id);
    state.expected_endpoints.lock().await.remove(&run_id);
    let _ = state.events.send(
        serde_json::json!({"type":"run_finished","run_id":run_id,"status":status}).to_string(),
    );
    Ok(())
}
