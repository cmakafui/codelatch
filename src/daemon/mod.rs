use std::{
    collections::HashMap,
    fs::OpenOptions,
    future::Future,
    io::ErrorKind,
    num::NonZeroU32,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use backoff::{ExponentialBackoff, backoff::Backoff};
use bytes::Bytes;
use fs4::fs_std::FileExt;
use futures_util::{SinkExt, StreamExt};
use governor::{Quota, RateLimiter};
use regex::Regex;
use reqwest::{Client, multipart};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::{
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{Mutex, oneshot},
    time::sleep,
};
use tokio_util::{
    codec::{Framed, LengthDelimitedCodec},
    sync::CancellationToken,
};
use tracing::{error, info, warn};

use crate::{
    config::{self, Config},
    db,
    errors::{AppError, Result},
    models::envelope::{HookEnvelope, HookResponseEnvelope},
};

const TELEGRAM_API: &str = "https://api.telegram.org";
const PEEK_CONTEXT_LINES: usize = 30;
const LOG_LINES: usize = 200;
const MAX_TELEGRAM_TEXT: usize = 4096;

#[derive(Clone)]
struct DaemonState {
    config: Config,
    db: SqlitePool,
    redactor: Arc<Redactor>,
    telegram: TelegramClient,
    shutdown: CancellationToken,
    pending_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<HookResponseEnvelope>>>>,
}

#[derive(Clone)]
struct TelegramClient {
    http: Client,
    token: SecretString,
    chat_id: i64,
    limiter: Arc<governor::DefaultDirectRateLimiter>,
}

pub async fn run(config: Config) -> Result<()> {
    let _lock_guard = acquire_singleton_lock()?;
    let token: SecretString = config.token()?.to_string().into();
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
    let shutdown_token = CancellationToken::new();
    let state = DaemonState {
        config,
        db,
        redactor: Arc::new(Redactor::new()?),
        telegram: TelegramClient {
            http: Client::new(),
            token,
            chat_id,
            limiter: Arc::new(RateLimiter::direct(Quota::per_second(
                NonZeroU32::new(20).expect("nonzero"),
            ))),
        },
        shutdown: shutdown_token.clone(),
        pending_waiters: Arc::new(Mutex::new(HashMap::new())),
    };

    info!(socket = %state.config.socket_path, "daemon listening");
    let long_poll_state = state.clone();
    let long_poll = tokio::spawn(async move {
        if let Err(err) = long_poll_loop(long_poll_state).await {
            warn!(error = %err, "telegram long poll loop stopped");
        }
    });
    let mut shutdown_signal = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            _ = &mut shutdown_signal => {
                info!("daemon received shutdown signal");
                state.shutdown.cancel();
                break;
            }
            _ = state.shutdown.cancelled() => {
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

    state.shutdown.cancel();
    let _ = long_poll.await;
    let _ = tokio::fs::remove_file(&state.config.socket_path).await;
    let _ = tokio::fs::remove_file(pid_path).await;
    Ok(())
}

fn acquire_singleton_lock() -> Result<std::fs::File> {
    let lock_path = config::lock_path()?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    match file.try_lock_exclusive() {
        Ok(true) => Ok(file),
        Ok(false) => Err(AppError::DaemonAlreadyRunning),
        Err(err) if err.kind() == ErrorKind::WouldBlock => Err(AppError::DaemonAlreadyRunning),
        Err(err) => Err(AppError::DaemonLock(err.to_string())),
    }
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
        if envelope.blocking && envelope.hook_event_name == "PermissionRequest" {
            let response = process_permission_request(&state, &envelope).await?;
            framed
                .send(Bytes::from(serde_json::to_vec(&response)?))
                .await?;
            continue;
        }
        process_async_event(&state, &envelope).await?;
    }
    Ok(())
}

async fn process_permission_request(
    state: &DaemonState,
    envelope: &HookEnvelope,
) -> Result<HookResponseEnvelope> {
    let now = now_epoch();
    let expires_at = now + state.config.auto_deny_seconds as i64;
    db::upsert_session(&state.db, envelope, now).await?;
    db::insert_pending_request(&state.db, envelope, expires_at, now).await?;

    let command = extract_command(envelope);
    let redacted_command = state.redactor.redact(&command);
    let message_id = state
        .telegram
        .send_permission_message(
            &envelope.session_name,
            &redacted_command,
            &envelope.cwd,
            &envelope.request_id,
            state.config.auto_deny_seconds,
        )
        .await?;
    db::set_pending_message_id(&state.db, &envelope.request_id, message_id).await?;

    let (tx, rx) = oneshot::channel::<HookResponseEnvelope>();
    {
        let mut pending = state.pending_waiters.lock().await;
        pending.insert(envelope.request_id.clone(), tx);
    }

    let timeout_state = state.clone();
    let timeout_request_id = envelope.request_id.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = timeout_state.shutdown.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(timeout_state.config.auto_deny_seconds)) => {
                if let Ok(changed) =
                    db::transition_pending_state(&timeout_state.db, &timeout_request_id, "timed_out").await
                    && changed
                {
                    let _ = timeout_state
                        .telegram
                        .edit_message(message_id, "🔴 Permission\n\n⏳ Timed out — denied")
                        .await;
                    let hook_output = deny_permission_output("Denied by timeout");
                    complete_waiter(&timeout_state, &timeout_request_id, hook_output).await;
                }
            }
        }
    });

    match rx.await {
        Ok(response) => Ok(response),
        Err(_) => Ok(HookResponseEnvelope {
            request_id: envelope.request_id.clone(),
            hook_output: deny_permission_output("Denied because daemon waiter closed"),
        }),
    }
}

