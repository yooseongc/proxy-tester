use std::collections::HashSet;

use chrono::Utc;
use proxy_tester_domain::{
    NetworkProfileDraft, PayloadKind, PayloadMode, Protocol, Scenario, ScenarioPath,
};
use proxy_tester_proto::v1::{
    ArtifactChunk, ControlMessage, PrepareRun, SetPaused, StartRun, StopRun, control_message,
};
use sqlx::{Row, SqlitePool};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{
    error::ApiError,
    repository,
    state::{AgentSession, AppState},
};

const ARTIFACT_CHUNK_BYTES: usize = 256 * 1024;

pub(crate) type EndpointRuntime = (String, String, String, Vec<String>);

pub(crate) async fn scenario_nodes(
    db: &SqlitePool,
    scenario: &Scenario,
) -> Result<(String, String, bool), ApiError> {
    match &scenario.path {
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
                    .map_err(|error| ApiError::internal(error.to_string()))?;
            Ok((
                body.client_endpoint.node_id,
                body.server_endpoint.node_id,
                row.get::<String, _>("status") == "prepared",
            ))
        }
    }
}

pub(crate) async fn scenario_runtime(
    db: &SqlitePool,
    scenario: &Scenario,
) -> Result<(EndpointRuntime, EndpointRuntime), ApiError> {
    match &scenario.path {
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
                .map(|value| value.0)
                .ok_or_else(|| ApiError::bad("invalid server pool"))?;
            let revision = profile_revision_id.to_string();
            let short = &revision[..8];
            let (start, _) = draft
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

pub(crate) async fn start_run(
    state: &AppState,
    scenario: Scenario,
    requested_name: Option<String>,
) -> Result<Uuid, ApiError> {
    scenario
        .validate()
        .map_err(|error| ApiError::bad(error.to_string()))?;
    validate_artifacts(&state.db, &scenario).await?;
    let mut artifact_ids: HashSet<Uuid> = [
        scenario.request_payload.clone(),
        scenario.response_payload.clone(),
    ]
    .into_iter()
    .filter(|payload| payload.kind == PayloadKind::File)
    .filter_map(|payload| payload.artifact_id)
    .collect();
    if let Some(capture_id) = scenario
        .capture_artifact_id
        .filter(|_| scenario.payload_mode == PayloadMode::CaptureReplay)
    {
        artifact_ids.insert(capture_id);
    }
    let mut active = state.active_run.lock().await;
    if active.is_some() {
        return Err(ApiError::conflict("이미 실행 중인 시험이 있습니다"));
    }
    let (client_id, server_id, profile_ready) = scenario_nodes(&state.db, &scenario).await?;
    if !profile_ready {
        return Err(ApiError::conflict(
            "referenced network profile revision is not prepared",
        ));
    }
    let sessions = state.agents.read().await;
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
    let scenario_json = serde_json::to_string(&scenario)?;
    let (client_runtime, server_runtime) = scenario_runtime(&state.db, &scenario).await?;
    let started_at = Utc::now();
    let run_name = requested_name
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{} · {}",
                scenario.name,
                started_at.format("%Y-%m-%d %H:%M:%S UTC")
            )
        });
    repository::runs::create(
        &state.db,
        repository::runs::NewRun {
            id: &run_id.to_string(),
            scenario_id: &scenario.id.to_string(),
            scenario_json: &scenario_json,
            run_name: &run_name,
        },
    )
    .await?;

    let prepare = ControlMessage {
        body: Some(control_message::Body::Prepare(PrepareRun {
            run_id: run_id.to_string(),
            scenario_json,
            command_id: String::new(),
            endpoint_role: 1,
            target_addr: client_runtime.0,
            interface_name: client_runtime.1,
            namespace: client_runtime.2,
            source_ips: client_runtime.3,
        })),
    };
    let preparation = async {
        send_artifacts(&state.db, &client, &artifact_ids).await?;
        if client_id != server_id {
            send_artifacts(&state.db, &server, &artifact_ids).await?;
        }
        command_agent(
            state,
            &client_id,
            &client,
            run_id,
            "prepare",
            prepare.clone(),
        )
        .await?;
        let mut server_prepare = prepare;
        if let Some(control_message::Body::Prepare(value)) = server_prepare.body.as_mut() {
            value.endpoint_role = 2;
            value.target_addr = server_runtime.0;
            value.interface_name = server_runtime.1;
            value.namespace = server_runtime.2;
            value.source_ips = server_runtime.3;
        }
        command_agent(
            state,
            &server_id,
            &server,
            run_id,
            "prepare",
            server_prepare,
        )
        .await
    }
    .await;
    if let Err(error) = preparation {
        repository::runs::finish(
            &state.db,
            &run_id.to_string(),
            "failed",
            Some(&error.message),
        )
        .await?;
        return Err(error);
    }

    let start = ControlMessage {
        body: Some(control_message::Body::Start(StartRun {
            run_id: run_id.to_string(),
            start_unix_ms: Utc::now().timestamp_millis() + 1000,
            command_id: String::new(),
            endpoint_role: 1,
        })),
    };
    let starting = async {
        command_agent(state, &client_id, &client, run_id, "start", start.clone()).await?;
        let mut server_start = start;
        if let Some(control_message::Body::Start(value)) = server_start.body.as_mut() {
            value.endpoint_role = 2;
        }
        command_agent(state, &server_id, &server, run_id, "start", server_start).await
    }
    .await;
    if let Err(error) = starting {
        stop_agents(run_id, [&client, &server]).await;
        repository::runs::finish(
            &state.db,
            &run_id.to_string(),
            "failed",
            Some(&error.message),
        )
        .await?;
        return Err(error);
    }

    repository::runs::mark_running(&state.db, &run_id.to_string(), &started_at.to_rfc3339())
        .await?;
    *active = Some(run_id);
    state.run_agents.lock().await.insert(
        run_id,
        HashSet::from([client_id.clone(), server_id.clone()]),
    );
    state.expected_endpoints.lock().await.insert(
        run_id,
        HashSet::from([format!("{client_id}:1"), format!("{server_id}:2")]),
    );
    let _ = state
        .events
        .send(serde_json::json!({"type":"run_started","run_id":run_id}).to_string());
    Ok(run_id)
}

