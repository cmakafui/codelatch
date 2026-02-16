use std::{path::Path, process::Stdio, time::Duration};

use tokio::{net::UnixStream, time::sleep};
use tracing::info;

use super::StartArgs;
use crate::{
    config, daemon,
    errors::{AppError, Result},
};

pub async fn execute(args: StartArgs) -> Result<()> {
    let config = config::load()?;
    if !config.is_configured() {
        return Err(AppError::NotConfigured);
    }

    if args.foreground {
        info!("starting codelatch daemon (foreground)");
        return daemon::run(config).await;
    }
    if args.background {
        info!("starting codelatch daemon (background)");
    }

    if UnixStream::connect(&config.socket_path).await.is_ok() {
        println!("Daemon already running.");
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("start")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..50 {
        if UnixStream::connect(&config.socket_path).await.is_ok() {
            println!("Daemon started.");
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }

    if Path::new(&config.socket_path).exists() {
        return Err(AppError::DaemonUnavailable);
    }
    Err(AppError::DaemonStartupTimeout)
}
