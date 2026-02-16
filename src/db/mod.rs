use std::{path::Path, str::FromStr};

use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::{
    config::Config,
    errors::{AppError, Result},
    models::envelope::HookEnvelope,
};

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    pub tmux_pane: String,
    pub last_seen_at: String,
}

#[derive(Debug, Clone)]
pub struct ReplyRoute {
    pub session_id: String,
    pub tmux_pane: String,
}

#[derive(Debug, Clone)]
pub struct DefaultRoute {
    pub session_id: String,
    pub session_name: String,
    pub tmux_pane: String,
}

pub async fn connect(config: &Config) -> Result<SqlitePool> {
    if let Some(parent) = Path::new(&config.db_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let uri = format!("sqlite://{}", config.db_path);
    let options = SqliteConnectOptions::from_str(&uri)
        .map_err(|err| AppError::DbConfig(err.to_string()))?
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

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pending_requests (
            request_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            session_name TEXT NOT NULL,
            tmux_pane TEXT NOT NULL,
            hook_event_name TEXT NOT NULL,
            state TEXT NOT NULL,
            telegram_message_id INTEGER,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reply_routes (
            telegram_message_id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            tmux_pane TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS default_route (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            session_id TEXT NOT NULL,
            session_name TEXT NOT NULL,
            tmux_pane TEXT NOT NULL,
            updated_at INTEGER NOT NULL
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

pub async fn get_session(pool: &SqlitePool, session_id: &str) -> Result<Option<SessionRecord>> {
    let row = sqlx::query(
        r#"
        SELECT session_id, name, cwd, tmux_pane, last_seen_at
        FROM sessions
        WHERE session_id = ?1
        LIMIT 1
        "#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(SessionRecord {
            session_id: row.try_get::<String, _>("session_id")?,
            name: row.try_get::<String, _>("name")?,
            cwd: row.try_get::<String, _>("cwd")?,
            tmux_pane: row.try_get::<String, _>("tmux_pane")?,
            last_seen_at: row.try_get::<String, _>("last_seen_at")?,
        })),
        None => Ok(None),
    }
}

pub async fn insert_pending_request(
    pool: &SqlitePool,
    envelope: &HookEnvelope,
    expires_at: i64,
    now_epoch: i64,
) -> Result<()> {
    let pane = envelope.tmux_pane.as_deref().unwrap_or_default();
    sqlx::query(
        r#"
        INSERT INTO pending_requests
        (request_id, session_id, session_name, tmux_pane, hook_event_name, state, created_at, expires_at)
        VALUES (?1, ?2, ?3, ?4, ?5, 'waiting', ?6, ?7)
        "#,
    )
    .bind(&envelope.request_id)
    .bind(&envelope.session_id)
    .bind(&envelope.session_name)
    .bind(pane)
    .bind(&envelope.hook_event_name)
    .bind(now_epoch)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_pending_message_id(
    pool: &SqlitePool,
    request_id: &str,
    message_id: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE pending_requests
        SET telegram_message_id = ?2
        WHERE request_id = ?1
        "#,
    )
    .bind(request_id)
    .bind(message_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn transition_pending_state(
    pool: &SqlitePool,
    request_id: &str,
    next_state: &str,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        UPDATE pending_requests
        SET state = ?2
        WHERE request_id = ?1 AND state = 'waiting'
        "#,
    )
    .bind(request_id)
    .bind(next_state)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn insert_reply_route(
    pool: &SqlitePool,
    telegram_message_id: i64,
    envelope: &HookEnvelope,
    now_epoch: i64,
) -> Result<()> {
    let Some(pane) = envelope.tmux_pane.as_deref() else {
        return Ok(());
    };
    sqlx::query(
        r#"
        INSERT OR REPLACE INTO reply_routes
        (telegram_message_id, session_id, tmux_pane, created_at)
        VALUES (?1, ?2, ?3, ?4)
        "#,
    )
    .bind(telegram_message_id)
    .bind(&envelope.session_id)
    .bind(pane)
    .bind(now_epoch)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn lookup_reply_route(
    pool: &SqlitePool,
    telegram_message_id: i64,
) -> Result<Option<ReplyRoute>> {
    let row = sqlx::query(
        r#"
        SELECT session_id, tmux_pane
        FROM reply_routes
        WHERE telegram_message_id = ?1
        "#,
    )
    .bind(telegram_message_id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(ReplyRoute {
            session_id: row.try_get::<String, _>("session_id")?,
            tmux_pane: row.try_get::<String, _>("tmux_pane")?,
        })),
        None => Ok(None),
    }
}

pub async fn find_session_by_name(pool: &SqlitePool, name: &str) -> Result<Option<DefaultRoute>> {
    let row = sqlx::query(
        r#"
        SELECT session_id, name, tmux_pane
        FROM sessions
        WHERE name = ?1
        ORDER BY last_seen_at DESC
        LIMIT 1
        "#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(DefaultRoute {
            session_id: row.try_get::<String, _>("session_id")?,
            session_name: row.try_get::<String, _>("name")?,
            tmux_pane: row.try_get::<String, _>("tmux_pane")?,
        })),
        None => Ok(None),
    }
}

pub async fn set_default_route(
    pool: &SqlitePool,
    route: &DefaultRoute,
    now_epoch: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO default_route (id, session_id, session_name, tmux_pane, updated_at)
        VALUES (1, ?1, ?2, ?3, ?4)
        ON CONFLICT(id) DO UPDATE SET
            session_id = excluded.session_id,
            session_name = excluded.session_name,
            tmux_pane = excluded.tmux_pane,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&route.session_id)
    .bind(&route.session_name)
    .bind(&route.tmux_pane)
    .bind(now_epoch)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_default_route(pool: &SqlitePool) -> Result<Option<DefaultRoute>> {
    let row = sqlx::query(
        r#"
        SELECT session_id, session_name, tmux_pane
        FROM default_route
        WHERE id = 1
        "#,
    )
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(DefaultRoute {
            session_id: row.try_get::<String, _>("session_id")?,
            session_name: row.try_get::<String, _>("session_name")?,
            tmux_pane: row.try_get::<String, _>("tmux_pane")?,
        })),
        None => Ok(None),
    }
}
