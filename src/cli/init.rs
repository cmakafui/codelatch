use std::{
    io::{self, Write},
    process::Stdio,
    time::Duration,
};

use tokio::{net::UnixStream, time::sleep};
use tracing::info;

use crate::{config, daemon, errors::Result, plugin};

pub async fn execute() -> Result<()> {
    let mut config = config::load().unwrap_or_default();

    print!("Telegram bot token (from BotFather): ");
    io::stdout().flush()?;
    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    let username = daemon::get_bot_username(&token).await?;
    println!("Bot verified: @{username}");
    println!("Send /start to @{username} now. Waiting up to 120 seconds...");

    let chat_id = daemon::wait_for_start_chat(&token, Duration::from_secs(120)).await?;
    println!("Paired chat_id: {chat_id}");

    config.telegram_bot_token = Some(token);
    config.telegram_chat_id = Some(chat_id);
    config::save(&config)?;

    let binary_path = std::env::current_exe()?;
    plugin::install_hooks(&binary_path)?;
    plugin::write_plugin_artifacts(&binary_path)?;
    let daemon_ready = ensure_daemon_running(&config.socket_path).await;

    info!("init completed");
    print_init_summary(daemon_ready.is_ok());
    Ok(())
}

fn print_init_summary(daemon_ready: bool) {
    println!("Paired ✅");
    println!("Hooks installed ✅");
    if daemon_ready {
        println!("Daemon running ✅");
    } else {
        println!("Daemon not running yet (run `codelatch start`) ⚠️");
    }
    println!(
        "Config saved at {}",
        config::config_path()
            .map(|v| v.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string())
    );
}

async fn ensure_daemon_running(socket_path: &str) -> Result<()> {
    if UnixStream::connect(socket_path).await.is_ok() {
        return Ok(());
    }
    let current_exe = std::env::current_exe()?;
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
    Ok(())
}
