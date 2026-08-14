use sqlx::{Row, SqlitePool};
use uuid::Uuid;

pub(crate) struct ArtifactRecord {
    pub(crate) kind: String,
    pub(crate) sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) path: String,
    pub(crate) analysis: serde_json::Value,
}

pub(crate) async fn find(db: &SqlitePool, id: Uuid) -> Result<Option<ArtifactRecord>, sqlx::Error> {
    sqlx::query("SELECT kind,sha256,size_bytes,path,analysis_json FROM artifacts WHERE id=?")
        .bind(id.to_string())
        .fetch_optional(db)
        .await
        .map(|row| {
            row.map(|row| ArtifactRecord {
                kind: row.get("kind"),
                sha256: row.get("sha256"),
                size_bytes: row.get::<i64, _>("size_bytes") as u64,
                path: row.get("path"),
                analysis: row
                    .get::<Option<String>, _>("analysis_json")
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
            })
        })
}

pub(crate) async fn list(db: &SqlitePool) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query("SELECT id,kind,name,sha256,size_bytes,format,packet_count,captured_bytes,analysis_json,created_at FROM artifacts ORDER BY created_at DESC")
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let analysis = row
                .get::<Option<String>, _>("analysis_json")
                .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
                .unwrap_or_default();
            serde_json::json!({
                "id": row.get::<String, _>("id"),
                "kind": row.get::<String, _>("kind"),
                "name": row.get::<String, _>("name"),
                "sha256": row.get::<String, _>("sha256"),
                "size_bytes": row.get::<i64, _>("size_bytes"),
                "format": row.get::<String, _>("format"),
                "packet_count": row.get::<i64, _>("packet_count"),
                "captured_bytes": row.get::<i64, _>("captured_bytes"),
                "analysis": analysis,
                "created_at": row.get::<String, _>("created_at"),
            })
        })
        .collect())
}
