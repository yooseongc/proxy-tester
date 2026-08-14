use uuid::Uuid;

use crate::{error::ApiError, repository, state::AppState};

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