async fn stop_agents<'a>(run_id: Uuid, agents: impl IntoIterator<Item = &'a AgentSession>) {
    for agent in agents {
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
}

pub(crate) async fn stop_run(state: &AppState, run_id: Uuid) -> Result<(), ApiError> {
    if *state.active_run.lock().await != Some(run_id) {
        return Err(ApiError::bad("실행 중인 run이 아닙니다"));
    }
    let agents = state.agents.read().await.clone();
    let participant_ids = state
        .run_agents
        .lock()
        .await
        .get(&run_id)
        .cloned()
        .unwrap_or_default();
    for agent_id in participant_ids {
        if let Some(agent) = agents.get(&agent_id) {
            let _ = command_agent(
                state,
                &agent_id,
                agent,
                run_id,
                "stop",
                ControlMessage {
                    body: Some(control_message::Body::Stop(StopRun {
                        run_id: run_id.to_string(),
                        command_id: String::new(),
                        endpoint_role: 0,
                    })),
                },
            )
            .await;
        }
    }
    finish_run(state, run_id, "cancelled", None).await
}

pub(crate) async fn set_run_paused(
    state: &AppState,
    run_id: Uuid,
    paused: bool,
) -> Result<&'static str, ApiError> {
    if *state.active_run.lock().await != Some(run_id) {
        return Err(ApiError::bad("실행 중인 run이 아닙니다"));
    }
    let message = ControlMessage {
        body: Some(control_message::Body::SetPaused(SetPaused {
            run_id: run_id.to_string(),
            paused,
            command_id: String::new(),
            endpoint_role: 0,
        })),
    };
    let agents = state.agents.read().await.clone();
    let participant_ids = state
        .run_agents
        .lock()
        .await
        .get(&run_id)
        .cloned()
        .unwrap_or_default();
    for agent_id in participant_ids {
        let agent = agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::internal(format!("{agent_id} is disconnected")))?;
        command_agent(
            state,
            &agent_id,
            agent,
            run_id,
            if paused { "pause" } else { "resume" },
            message.clone(),
        )
        .await?;
    }
    let status = if paused { "paused" } else { "running" };
    repository::runs::set_status(&state.db, &run_id.to_string(), status).await?;
    let _ = state
        .events
        .send(serde_json::json!({"type":"run_state","run_id":run_id,"status":status}).to_string());
    Ok(status)
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
