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

    println!("Config: {}", config::config_path()?.display());
    println!("Socket: {}", config.socket_path);
    println!("DB: {}", config.db_path);
    println!("PID: {}", config::pid_path()?.display());
    println!(
        "Hooks installed: {}",
        if plugin::hooks_installed()? {
            "yes"
        } else {
            "no"
        }
    );
    println!("Run `codelatch status` for live daemon/socket checks.");
    Ok(())
}