async fn process_async_event(state: &DaemonState, envelope: &HookEnvelope) -> Result<()> {
    db::upsert_session(&state.db, envelope, now_epoch()).await?;

    let payload_pretty = serde_json::to_string_pretty(&envelope.payload)?;
    let redacted_payload = state.redactor.redact(&payload_pretty);
    let tmux_context =
        capture_context(envelope.tmux_pane.as_deref(), state.config.context_lines).await;
    let redacted_context = tmux_context
        .as_ref()
        .map(|value| state.redactor.redact(value));
    let markdown = format_async_markdown(envelope, &redacted_payload, redacted_context.as_deref());

    let message_id = if markdown.chars().count() <= MAX_TELEGRAM_TEXT {
        state.telegram.send_markdown(&markdown).await?
    } else {
        let file_name = format!(
            "{}-{}-event.txt",
            safe_filename(&envelope.session_name),
            safe_filename(&envelope.hook_event_name.to_lowercase())
        );
        let mut text = format!(
            "{} {} · {}\n\n{}",
            icon_for(envelope),
            envelope.hook_event_name,
            envelope.session_name,
            redacted_payload
        );
        if let Some(context) = redacted_context {
            text.push_str("\n\nContext:\n");
            text.push_str(&context);
        }
        state
            .telegram
            .send_document(
                &file_name,
                text.into_bytes(),
                Some(&format!(
                    "*{}* · {}",
                    md_escape_text(&event_title(envelope)),
                    md_inline_code(&envelope.session_name)
                )),
            )
            .await?
    };
    if envelope.hook_event_name == "Notification" {
        db::insert_reply_route(&state.db, message_id, envelope, now_epoch()).await?;
    }

    Ok(())
}

async fn long_poll_loop(state: DaemonState) -> Result<()> {
    let mut offset: i64 = 0;
    loop {
        let updates = tokio::select! {
            _ = state.shutdown.cancelled() => return Ok(()),
            updates = state.telegram.get_updates(offset) => updates?,
        };
        for update in updates {
            offset = update.update_id + 1;

            if let Some(callback) = update.callback_query {
                if let Err(err) = handle_callback_query(&state, callback).await {
                    warn!(error = %err, "failed processing callback query");
                }
                continue;
            }
            if let Some(message) = update.message
                && let Err(err) = handle_message(&state, message).await
            {
                warn!(error = %err, "failed processing telegram message");
            }
        }
    }
}

async fn handle_message(state: &DaemonState, message: TelegramMessage) -> Result<()> {
    if message.chat.id != state.telegram.chat_id {
        return Ok(());
    }

    let Some(text) = message.text.clone() else {
        return Ok(());
    };

    if text.starts_with("/peek") {
        handle_peek_command(state, &message).await?;
        return Ok(());
    }

    if text.starts_with("/diff") {
        handle_diff_command(state, &message).await?;
        return Ok(());
    }

    if text.starts_with("/log") {
        handle_log_command(state, &message).await?;
        return Ok(());
    }

    if text.starts_with("/sessions") {
        let sessions = db::list_sessions(&state.db).await?;
        let default = db::get_default_route(&state.db).await?;
        if sessions.is_empty() {
            state.telegram.send_message("No active sessions.").await?;
        } else {
            let mut out = String::from("Active sessions:\n");
            for s in sessions {
                let marker = default
                    .as_ref()
                    .is_some_and(|route| route.session_id == s.session_id);
                let prefix = if marker { "* " } else { "- " };
                out.push_str(&format!("{prefix}{} ({})\n", s.name, s.session_id));
            }
            state.telegram.send_message(&out).await?;
        }
        return Ok(());
    }

    if text.starts_with("/switch") {
        let mut parts = text.split_whitespace();
        let _ = parts.next();
        let Some(name) = parts.next() else {
            let current = db::get_default_route(&state.db).await?;
            let msg = match current {
                Some(route) => format!("Current default session: {}", route.session_name),
                None => "No default session set. Use /switch <name>.".to_string(),
            };
            state.telegram.send_message(&msg).await?;
            return Ok(());
        };

        let Some(route) = db::find_session_by_name(&state.db, name).await? else {
            state
                .telegram
                .send_message("Session not found. Use /sessions to list active sessions.")
                .await?;
            return Ok(());
        };
        db::set_default_route(&state.db, &route, now_epoch()).await?;
        state
            .telegram
            .send_message(&format!(
                "Default session switched to {}.",
                route.session_name
            ))
            .await?;
        return Ok(());
    }

    let Some(reply_to) = message.reply_to_message else {
        if let Some(route) = db::get_default_route(&state.db).await? {
            if inject_reply(&route.tmux_pane, &text).await {
                state
                    .telegram
                    .send_message(&format!(
                        "Sent message to default session {}.",
                        route.session_name
                    ))
                    .await?;
            } else {
                state
                    .telegram
                    .send_message("Failed to inject message into default session.")
                    .await?;
            }
            return Ok(());
        }
        state
            .telegram
            .send_message("Reply to a session message, or use /switch <name> first.")
            .await?;
        return Ok(());
    };
    let Some(route) = db::lookup_reply_route(&state.db, reply_to.message_id).await? else {
        return Ok(());
    };

    if inject_reply(&route.tmux_pane, &text).await {
        state
            .telegram
            .send_message(&format!("Sent reply to session {}.", route.session_id))
            .await?;
    } else {
        state
            .telegram
            .send_message("Failed to inject reply into tmux session.")
            .await?;
    }
    Ok(())
}

