use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{error, info, warn};

use crate::{
    config::{self, Config},
    db,
    errors::{AppError, Result},
    models::envelope::HookEnvelope,
};

const TELEGRAM_API: &str = "https://api.telegram.org";

#[derive(Clone)]
struct DaemonState {
    config: Config,
    db: SqlitePool,
    redactor: Arc<Redactor>,
    telegram: TelegramClient,
}

#[derive(Clone)]
struct TelegramClient {
    http: Client,
    token: String,
    chat_id: i64,
}

pub async fn run(config: Config) -> Result<()> {
    let token = config.token()?.to_string();
    let chat_id = config.chat_id()?;
    let pid_path = config::pid_path()?;
    if let Some(parent) = pid_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&pid_path, std::process::id().to_string()).await?;

    if let Some(parent) = Path::new(&config.socket_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if Path::new(&config.socket_path).exists() {
        let _ = tokio::fs::remove_file(&config.socket_path).await;
    }

    let db = db::connect(&config).await?;
    let listener = UnixListener::bind(&config.socket_path)?;
    let state = DaemonState {
        config,
        db,
        redactor: Arc::new(Redactor::new()?),
        telegram: TelegramClient {
            http: Client::new(),
            token: token.clone(),
            chat_id,
        },
    };

    info!(socket = %state.config.socket_path, "daemon listening");
    let long_poll = tokio::spawn(long_poll_loop(token));
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("daemon received shutdown signal");
                break;
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_client(stream, state).await {
                        error!(error = %err, "failed to handle hook client");
                    }
                });
            }
        }
    }

    long_poll.abort();
    let _ = tokio::fs::remove_file(&state.config.socket_path).await;
    let _ = tokio::fs::remove_file(pid_path).await;
    Ok(())
}

pub async fn get_bot_username(token: &str) -> Result<String> {
    let http = Client::new();
    let url = format!("{TELEGRAM_API}/bot{token}/getMe");
    let response: TelegramResponse<BotUser> = http.get(url).send().await?.json().await?;
    if !response.ok {
        return Err(AppError::TelegramAuthFailed);
    }
    let Some(result) = response.result else {
        return Err(AppError::TelegramAuthFailed);
    };
    Ok(result.username.unwrap_or_else(|| "unknown-bot".to_string()))
}

pub async fn wait_for_start_chat(token: &str, max_wait: Duration) -> Result<i64> {
    let http = Client::new();
    let mut offset: i64 = 0;
    let start = tokio::time::Instant::now();

    while start.elapsed() < max_wait {
        let url = format!("{TELEGRAM_API}/bot{token}/getUpdates");
        let payload = json!({
            "timeout": 20,
            "offset": offset,
            "allowed_updates": ["message"]
        });
        let response: TelegramResponse<Vec<TelegramUpdate>> =
            http.post(url).json(&payload).send().await?.json().await?;

        if !response.ok {
            return Err(AppError::TelegramApi(
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string()),
            ));
        }

        for update in response.result.unwrap_or_default() {
            offset = update.update_id + 1;
            if let Some(message) = update.message
                && message.text.as_deref() == Some("/start")
            {
                return Ok(message.chat.id);
            }
        }
    }

    Err(AppError::TelegramPairingTimeout)
}

async fn handle_client(stream: UnixStream, state: DaemonState) -> Result<()> {
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
    while let Some(frame) = framed.next().await {
        let bytes = frame?;
        let envelope: HookEnvelope = serde_json::from_slice(&bytes)?;
        process_event(&state, envelope).await?;
    }
    Ok(())
}

