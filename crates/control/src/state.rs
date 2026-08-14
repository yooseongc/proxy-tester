use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use proxy_tester_proto::v1::{ControlMessage, NetworkProgress};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct AgentSession {
    pub(crate) instance_id: String,
    pub(crate) generation: Uuid,
    pub(crate) role: i32,
    pub(crate) hostname: String,
    pub(crate) interfaces: Vec<String>,
    pub(crate) inventory_json: String,
    pub(crate) last_seen_ms: i64,
    pub(crate) tx: mpsc::Sender<Result<ControlMessage, Status>>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) db: SqlitePool,
    pub(crate) database_url: Arc<String>,
    pub(crate) database_fallback: bool,
    pub(crate) agents: Arc<RwLock<HashMap<String, AgentSession>>>,
    pub(crate) events: broadcast::Sender<String>,
    pub(crate) active_run: Arc<Mutex<Option<Uuid>>>,
    pub(crate) run_agents: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
    pub(crate) completed_agents: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
    pub(crate) expected_endpoints: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
    pub(crate) pending_acks: Arc<Mutex<HashMap<String, oneshot::Sender<CommandAckResult>>>>,
    pub(crate) pending_network: Arc<Mutex<HashMap<String, oneshot::Sender<NetworkProgress>>>>,
    pub(crate) agent_grace_secs: u64,
    pub(crate) command_timeout_secs: u64,
    pub(crate) artifact_dir: Arc<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct CommandAckResult {
    pub(crate) agent_id: String,
    pub(crate) ok: bool,
    pub(crate) error: String,
}
