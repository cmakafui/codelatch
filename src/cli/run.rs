use std::{env, process::Stdio, time::Duration};

use tokio::{net::UnixStream, process::Command, time::sleep};
use tracing::info;
use ulid::Ulid;

use super::RunArgs;
use crate::{
    config,
    errors::{AppError, Result},
};

pub async fn execute(args: RunArgs) -> Result<()> {
    let mut config = config::load().unwrap_or_default();
    if !config.is_configured() {
        println!("First run detected. Starting guided setup...");
        super::init::execute().await?;
        config = config::load()?;
        if !config.is_configured() {
            return Err(AppError::NotConfigured);
        }
    }

    ensure_tmux().await?;
    ensure_daemon_running(&config.socket_path).await?;

    let session_id = Ulid::new().to_string();
    let cwd = env::current_dir()?;
    let repo_name = cwd
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("project")
        .to_string();
    let suffix = session_id
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let session_name = format!("{repo_name}-{suffix}");
    let tmux_session = format!("codelatch:{session_name}:{session_id}");

    let new_session_status = Command::new("tmux")
        .args(["new-session", "-d", "-s", &tmux_session, "-c"])
        .arg(&cwd)
        .status()
        .await?;
    if !new_session_status.success() {
        return Err(AppError::TmuxFailed(
            "unable to create tmux session".to_string(),
        ));
    }

    let launch_command = format!(
        "CODELATCH_SESSION_ID={} CODELATCH_SESSION_NAME={} CODELATCH_SOCKET={} {}",
        shell_quote(&session_id),
        shell_quote(&session_name),
        shell_quote(&config.socket_path),
        build_claude_command(&args.claude_args)
    );

    let send_status = Command::new("tmux")
        .args(["send-keys", "-t", &tmux_session, &launch_command, "C-m"])
        .status()
        .await?;
    if !send_status.success() {
        return Err(AppError::TmuxFailed(
            "unable to inject Claude launch command".to_string(),
        ));
    }

    info!(session_id = %session_id, session_name = %session_name, "started managed session");
    println!("Started managed session: {session_name}");
    println!("tmux session: {tmux_session}");

    if !args.no_attach {
        let attach_status = Command::new("tmux")
            .args(["attach", "-t", &tmux_session])
            .status()
            .await?;
        if !attach_status.success() {
            return Err(AppError::TmuxFailed(
                "unable to attach to tmux session".to_string(),
            ));
        }
    }

    Ok(())
}

async fn ensure_tmux() -> Result<()> {
    let status = Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match status {
        Ok(code) if code.success() => Ok(()),
        _ => Err(AppError::TmuxMissing),
    }
}

async fn ensure_daemon_running(socket_path: &str) -> Result<()> {
    if UnixStream::connect(socket_path).await.is_ok() {
        return Ok(());
    }

    let current_exe = env::current_exe()?;
    let mut child = std::process::Command::new(current_exe);
    child
        .arg("start")
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    child.spawn()?;

    for _ in 0..50 {
        if UnixStream::connect(socket_path).await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(AppError::DaemonStartupTimeout)
}

fn build_claude_command(claude_args: &[String]) -> String {
    if claude_args.is_empty() {
        return "claude".to_string();
    }
    let mut out = String::from("claude");
    for arg in claude_args {
        out.push(' ');
        out.push_str(&shell_quote(arg));
    }
    out
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}
