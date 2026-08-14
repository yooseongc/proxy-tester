use sqlx::{Row, SqlitePool};

pub(crate) struct RunEvent<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) source: &'a str,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) stage: &'a str,
    pub(crate) status: &'a str,
    pub(crate) detail: serde_json::Value,
}

pub(crate) async fn append_run_event(
    db: &SqlitePool,
    event: RunEvent<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO run_operation_events(run_id,source,agent_id,stage,status,detail_json,created_at) VALUES(?,?,?,?,?,?,?)")
        .bind(event.run_id)
        .bind(event.source)
        .bind(event.agent_id)
        .bind(event.stage)
        .bind(event.status)
        .bind(event.detail.to_string())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn network_operation(
    db: &SqlitePool,
    operation_id: &str,
    event_limit: i64,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let row = sqlx::query("SELECT id,profile_revision_id,kind,status,detail_json,created_at,updated_at FROM network_operations WHERE id=?")
        .bind(operation_id)
        .fetch_optional(db)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let event_rows = sqlx::query("SELECT node_id,stage,status,detail_json,created_at FROM network_operation_events WHERE operation_id=? ORDER BY id DESC LIMIT ?")
        .bind(operation_id)
        .bind(event_limit)
        .fetch_all(db)
        .await?;
    let mut events = event_rows
        .into_iter()
        .map(|event| serde_json::json!({
            "source":"network",
            "node_id":event.get::<String,_>("node_id"),
            "stage":event.get::<String,_>("stage"),
            "status":event.get::<String,_>("status"),
            "detail":serde_json::from_str::<serde_json::Value>(event.get("detail_json")).unwrap_or_default(),
            "created_at":event.get::<String,_>("created_at")
        }))
        .collect::<Vec<_>>();
    events.reverse();
    Ok(Some(serde_json::json!({
        "id":row.get::<String,_>("id"),
        "profile_revision_id":row.get::<String,_>("profile_revision_id"),
        "kind":row.get::<String,_>("kind"),
        "status":row.get::<String,_>("status"),
        "detail":serde_json::from_str::<serde_json::Value>(row.get("detail_json")).unwrap_or_default(),
        "events":events,
        "created_at":row.get::<String,_>("created_at"),
        "updated_at":row.get::<String,_>("updated_at")
    })))
}

pub(crate) async fn run_diagnostics(
    db: &SqlitePool,
    run_id: &str,
    event_limit: i64,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let run = sqlx::query("SELECT id,status,error,started_at,finished_at FROM runs WHERE id=?")
        .bind(run_id)
        .fetch_optional(db)
        .await?;
    let Some(run) = run else { return Ok(None) };
    let participants = sqlx::query("SELECT agent_id,role,phase,last_command_id,error,updated_at FROM run_participants WHERE run_id=? ORDER BY agent_id")
        .bind(run_id).fetch_all(db).await?
        .into_iter().map(|row| serde_json::json!({
            "agent_id":row.get::<String,_>("agent_id"),"role":row.get::<i64,_>("role"),
            "phase":row.get::<String,_>("phase"),"last_command_id":row.get::<Option<String>,_>("last_command_id"),
            "error":row.get::<Option<String>,_>("error"),"updated_at":row.get::<String,_>("updated_at")
        })).collect::<Vec<_>>();
    let rows = sqlx::query("SELECT source,agent_id,stage,status,detail_json,created_at FROM run_operation_events WHERE run_id=? ORDER BY id DESC LIMIT ?")
        .bind(run_id).bind(event_limit).fetch_all(db).await?;
    let mut events = rows.into_iter().map(|row| serde_json::json!({
        "source":row.get::<String,_>("source"),"agent_id":row.get::<Option<String>,_>("agent_id"),
        "stage":row.get::<String,_>("stage"),"status":row.get::<String,_>("status"),
        "detail":serde_json::from_str::<serde_json::Value>(row.get("detail_json")).unwrap_or_default(),
        "created_at":row.get::<String,_>("created_at")
    })).collect::<Vec<_>>();
    events.reverse();
    Ok(Some(serde_json::json!({
        "id":run.get::<String,_>("id"),"status":run.get::<String,_>("status"),
        "error":run.get::<Option<String>,_>("error"),"started_at":run.get::<Option<String>,_>("started_at"),
        "finished_at":run.get::<Option<String>,_>("finished_at"),"participants":participants,"events":events
    })))
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::{RunEvent, append_run_event, run_diagnostics};

    #[tokio::test]
    async fn run_events_are_ordered_limited_and_structured() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::database::migrate(&db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,scenario_id,status,scenario_json) VALUES('run-1','scenario-1','running','{}')")
            .execute(&db).await.unwrap();
        for (stage, status) in [
            ("prepare", "sent"),
            ("prepare", "acknowledged"),
            ("run", "running"),
        ] {
            append_run_event(
                &db,
                RunEvent {
                    run_id: "run-1",
                    source: "control",
                    agent_id: Some("client-1"),
                    stage,
                    status,
                    detail: serde_json::json!({"stage":stage}),
                },
            )
            .await
            .unwrap();
        }
        let value = run_diagnostics(&db, "run-1", 2).await.unwrap().unwrap();
        let events = value["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["status"], "acknowledged");
        assert_eq!(events[1]["stage"], "run");
        assert_eq!(events[1]["detail"]["stage"], "run");
    }
}
