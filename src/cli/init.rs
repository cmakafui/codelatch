use std::{
    io::{self, Write},
    time::Duration,
};

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

    info!("init completed");
    print_init_summary();
    Ok(())
}

fn print_init_summary() {
    println!("Paired ✅");
    println!("Hooks installed ✅");
    println!("Daemon ready to start with `codelatch run` ✅");
    println!(
        "Config saved at {}",
        config::config_path()
            .map(|v| v.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string())
    );
}
