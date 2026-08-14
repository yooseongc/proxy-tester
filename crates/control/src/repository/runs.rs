use sqlx::{Row, SqlitePool};

pub(crate) struct RunRecord {
    pub(crate) rowid: Option<i64>,
    pub(crate) id: String,
    pub(crate) scenario_id: Option<String>,
    pub(crate) run_name: Option<String>,
    pub(crate) status: String,
    pub(crate) started_at: Option<String>,
    pub(crate) finished_at: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) scenario_json: String,
}

pub(crate) struct MetricSampleRecord {
    pub(crate) agent_id: String,
    pub(crate) role: i64,
    pub(crate) unix_ms: i64,
    pub(crate) metrics_json: String,
}

fn run_from_row(row: sqlx::sqlite::SqliteRow, include_rowid: bool) -> RunRecord {
    RunRecord {
        rowid: include_rowid.then(|| row.get("rowid")),
        id: row.get("id"),
        scenario_id: row.get("scenario_id"),
        run_name: row.get("run_name"),
        status: row.get("status"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        error: row.get("error"),
        scenario_json: row.get("scenario_json"),
    }
}

fn sample_from_row(row: sqlx::sqlite::SqliteRow) -> MetricSampleRecord {
    MetricSampleRecord {
        agent_id: row.get("agent_id"),
        role: row.get("role"),
        unix_ms: row.get("unix_ms"),
        metrics_json: row.get("metrics_json"),
    }
}

pub(crate) async fn list_recent(
    db: &SqlitePool,
    limit: i64,
) -> Result<Vec<RunRecord>, sqlx::Error> {
    let rows = sqlx::query("SELECT id,scenario_id,run_name,status,started_at,finished_at,error,scenario_json FROM runs ORDER BY rowid DESC LIMIT ?")
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| run_from_row(row, false))
        .collect())
}

pub(crate) async fn list_page(
    db: &SqlitePool,
    cursor: i64,
    limit: i64,
) -> Result<Vec<RunRecord>, sqlx::Error> {
    let rows = sqlx::query("SELECT rowid,id,scenario_id,run_name,status,started_at,finished_at,error,scenario_json FROM runs WHERE rowid < ? ORDER BY rowid DESC LIMIT ?")
        .bind(cursor)
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| run_from_row(row, true))
        .collect())
}

pub(crate) async fn find(db: &SqlitePool, id: &str) -> Result<Option<RunRecord>, sqlx::Error> {
    let row = sqlx::query("SELECT id,scenario_id,run_name,status,started_at,finished_at,error,scenario_json FROM runs WHERE id=?")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|row| run_from_row(row, false)))
}

pub(crate) async fn samples(
    db: &SqlitePool,
    run_id: &str,
    from_unix_ms: i64,
    to_unix_ms: i64,
) -> Result<Vec<MetricSampleRecord>, sqlx::Error> {
    let rows = sqlx::query("SELECT agent_id,role,unix_ms,metrics_json FROM metric_samples WHERE run_id=? AND unix_ms>=? AND unix_ms<=? ORDER BY unix_ms")
        .bind(run_id)
        .bind(from_unix_ms)
        .bind(to_unix_ms)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(sample_from_row).collect())
}

pub(crate) async fn finish(
    db: &SqlitePool,
    run_id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE runs SET status=?,finished_at=?,error=? WHERE id=?")
        .bind(status)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(error)
        .bind(run_id)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) struct ParticipantCommand<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) agent_id: &'a str,
    pub(crate) instance_id: &'a str,
    pub(crate) role: i32,
    pub(crate) phase: &'a str,
    pub(crate) command_id: &'a str,
}

pub(crate) async fn begin_participant_command(
    db: &SqlitePool,
    command: ParticipantCommand<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO run_participants(run_id,agent_id,instance_id,role,phase,last_command_id,error,updated_at) VALUES(?,?,?,?,?,?,NULL,?) ON CONFLICT(run_id,agent_id) DO UPDATE SET instance_id=excluded.instance_id,phase=excluded.phase,last_command_id=excluded.last_command_id,error=NULL,updated_at=excluded.updated_at")
        .bind(command.run_id)
        .bind(command.agent_id)
        .bind(command.instance_id)
        .bind(command.role)
        .bind(command.phase)
        .bind(command.command_id)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn acknowledge_participant_command(
    db: &SqlitePool,
    run_id: &str,
    agent_id: &str,
    phase: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE run_participants SET phase=?,updated_at=? WHERE run_id=? AND agent_id=?")
        .bind(format!("{phase}_acked"))
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(run_id)
        .bind(agent_id)
        .execute(db)
        .await?;
    Ok(())
}