async fn handle_peek_command(state: &DaemonState, message: &TelegramMessage) -> Result<()> {
    let Some(session) = resolve_session_for_message(state, message).await? else {
        state
            .telegram
            .send_message("No active session. Use /sessions to pick one.")
            .await?;
        return Ok(());
    };

    let recent_output = capture_context(Some(&session.tmux_pane), PEEK_CONTEXT_LINES)
        .await
        .unwrap_or_else(|| "No tmux output available".to_string());
    let redacted_output = state.redactor.redact(&recent_output);
    let running_command = detect_running_command(&session.tmux_pane)
        .await
        .unwrap_or_else(|| "idle".to_string());
    let current_file = detect_current_file(&running_command, &redacted_output)
        .unwrap_or_else(|| "unknown".to_string());
    let current_task =
        latest_nonempty_line(&redacted_output).unwrap_or_else(|| "unknown".to_string());
    let mut preview_output = redacted_output;
    let mut body = format!(
        "*🔵 Peek* · {}\n\n*Session* {}\n*Dir* {}\n*Task* {}\n*Running* {}\n*Current file* {}\n\n*Recent output*\n{}",
        md_inline_code(&session.name),
        md_inline_code(&session.session_id),
        md_inline_code(&session.cwd),
        md_inline_code(&current_task),
        md_inline_code(&running_command),
        md_inline_code(&current_file),
        md_code_block("", &preview_output)
    );
    if body.chars().count() > MAX_TELEGRAM_TEXT {
        preview_output = truncate_tail(&preview_output, 1800);
        body = format!(
            "*🔵 Peek* · {}\n\n*Session* {}\n*Dir* {}\n*Task* {}\n*Running* {}\n*Current file* {}\n\n*Recent output*\n{}\n\nTruncated for Telegram",
            md_inline_code(&session.name),
            md_inline_code(&session.session_id),
            md_inline_code(&session.cwd),
            md_inline_code(&current_task),
            md_inline_code(&running_command),
            md_inline_code(&current_file),
            md_code_block("", &preview_output)
        );
    }
    let keyboard = json!({
        "inline_keyboard": [[
            {"text":"Diff", "callback_data": format!("peek:diff:{}", session.session_id)},
            {"text":"Log", "callback_data": format!("peek:log:{}", session.session_id)},
            {"text":"Stop", "callback_data": format!("peek:stop:{}", session.session_id)}
        ]]
    });
    state
        .telegram
        .send_markdown_with_markup(&body, Some(keyboard))
        .await?;
    Ok(())
}

async fn handle_diff_command(state: &DaemonState, message: &TelegramMessage) -> Result<()> {
    let Some(session) = resolve_session_for_message(state, message).await? else {
        state
            .telegram
            .send_message("No active session. Use /sessions to pick one.")
            .await?;
        return Ok(());
    };
    send_diff_for_session(state, &session).await
}

async fn handle_log_command(state: &DaemonState, message: &TelegramMessage) -> Result<()> {
    let Some(session) = resolve_session_for_message(state, message).await? else {
        state
            .telegram
            .send_message("No active session. Use /sessions to pick one.")
            .await?;
        return Ok(());
    };
    send_log_for_session(state, &session).await
}

