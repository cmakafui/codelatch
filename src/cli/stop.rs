use std::{fs, path::Path, time::Duration};

use tokio::{net::UnixStream, process::Command, time::sleep};

use crate::{
    config,
    errors::{AppError, Result},
};

pub async fn execute() -> Result<()> {
    let config = config::load().unwrap_or_default();
    let pid_path = config::pid_path()?;

    let pid = read_pid(&pid_path);
    if let Some(pid_value) = pid {
        let status = Command::new("kill")
            .args(["-INT", &pid_value.to_string()])
            .status()
            .await?;
        if !status.success() {
            let _ = Command::new("kill")
                .args(["-TERM", &pid_value.to_string()])
                .status()
                .await;
        }
    }

    for _ in 0..50 {
        if UnixStream::connect(&config.socket_path).await.is_err() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    if Path::new(&config.socket_path).exists() {
        let _ = fs::remove_file(&config.socket_path);
    }
    if pid_path.exists() {
        let _ = fs::remove_file(&pid_path);
    }

    if UnixStream::connect(&config.socket_path).await.is_ok() {
        return Err(AppError::DaemonUnavailable);
    }

    println!("Daemon stopped.");
    Ok(())
}

fn read_pid(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    text.trim().parse::<u32>().ok()
}
