use anyhow::Context;
use chrono::Utc;
use sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions};
use tracing::{info, warn};

pub(crate) async fn migrate(db: &SqlitePool) -> anyhow::Result<()> {
    let schema: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='schema_metadata'",
    )
    .fetch_optional(db)
    .await?;
    let version: Option<i64> = if schema.is_some() {
        sqlx::query_scalar("SELECT version FROM schema_metadata LIMIT 1")
            .fetch_optional(db)
            .await?
    } else {
        None
    };
    if version.is_some_and(|version| version != 4) {
        anyhow::bail!("unsupported database schema version {version:?}");
    }
    for sql in [
        "PRAGMA journal_mode=WAL",
        "CREATE TABLE IF NOT EXISTS schema_metadata(version INTEGER NOT NULL)",
        "INSERT INTO schema_metadata(version) SELECT 4 WHERE NOT EXISTS(SELECT 1 FROM schema_metadata)",
        "CREATE TABLE IF NOT EXISTS network_profiles(id TEXT PRIMARY KEY,name TEXT NOT NULL,draft_json TEXT NOT NULL,status TEXT NOT NULL,archived INTEGER NOT NULL DEFAULT 0,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS network_profile_revisions(id TEXT PRIMARY KEY,profile_id TEXT NOT NULL,revision INTEGER NOT NULL,sha256 TEXT NOT NULL UNIQUE,body_json TEXT NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL,UNIQUE(profile_id,revision))",
        "CREATE TABLE IF NOT EXISTS network_operations(id TEXT PRIMARY KEY,profile_revision_id TEXT NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,plan_token_hash TEXT,expires_at TEXT,detail_json TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS network_operation_events(id INTEGER PRIMARY KEY AUTOINCREMENT,operation_id TEXT NOT NULL,node_id TEXT NOT NULL,stage TEXT NOT NULL,status TEXT NOT NULL,detail_json TEXT NOT NULL,created_at TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS idx_network_events_operation ON network_operation_events(operation_id,id)",
        "CREATE TABLE IF NOT EXISTS scenarios(id TEXT PRIMARY KEY,name TEXT NOT NULL,body TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,scenario_id TEXT NOT NULL,status TEXT NOT NULL,started_at TEXT,finished_at TEXT,error TEXT,scenario_json TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS metric_samples(id INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT NOT NULL,agent_id TEXT NOT NULL,role INTEGER NOT NULL,unix_ms INTEGER NOT NULL,metrics_json TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS idx_metrics_run_time ON metric_samples(run_id,unix_ms)",
        "CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT,run_id TEXT,unix_ms INTEGER NOT NULL,level TEXT NOT NULL,message TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY,name TEXT NOT NULL,sha256 TEXT NOT NULL UNIQUE,size_bytes INTEGER NOT NULL,format TEXT NOT NULL,packet_count INTEGER NOT NULL,captured_bytes INTEGER NOT NULL,path TEXT NOT NULL,created_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS run_participants(run_id TEXT NOT NULL,agent_id TEXT NOT NULL,instance_id TEXT NOT NULL,role INTEGER NOT NULL,phase TEXT NOT NULL,last_command_id TEXT,error TEXT,updated_at TEXT NOT NULL,PRIMARY KEY(run_id,agent_id))",
    ] {
        sqlx::query(sql).execute(db).await?;
    }
    let _ = sqlx::query("ALTER TABLE runs ADD COLUMN run_name TEXT")
        .execute(db)
        .await;
    let _ = sqlx::query("ALTER TABLE artifacts ADD COLUMN kind TEXT NOT NULL DEFAULT 'pcap'")
        .execute(db)
        .await;
    let _ = sqlx::query("ALTER TABLE artifacts ADD COLUMN analysis_json TEXT")
        .execute(db)
        .await;
    Ok(())
}

pub(crate) async fn open_database(
    configured_url: &str,
) -> anyhow::Result<(SqlitePool, String, bool)> {
    let configured = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(configured_url)
        .await
        .context("open configured sqlite database")?;
    let schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_metadata')",
    )
    .fetch_one(&configured)
    .await?;
    let has_application_tables: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name IN ('runs','scenarios','artifacts','network_profiles'))",
    )
    .fetch_one(&configured)
    .await?;
    let version = if schema_exists {
        sqlx::query_scalar::<_, i64>("SELECT version FROM schema_metadata LIMIT 1")
            .fetch_optional(&configured)
            .await?
    } else {
        None
    };
    if version == Some(4) || (!schema_exists && !has_application_tables) {
        return Ok((configured, configured_url.to_owned(), false));
    }
    configured.close().await;
    let fallback_url = schema_fallback_url(configured_url)?;
    warn!(configured_url, %fallback_url, ?version, "unsupported database preserved; using schema-specific database");
    let fallback = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&fallback_url)
        .await
        .context("open schema-specific sqlite database")?;
    Ok((fallback, fallback_url, true))
}

pub(crate) fn schema_fallback_url(url: &str) -> anyhow::Result<String> {
    let (prefix, suffix) = url.split_once('?').map_or((url, ""), |(a, b)| (a, b));
    let path = prefix
        .strip_prefix("sqlite://")
        .context("DATABASE_URL must use sqlite://")?;
    if path == ":memory:" || path.is_empty() {
        anyhow::bail!("cannot create a schema-specific path for an in-memory database");
    }
    let path = std::path::Path::new(path);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .context("invalid database filename")?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("db");
    let fallback = path.with_file_name(format!("{stem}.schema-4.{extension}"));
    Ok(format!(
        "sqlite://{}{}",
        fallback.to_string_lossy().replace('\\', "/"),
        if suffix.is_empty() {
            String::new()
        } else {
            format!("?{suffix}")
        }
    ))
}

pub(crate) async fn apply_retention(db: &SqlitePool, days: i64) -> anyhow::Result<()> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    sqlx::query("DELETE FROM metric_samples WHERE run_id IN (SELECT id FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?)").bind(&cutoff).execute(db).await?;
    sqlx::query("DELETE FROM events WHERE run_id IN (SELECT id FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?)").bind(&cutoff).execute(db).await?;
    sqlx::query("DELETE FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?")
        .bind(&cutoff)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn cleanup_orphan_artifacts(db: &SqlitePool, days: i64) -> anyhow::Result<()> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let rows = sqlx::query("SELECT id,path FROM artifacts WHERE created_at < ?")
        .bind(cutoff)
        .fetch_all(db)
        .await?;
    for row in rows {
        let id: String = row.get("id");
        let pattern = format!("%{id}%");
        let scenario_refs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM scenarios WHERE body LIKE ?")
                .bind(&pattern)
                .fetch_one(db)
                .await?;
        let run_refs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE scenario_json LIKE ?")
                .bind(&pattern)
                .fetch_one(db)
                .await?;
        if scenario_refs + run_refs > 0 {
            continue;
        }
        let path: String = row.get("path");
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(artifact_id=%id, %error, "failed to remove orphan artifact file");
                continue;
            }
        }
        sqlx::query("DELETE FROM artifacts WHERE id=?")
            .bind(&id)
            .execute(db)
            .await?;
        info!(artifact_id=%id, "removed orphan artifact");
    }
    Ok(())
}
