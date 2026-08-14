use chrono::Utc;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

pub(crate) async fn upsert(
    db: &SqlitePool,
    id: Uuid,
    name: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO scenarios(id,name,body,created_at,updated_at) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,body=excluded.body,updated_at=excluded.updated_at")
        .bind(id.to_string())
        .bind(name)
        .bind(body)
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn list_bodies(db: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query("SELECT body FROM scenarios ORDER BY updated_at DESC")
        .fetch_all(db)
        .await
        .map(|rows| rows.into_iter().map(|row| row.get("body")).collect())
}
