use std::{fs, path::Path, process::Stdio};

use tokio::{net::UnixStream, process::Command};

use crate::{
    config,
    errors::{AppError, Result},
    plugin,
};

pub async fn execute() -> Result<()> {
    let config = config::load()?;
    if !config.is_configured() {
        return Err(AppError::NotConfigured);
    }

    let mut ready = true;
    println!("Status:");

    let hooks_ok = plugin::hooks_installed()?;
    if hooks_ok {
        println!("✅ Hooks installed");
    } else {
        ready = false;
        println!("⚠️ Hooks not installed");
    }

    if UnixStream::connect(&config.socket_path).await.is_ok() {
        println!("✅ Daemon socket reachable");
    } else {
        ready = false;
        println!("⚠️ Daemon socket unreachable ({})", config.socket_path);
    }

    let pid_path = config::pid_path()?;
    if Path::new(&pid_path).exists() {
        let pid_text = fs::read_to_string(&pid_path).unwrap_or_else(|_| "<unknown>".to_string());
        println!("✅ PID file present ({})", pid_text.trim());
    } else {
        ready = false;
        println!("⚠️ PID file missing ({})", pid_path.display());
    }

    let tmux_ok = Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if tmux_ok {
        println!("✅ tmux available");
    } else {
        ready = false;
        println!("⚠️ tmux not available");
    }

    match crate::daemon::get_bot_username(config.token()?).await {
        Ok(username) => println!("✅ Telegram auth ok (@{username})"),
        Err(_) => {
            ready = false;
            println!("⚠️ Telegram auth failed");
        }
    }

    if ready {
        println!("✅ Ready");
    } else {
        println!("⚠️ Not ready (run `codelatch doctor --fix`)");
    }

    Ok(())
}
