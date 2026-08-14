use proxy_tester_proto::v1::{ControlMessage, NetworkCommand, NetworkProgress, control_message};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{error::ApiError, state::AppState, wire};

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