async fn process_event(state: &DaemonState, envelope: HookEnvelope) -> Result<()> {
    db::upsert_session(&state.db, &envelope, now_epoch()).await?;

    let payload_pretty = serde_json::to_string_pretty(&envelope.payload)?;
    let redacted_payload = state.redactor.redact(&payload_pretty);
    let tmux_context =
        capture_context(envelope.tmux_pane.as_deref(), state.config.context_lines).await;

    let mut body = format!(
        "{} {} · {}\n\n```json\n{}\n```",
        icon_for(&envelope.hook_event_name),
        envelope.hook_event_name,
        envelope.session_name,
        redacted_payload
    );
    if let Some(context) = tmux_context {
        body.push_str("\n\nContext:\n```text\n");
        body.push_str(&state.redactor.redact(&context));
        body.push_str("\n```");
    }

    if body.len() > state.config.max_inline_length {
        body.truncate(state.config.max_inline_length);
        body.push_str("\n\n...[truncated]");
    }

    if let Err(err) = state.telegram.send_message(&body).await {
        warn!(error = %err, "failed to send telegram message");
    }
    Ok(())
}

async fn capture_context(tmux_pane: Option<&str>, lines: usize) -> Option<String> {
    let pane = tmux_pane?;
    let start = format!("-{lines}");
    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", pane, "-S", &start])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn icon_for(hook_event: &str) -> &'static str {
    match hook_event {
        "Notification" => "🟡",
        "PostToolUseFailure" => "❌",
        "Stop" | "TaskCompleted" | "SessionEnd" => "✅",
        "SessionStart" => "🔵",
        _ => "🔵",
    }
}

async fn long_poll_loop(token: String) {
    let http = Client::new();
    let mut offset: i64 = 0;

    loop {
        let url = format!("{TELEGRAM_API}/bot{token}/getUpdates");
        let payload = json!({
            "timeout": 20,
            "offset": offset,
            "allowed_updates": ["message", "callback_query"]
        });
        let request = http.post(&url).json(&payload);

        match request.send().await {
            Ok(response) => {
                let parsed = response
                    .json::<TelegramResponse<Vec<TelegramUpdate>>>()
                    .await;
                match parsed {
                    Ok(value) if value.ok => {
                        for update in value.result.unwrap_or_default() {
                            offset = update.update_id + 1;
                        }
                    }
                    Ok(value) => {
                        warn!(
                            description = ?value.description,
                            "telegram getUpdates returned non-ok"
                        );
                    }
                    Err(err) => warn!(error = %err, "failed to parse telegram updates"),
                }
            }
            Err(err) => warn!(error = %err, "telegram long poll request failed"),
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

impl TelegramClient {
    async fn send_message(&self, text: &str) -> Result<()> {
        let url = format!("{TELEGRAM_API}/bot{}/sendMessage", self.token);
        let payload = json!({
            "chat_id": self.chat_id,
            "text": text
        });

        let response: TelegramResponse<Value> = self
            .http
            .post(url)
            .json(&payload)
            .send()
            .await?
            .json()
            .await?;

        if !response.ok {
            return Err(AppError::TelegramApi(
                response
                    .description
                    .unwrap_or_else(|| "unknown sendMessage failure".to_string()),
            ));
        }
        Ok(())
    }
}

struct Redactor {
    patterns: Vec<Regex>,
}

impl Redactor {
    fn new() -> Result<Self> {
        let patterns = vec![
            Regex::new(r"(?i)bearer\s+[A-Za-z0-9\-._~+/]+=*")
                .map_err(|err| AppError::TelegramApi(err.to_string()))?,
            Regex::new(r"gh[pousr]_[A-Za-z0-9]{20,}")
                .map_err(|err| AppError::TelegramApi(err.to_string()))?,
            Regex::new(r"sk-[A-Za-z0-9]{20,}")
                .map_err(|err| AppError::TelegramApi(err.to_string()))?,
            Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9._-]+\.[A-Za-z0-9._-]+")
                .map_err(|err| AppError::TelegramApi(err.to_string()))?,
        ];
        Ok(Self { patterns })
    }

    fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for pattern in &self.patterns {
            out = pattern.replace_all(&out, "[REDACTED]").to_string();
        }
        out
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BotUser {
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}
