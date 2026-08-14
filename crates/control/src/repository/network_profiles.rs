use proxy_tester_domain::{NetworkProfileDraft, NetworkProfileRevision};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

pub(crate) async fn list_profiles(db: &SqlitePool) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query("SELECT id,name,draft_json,status,archived,created_at,updated_at FROM network_profiles ORDER BY updated_at DESC")
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "id": row.get::<String, _>("id"),
                "name": row.get::<String, _>("name"),
                "draft": serde_json::from_str::<serde_json::Value>(row.get("draft_json"))
                    .unwrap_or_default(),
                "status": row.get::<String, _>("status"),
                "archived": row.get::<i64, _>("archived") != 0,
                "created_at": row.get::<String, _>("created_at"),
                "updated_at": row.get::<String, _>("updated_at")
            })
        })
        .collect())
}

pub(crate) async fn upsert(
    db: &SqlitePool,
    draft: &NetworkProfileDraft,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let body = serde_json::to_string(draft).map_err(|error| sqlx::Error::Decode(error.into()))?;
    sqlx::query("INSERT INTO network_profiles(id,name,draft_json,status,created_at,updated_at) VALUES(?,?,?,'draft',?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,draft_json=excluded.draft_json,status=CASE WHEN network_profiles.status='prepared' THEN 'prepared' ELSE 'draft' END,updated_at=excluded.updated_at")
        .bind(draft.id.to_string())
        .bind(&draft.name)
        .bind(body)
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) enum ArchiveResult {
    Archived,
    PreparedRevision,
    NotFound,
}

pub(crate) async fn archive(
    db: &SqlitePool,
    profile_id: Uuid,
) -> Result<ArchiveResult, sqlx::Error> {
    let prepared: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM network_profile_revisions WHERE profile_id=? AND status='prepared'",
    )
    .bind(profile_id.to_string())
    .fetch_one(db)
    .await?;
    if prepared > 0 {
        return Ok(ArchiveResult::PreparedRevision);
    }
    let result = sqlx::query(
        "UPDATE network_profiles SET archived=1,status='archived',updated_at=? WHERE id=?",
    )
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(profile_id.to_string())
    .execute(db)
    .await?;
    Ok(if result.rows_affected() == 0 {
        ArchiveResult::NotFound
    } else {
        ArchiveResult::Archived
    })
}

pub(crate) async fn list_revisions(
    db: &SqlitePool,
    profile_id: Option<Uuid>,
) -> Result<Vec<NetworkProfileRevision>, sqlx::Error> {
    let rows = if let Some(profile_id) = profile_id {
        sqlx::query("SELECT id,profile_id,revision,sha256,body_json FROM network_profile_revisions WHERE profile_id=? ORDER BY revision DESC")
            .bind(profile_id.to_string())
            .fetch_all(db)
            .await?
    } else {
        sqlx::query("SELECT id,profile_id,revision,sha256,body_json FROM network_profile_revisions ORDER BY created_at DESC")
            .fetch_all(db)
            .await?
    };
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(NetworkProfileRevision {
                id: Uuid::parse_str(row.get::<String, _>("id").as_str()).ok()?,
                profile_id: Uuid::parse_str(row.get::<String, _>("profile_id").as_str()).ok()?,
                revision: row.get::<i64, _>("revision") as u32,
                sha256: row.get("sha256"),
                body: serde_json::from_str(row.get("body_json")).ok()?,
            })
        })
        .collect())
}

pub(crate) async fn revision_context(
    db: &SqlitePool,
    revision_id: Uuid,
) -> Result<Option<(NetworkProfileDraft, Vec<String>, Uuid)>, sqlx::Error> {
    let row = sqlx::query("SELECT profile_id,body_json FROM network_profile_revisions WHERE id=?")
        .bind(revision_id.to_string())
        .fetch_optional(db)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let draft: NetworkProfileDraft = serde_json::from_str(row.get("body_json"))
        .map_err(|error| sqlx::Error::Decode(error.into()))?;
    let mut nodes = vec![
        draft.client_endpoint.node_id.clone(),
        draft.server_endpoint.node_id.clone(),
    ];
    nodes.sort();
    nodes.dedup();
    let profile_id = Uuid::parse_str(row.get::<String, _>("profile_id").as_str())
        .map_err(|error| sqlx::Error::Decode(error.into()))?;
    Ok(Some((draft, nodes, profile_id)))
}

pub(crate) async fn set_prepared(
    db: &SqlitePool,
    revision_id: Uuid,
    profile_id: Uuid,
    prepared: bool,
) -> Result<(), sqlx::Error> {
    let status = if prepared { "prepared" } else { "unprepared" };
    let mut transaction = db.begin().await?;
    sqlx::query("UPDATE network_profile_revisions SET status=? WHERE id=?")
        .bind(status)
        .bind(revision_id.to_string())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE network_profiles SET status=?,updated_at=? WHERE id=?")
        .bind(status)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(profile_id.to_string())
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}