async fn send_diff_for_session(state: &DaemonState, session: &db::SessionRecord) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", &session.cwd, "diff", "--no-color"])
        .output()
        .await?;
    if !output.status.success() {
        let err = normalize_terminal_text(String::from_utf8_lossy(&output.stderr).as_ref());
        let msg = format!(
            "*❌ Diff failed* · {}\n\n{}",
            md_inline_code(&session.name),
            md_code_block("", &state.redactor.redact(err.trim()))
        );
        state.telegram.send_markdown(&msg).await?;
        return Ok(());
    }

    let diff_stdout = normalize_terminal_text(String::from_utf8_lossy(&output.stdout).as_ref());
    let diff = state.redactor.redact(&diff_stdout);
    if diff.trim().is_empty() {
        state
            .telegram
            .send_markdown(&format!(
                "*✅ Diff* · {}\n\nNo changes",
                md_inline_code(&session.name)
            ))
            .await?;
        return Ok(());
    }

    let inline = format!(
        "*🔵 Diff* · {}\n\n{}",
        md_inline_code(&session.name),
        md_code_block("diff", &diff)
    );
    if inline.chars().count() <= MAX_TELEGRAM_TEXT {
        state.telegram.send_markdown(&inline).await?;
        return Ok(());
    }

    let filename = format!("{}-diff.patch", safe_filename(&session.name));
    let caption = format!("*🔵 Diff* · {}", md_inline_code(&session.name));
    state
        .telegram
        .send_document(&filename, diff.into_bytes(), Some(&caption))
        .await?;
    Ok(())
}

async fn send_log_for_session(state: &DaemonState, session: &db::SessionRecord) -> Result<()> {
    let log = capture_context(Some(&session.tmux_pane), LOG_LINES)
        .await
        .unwrap_or_else(|| "No tmux log available".to_string());
    let redacted_log = state.redactor.redact(&log);
    let filename = format!("{}-log.txt", safe_filename(&session.name));
    let caption = format!("*🔵 Log* · {}", md_inline_code(&session.name));
    state
        .telegram
        .send_document(&filename, redacted_log.into_bytes(), Some(&caption))
        .await?;
    Ok(())
}

async fn resolve_session_for_message(
    state: &DaemonState,
    message: &TelegramMessage,
) -> Result<Option<db::SessionRecord>> {
    if let Some(reply_to) = message.reply_to_message.as_ref()
        && let Some(route) = db::lookup_reply_route(&state.db, reply_to.message_id).await?
        && let Some(session) = db::get_session(&state.db, &route.session_id).await?
    {
        return Ok(Some(session));
    }

    if let Some(default_route) = db::get_default_route(&state.db).await?
        && let Some(session) = db::get_session(&state.db, &default_route.session_id).await?
    {
        return Ok(Some(session));
    }

    let sessions = db::list_sessions(&state.db).await?;
    Ok(sessions.into_iter().next())
}

