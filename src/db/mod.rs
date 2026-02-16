use std::{path::Path, str::FromStr};

use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::{config::Config, errors::Result, models::envelope::HookEnvelope};

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    pub tmux_pane: String,
    pub last_seen_at: String,
}

pub async fn connect(config: &Config) -> Result<SqlitePool> {
    if let Some(parent) = Path::new(&config.db_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let uri = format!("sqlite://{}", config.db_path);
    let options = SqliteConnectOptions::from_str(&uri)
        .map_err(|err| crate::errors::AppError::DbConfig(err.to_string()))?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    bootstrap(&pool).await?;
    Ok(pool)
}

pub async fn bootstrap(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            cwd TEXT NOT NULL,
            tmux_pane TEXT NOT NULL,
            last_seen_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn upsert_session(
    pool: &SqlitePool,
    envelope: &HookEnvelope,
    now_epoch: i64,
) -> Result<()> {
    let pane = envelope.tmux_pane.as_deref().unwrap_or_default();
    sqlx::query(
        r#"
        INSERT INTO sessions (session_id, name, cwd, tmux_pane, last_seen_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(session_id) DO UPDATE SET
            name = excluded.name,
            cwd = excluded.cwd,
            tmux_pane = excluded.tmux_pane,
            last_seen_at = excluded.last_seen_at
        "#,
    )
    .bind(&envelope.session_id)
    .bind(&envelope.session_name)
    .bind(&envelope.cwd)
    .bind(pane)
    .bind(now_epoch.to_string())
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn list_sessions(pool: &SqlitePool) -> Result<Vec<SessionRecord>> {
    let rows = sqlx::query(
        r#"
        SELECT session_id, name, cwd, tmux_pane, last_seen_at
        FROM sessions
        ORDER BY last_seen_at DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(SessionRecord {
            session_id: row.try_get::<String, _>("session_id")?,
            name: row.try_get::<String, _>("name")?,
            cwd: row.try_get::<String, _>("cwd")?,
            tmux_pane: row.try_get::<String, _>("tmux_pane")?,
            last_seen_at: row.try_get::<String, _>("last_seen_at")?,
        });
    }
    Ok(out)
}