async fn handle_callback_query(state: &DaemonState, callback: TelegramCallbackQuery) -> Result<()> {
    state.telegram.answer_callback_query(&callback.id).await?;
    if callback.message.as_ref().map(|m| m.chat.id) != Some(state.telegram.chat_id) {
        return Ok(());
    }

    let Some(data) = callback.data.as_deref() else {
        return Ok(());
    };
    let mut parts = data.splitn(3, ':');
    let kind = parts.next().unwrap_or_default();
    match kind {
        "permit" => {
            let request_id = parts.next().unwrap_or_default();
            let action = parts.next().unwrap_or_default();
            if request_id.is_empty() {
                return Ok(());
            }

            let (next_state, status_text, hook_output) = match action {
                "allow" => ("approved", "✅ Approved", allow_permission_output()),
                "deny" => (
                    "denied",
                    "❌ Denied",
                    deny_permission_output("Denied by remote operator"),
                ),
                _ => return Ok(()),
            };

            let changed = db::transition_pending_state(&state.db, request_id, next_state).await?;
            if changed {
                if let Some(message) = callback.message {
                    let _ = state
                        .telegram
                        .edit_message(
                            message.message_id,
                            &format!("🔴 Permission\n\n{status_text}"),
                        )
                        .await;
                }
                complete_waiter(state, request_id, hook_output).await;
            }
        }
        "peek" => {
            let action = parts.next().unwrap_or_default();
            let session_id = parts.next().unwrap_or_default();
            if session_id.is_empty() {
                return Ok(());
            }
            handle_peek_callback_action(state, action, session_id).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_peek_callback_action(
    state: &DaemonState,
    action: &str,
    session_id: &str,
) -> Result<()> {
    let Some(session) = db::get_session(&state.db, session_id).await? else {
        state
            .telegram
            .send_message("Session is no longer active.")
            .await?;
        return Ok(());
    };

    match action {
        "diff" => send_diff_for_session(state, &session).await?,
        "log" => send_log_for_session(state, &session).await?,
        "stop" => {
            if send_interrupt(&session.tmux_pane).await {
                let text = format!(
                    "*⏹ Stop sent* · {}\n\nSent Ctrl\\+C to {}",
                    md_inline_code(&session.name),
                    md_inline_code(&session.tmux_pane)
                );
                state.telegram.send_markdown(&text).await?;
            } else {
                state
                    .telegram
                    .send_message("Failed to send interrupt to tmux pane.")
                    .await?;
            }
        }
        _ => {}
    }

    Ok(())
}

async fn complete_waiter(state: &DaemonState, request_id: &str, hook_output: Value) {
    let mut waiters = state.pending_waiters.lock().await;
    if let Some(sender) = waiters.remove(request_id) {
        let _ = sender.send(HookResponseEnvelope {
            request_id: request_id.to_string(),
            hook_output,
        });
    }
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
    Some(normalize_terminal_text(
        String::from_utf8_lossy(&output.stdout).as_ref(),
    ))
}

async fn send_interrupt(tmux_pane: &str) -> bool {
    Command::new("tmux")
        .args(["send-keys", "-t", tmux_pane, "C-c"])
        .status()
        .await
        .is_ok_and(|status| status.success())
}

async fn detect_running_command(tmux_pane: &str) -> Option<String> {
    let pane_pid = tmux_display_value(tmux_pane, "#{pane_pid}")
        .await?
        .trim()
        .parse::<i32>()
        .ok()?;
    let pane_current_command = tmux_display_value(tmux_pane, "#{pane_current_command}").await?;
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return Some(pane_current_command.trim().to_string());
    }

    let mut parent_to_children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut commands: HashMap<i32, String> = HashMap::new();
    let text = normalize_terminal_text(String::from_utf8_lossy(&output.stdout).as_ref());
    for line in text.lines() {
        let mut cols = line.split_whitespace();
        let Some(pid_raw) = cols.next() else {
            continue;
        };
        let Some(ppid_raw) = cols.next() else {
            continue;
        };
        let command_raw = cols.collect::<Vec<_>>().join(" ");
        if command_raw.is_empty() {
            continue;
        }
        let Ok(pid) = pid_raw.parse::<i32>() else {
            continue;
        };
        let Ok(ppid) = ppid_raw.parse::<i32>() else {
            continue;
        };
        parent_to_children.entry(ppid).or_default().push(pid);
        commands.insert(pid, command_raw);
    }

    let mut current_pid = pane_pid;
    let mut best_command = commands
        .get(&pane_pid)
        .cloned()
        .unwrap_or_else(|| pane_current_command.trim().to_string());

    for _ in 0..25 {
        let Some(children) = parent_to_children.get(&current_pid) else {
            break;
        };
        let child_pid = children.iter().max().copied()?;
        current_pid = child_pid;
        if let Some(command) = commands.get(&child_pid)
            && !looks_like_shell(command)
        {
            best_command = command.clone();
        }
    }

    let normalized = best_command.trim().to_string();
    if normalized.is_empty() {
        return Some("idle".to_string());
    }
    if looks_like_shell(&normalized) {
        return Some("idle".to_string());
    }
    Some(normalized)
}

async fn tmux_display_value(tmux_pane: &str, format_expr: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", tmux_pane, format_expr])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn looks_like_shell(command: &str) -> bool {
    let base = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_start_matches('-')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    matches!(base, "bash" | "zsh" | "sh" | "fish" | "tmux" | "login")
}

fn detect_current_file(running_command: &str, context: &str) -> Option<String> {
    if let Some(path) = first_path_token(running_command) {
        return Some(path);
    }
    for line in context.lines().rev() {
        if let Some(path) = first_path_token(line) {
            return Some(path);
        }
    }
    None
}

fn first_path_token(input: &str) -> Option<String> {
    for token in input.split_whitespace().rev() {
        let cleaned = token.trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | '[' | ']' | '(' | ')' | '{' | '}' | ',' | ';' | ':'
                )
        });
        if cleaned.contains("://") || cleaned.starts_with('-') {
            continue;
        }
        if !cleaned.contains('.') || cleaned.ends_with('.') {
            continue;
        }
        if cleaned.contains('/') || cleaned.contains('.') {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn latest_nonempty_line(input: &str) -> Option<String> {
    for line in input.lines().rev() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn truncate_tail(input: &str, max_chars: usize) -> String {
    let chars: Vec<char> = input.chars().collect();
    if chars.len() <= max_chars {
        return input.to_string();
    }
    chars[chars.len().saturating_sub(max_chars)..]
        .iter()
        .collect::<String>()
}

async fn inject_reply(tmux_pane: &str, text: &str) -> bool {
    let sanitized = text.replace('\n', " ");
    let literal = Command::new("tmux")
        .args(["send-keys", "-t", tmux_pane, "-l", &sanitized])
        .status()
        .await;
    if !literal.is_ok_and(|status| status.success()) {
        return false;
    }
    Command::new("tmux")
        .args(["send-keys", "-t", tmux_pane, "C-m"])
        .status()
        .await
        .is_ok_and(|status| status.success())
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn extract_command(envelope: &HookEnvelope) -> String {
    envelope
        .payload
        .get("tool_input")
        .and_then(|tool| tool.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("<unknown command>")
        .to_string()
}

fn icon_for(envelope: &HookEnvelope) -> &'static str {
    if envelope.hook_event_name == "Notification" {
        let notification_type = envelope
            .payload
            .get("notification_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match notification_type {
            "elicitation_dialog" => "🟡",
            "permission_prompt" => "🔴",
            _ => "🔵",
        };
    }

    match envelope.hook_event_name.as_str() {
        "PostToolUseFailure" => "❌",
        "Stop" | "TaskCompleted" | "SessionEnd" => "✅",
        "SessionStart" => "🔵",
        _ => "🔵",
    }
}

fn allow_permission_output() -> Value {
    json!({
      "hookSpecificOutput": {
        "hookEventName": "PermissionRequest",
        "decision": { "behavior": "allow" }
      }
    })
}

fn deny_permission_output(message: &str) -> Value {
    json!({
      "hookSpecificOutput": {
        "hookEventName": "PermissionRequest",
        "decision": {
          "behavior": "deny",
          "message": message
        }
      }
    })
}

fn format_async_markdown(
    envelope: &HookEnvelope,
    redacted_payload: &str,
    redacted_context: Option<&str>,
) -> String {
    let mut out = format!(
        "*{}* · {}",
        md_escape_text(&event_title(envelope)),
        md_inline_code(&envelope.session_name)
    );

    match envelope.hook_event_name.as_str() {
        "SessionStart" => {
            out.push_str("\n\n*Dir* ");
            out.push_str(&md_inline_code(&envelope.cwd));
            out.push_str("\n\nNew session latched");
        }
        "SessionEnd" => {
            out.push_str("\n\nSession ended");
        }
        "Stop" | "TaskCompleted" => {
            out.push_str("\n\nTask finished");
        }
        _ => {
            out.push_str("\n\n*Payload*\n");
            out.push_str(&md_code_block("json", redacted_payload));
            if let Some(context) = redacted_context {
                out.push_str("\n\n*Context*\n");
                out.push_str(&md_code_block("", context));
            }
            if envelope.hook_event_name == "Notification" {
                out.push_str("\n\nReply to this message");
            }
        }
    }

    out
}

fn event_title(envelope: &HookEnvelope) -> String {
    if envelope.hook_event_name == "Notification" {
        let notification_type = envelope
            .payload
            .get("notification_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match notification_type {
            "elicitation_dialog" => "🟡 Question".to_string(),
            "permission_prompt" => "🔴 Permission Prompt".to_string(),
            "idle_prompt" => "🔵 Idle Prompt".to_string(),
            _ => "🔵 Notification".to_string(),
        };
    }

    match envelope.hook_event_name.as_str() {
        "PostToolUseFailure" => "❌ Tool Failure".to_string(),
        "Stop" | "TaskCompleted" => "✅ Done".to_string(),
        "SessionStart" => "🔵 Session Start".to_string(),
        "SessionEnd" => "🔵 Session End".to_string(),
        _ => format!("{} {}", icon_for(envelope), envelope.hook_event_name),
    }
}

fn md_escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(
            ch,
            '\\' | '_'
                | '*'
                | '['
                | ']'
                | '('
                | ')'
                | '~'
                | '`'
                | '>'
                | '#'
                | '+'
                | '-'
                | '='
                | '|'
                | '{'
                | '}'
                | '.'
                | '!'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn md_inline_code(input: &str) -> String {
    format!("`{}`", md_escape_code(input))
}

fn md_code_block(language: &str, input: &str) -> String {
    format!("```{language}\n{}\n```", md_escape_code(input))
}

fn md_escape_code(input: &str) -> String {
    input.replace('\\', "\\\\").replace('`', "\\`")
}

fn safe_filename(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return "codelatch".to_string();
    }
    out
}

fn normalize_terminal_text(input: &str) -> String {
    const TAB_MARKER: &str = "__CODELATCH_TAB__";
    const CR_MARKER: &str = "__CODELATCH_CR__";

    let preserved = input.replace('\t', TAB_MARKER).replace('\r', CR_MARKER);
    let stripped = strip_ansi_escapes::strip(preserved.as_bytes());
    String::from_utf8_lossy(&stripped)
        .replace(TAB_MARKER, "\t")
        .replace(CR_MARKER, "\r")
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        .collect()
}

impl TelegramClient {
    async fn send_message(&self, text: &str) -> Result<i64> {
        let token = self.token.expose_secret();
        let url = format!("{TELEGRAM_API}/bot{token}/sendMessage");
        let payload = json!({ "chat_id": self.chat_id, "text": text });
        let client = self.http.clone();

        self.with_retry(|| {
            let payload = payload.clone();
            let url = url.clone();
            let client = client.clone();
            async move {
                let response: TelegramResponse<TelegramSentMessage> = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await?
                    .json()
                    .await?;
                if !response.ok {
                    return Err(AppError::TelegramApi(
                        response
                            .description
                            .unwrap_or_else(|| "sendMessage failed".to_string()),
                    ));
                }
                let Some(message) = response.result else {
                    return Err(AppError::TelegramApi(
                        "sendMessage missing result".to_string(),
                    ));
                };
                Ok(message.message_id)
            }
        })
        .await
    }

    async fn send_markdown(&self, text: &str) -> Result<i64> {
        self.send_markdown_with_markup(text, None).await
    }

    async fn send_markdown_with_markup(
        &self,
        text: &str,
        reply_markup: Option<Value>,
    ) -> Result<i64> {
        let token = self.token.expose_secret();
        let url = format!("{TELEGRAM_API}/bot{token}/sendMessage");
        let mut payload = json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "MarkdownV2"
        });
        if let Some(markup) = reply_markup {
            payload["reply_markup"] = markup;
        }
        let client = self.http.clone();

        self.with_retry(|| {
            let payload = payload.clone();
            let url = url.clone();
            let client = client.clone();
            async move {
                let response: TelegramResponse<TelegramSentMessage> = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await?
                    .json()
                    .await?;
                if !response.ok {
                    return Err(AppError::TelegramApi(
                        response
                            .description
                            .unwrap_or_else(|| "sendMessage markdown failed".to_string()),
                    ));
                }
                let Some(message) = response.result else {
                    return Err(AppError::TelegramApi(
                        "sendMessage markdown missing result".to_string(),
                    ));
                };
                Ok(message.message_id)
            }
        })
        .await
    }

    async fn send_document(
        &self,
        file_name: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> Result<i64> {
        let token = self.token.expose_secret();
        let url = format!("{TELEGRAM_API}/bot{token}/sendDocument");
        let file_name = file_name.to_string();
        let caption = caption.map(str::to_string);
        let chat_id = self.chat_id.to_string();
        let document = bytes;
        let client = self.http.clone();

        self.with_retry(|| {
            let url = url.clone();
            let file_name = file_name.clone();
            let caption = caption.clone();
            let chat_id = chat_id.clone();
            let data = document.clone();
            let client = client.clone();
            async move {
                let part = multipart::Part::bytes(data)
                    .file_name(file_name)
                    .mime_str("text/plain; charset=utf-8")
                    .map_err(|err| AppError::TelegramApi(err.to_string()))?;
                let mut form = multipart::Form::new()
                    .text("chat_id", chat_id)
                    .part("document", part);
                if let Some(caption) = caption {
                    form = form
                        .text("caption", caption)
                        .text("parse_mode", "MarkdownV2".to_string());
                }

                let response: TelegramResponse<TelegramSentMessage> = client
                    .post(&url)
                    .multipart(form)
                    .send()
                    .await?
                    .json()
                    .await?;
                if !response.ok {
                    return Err(AppError::TelegramApi(
                        response
                            .description
                            .unwrap_or_else(|| "sendDocument failed".to_string()),
                    ));
                }
                let Some(message) = response.result else {
                    return Err(AppError::TelegramApi(
                        "sendDocument missing result".to_string(),
                    ));
                };
                Ok(message.message_id)
            }
        })
        .await
    }

    async fn send_permission_message(
        &self,
        session_name: &str,
        command: &str,
        cwd: &str,
        request_id: &str,
        timeout_seconds: u64,
    ) -> Result<i64> {
        let minutes = timeout_seconds / 60;
        let seconds = timeout_seconds % 60;
        let text = format!(
            "*🔴 Permission* · {}\n\n*Claude wants to run*\n{}\n\n*Dir* {}\n\nAuto deny in {:02}:{:02}",
            md_inline_code(session_name),
            md_code_block("bash", command),
            md_inline_code(cwd),
            minutes,
            seconds
        );

        let reply_markup = json!({
          "inline_keyboard": [[
            {"text":"Allow", "callback_data": format!("permit:{request_id}:allow")},
            {"text":"Deny", "callback_data": format!("permit:{request_id}:deny")}
          ]]
        });

        self.send_markdown_with_markup(&text, Some(reply_markup))
            .await
    }

    async fn edit_message(&self, message_id: i64, text: &str) -> Result<()> {
        let token = self.token.expose_secret();
        let url = format!("{TELEGRAM_API}/bot{token}/editMessageText");
        let payload = json!({
            "chat_id": self.chat_id,
            "message_id": message_id,
            "text": text
        });
        let client = self.http.clone();

        self.with_retry(|| {
            let payload = payload.clone();
            let url = url.clone();
            let client = client.clone();
            async move {
                let response: TelegramResponse<Value> = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await?
                    .json()
                    .await?;
                if !response.ok {
                    return Err(AppError::TelegramApi(
                        response
                            .description
                            .unwrap_or_else(|| "editMessageText failed".to_string()),
                    ));
                }
                Ok(())
            }
        })
        .await
    }

    async fn answer_callback_query(&self, callback_query_id: &str) -> Result<()> {
        let token = self.token.expose_secret();
        let url = format!("{TELEGRAM_API}/bot{token}/answerCallbackQuery");
        let payload = json!({ "callback_query_id": callback_query_id });
        let client = self.http.clone();

        self.with_retry(|| {
            let payload = payload.clone();
            let url = url.clone();
            let client = client.clone();
            async move {
                let response: TelegramResponse<Value> = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await?
                    .json()
                    .await?;
                if !response.ok {
                    return Err(AppError::TelegramApi(
                        response
                            .description
                            .unwrap_or_else(|| "answerCallbackQuery failed".to_string()),
                    ));
                }
                Ok(())
            }
        })
        .await
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<TelegramUpdate>> {
        let token = self.token.expose_secret();
        let url = format!("{TELEGRAM_API}/bot{token}/getUpdates");
        let payload = json!({
            "timeout": 20,
            "offset": offset,
            "allowed_updates": ["message", "callback_query"]
        });
        let client = self.http.clone();

        self.with_retry(|| {
            let payload = payload.clone();
            let url = url.clone();
            let client = client.clone();
            async move {
                let response: TelegramResponse<Vec<TelegramUpdate>> = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await?
                    .json()
                    .await?;
                if !response.ok {
                    return Err(AppError::TelegramApi(
                        response
                            .description
                            .unwrap_or_else(|| "getUpdates failed".to_string()),
                    ));
                }
                Ok(response.result.unwrap_or_default())
            }
        })
        .await
    }

    async fn with_retry<T, F, Fut>(&self, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut backoff = ExponentialBackoff {
            initial_interval: Duration::from_millis(250),
            max_interval: Duration::from_secs(4),
            max_elapsed_time: Some(Duration::from_secs(20)),
            ..ExponentialBackoff::default()
        };
        backoff.reset();

        let mut attempts: u32 = 0;
        loop {
            self.limiter.until_ready().await;
            match op().await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    attempts += 1;
                    if !is_retryable_telegram_error(&err) {
                        return Err(err);
                    }
                    let Some(delay) = backoff.next_backoff() else {
                        return Err(err);
                    };
                    warn!(
                        attempt = attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "retrying telegram request"
                    );
                    sleep(delay).await;
                }
            }
        }
    }
}

fn is_retryable_telegram_error(err: &AppError) -> bool {
    match err {
        AppError::Http(http) => {
            http.is_timeout()
                || http.is_connect()
                || http.is_request()
                || http
                    .status()
                    .is_some_and(|status| status.is_server_error() || status.as_u16() == 429)
        }
        AppError::TelegramApi(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("too many requests")
                || lower.contains("retry after")
                || lower.contains("timed out")
                || lower.contains("bad gateway")
                || lower.contains("gateway timeout")
                || lower.contains("internal server error")
        }
        _ => false,
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
            Regex::new(r"AKIA[0-9A-Z]{16}")
                .map_err(|err| AppError::TelegramApi(err.to_string()))?,
            Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9._-]+\.[A-Za-z0-9._-]+")
                .map_err(|err| AppError::TelegramApi(err.to_string()))?,
            Regex::new(
                r"(?s)-----BEGIN [A-Z ]+PRIVATE KEY-----.*?-----END [A-Z ]+PRIVATE KEY-----",
            )
            .map_err(|err| AppError::TelegramApi(err.to_string()))?,
            Regex::new(r"(?im)^\s*[A-Z0-9_]*(TOKEN|SECRET|PASSWORD|API_KEY)[A-Z0-9_]*\s*=\s*.+$")
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
struct TelegramSentMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    chat: TelegramChat,
    text: Option<String>,
    reply_to_message: Option<TelegramReplyMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramReplyMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    data: Option<String>,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[cfg(test)]
mod tests {
    use super::normalize_terminal_text;

    #[test]
    fn strips_color_ansi_sequences() {
        let input = "\u{1b}[31merror\u{1b}[0m";
        assert_eq!(normalize_terminal_text(input), "error");
    }

    #[test]
    fn strips_cursor_and_erase_sequences() {
        let input = "line\u{1b}[2K\u{1b}[1Aafter";
        assert_eq!(normalize_terminal_text(input), "lineafter");
    }

    #[test]
    fn preserves_newlines_tabs_and_cr() {
        let input = "a\tb\nc\rd";
        assert_eq!(normalize_terminal_text(input), "a\tb\nc\rd");
    }

    #[test]
    fn removes_other_control_characters() {
        let input = format!("a{}b{}c", '\u{7}', '\u{0}');
        assert_eq!(normalize_terminal_text(&input), "abc");
    }
}
