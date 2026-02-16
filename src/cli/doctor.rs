use std::{fs, path::Path, process::Stdio};

use tokio::{net::UnixStream, process::Command};

use super::DoctorArgs;
use crate::{
    config, daemon, db,
    errors::{AppError, Result},
    plugin,
};

pub async fn execute(args: DoctorArgs) -> Result<()> {
    let config = config::load()?;
    if !config.is_configured() {
        return Err(AppError::NotConfigured);
    }

    if args.fix {
        apply_fixes(&config).await?;
    }

    println!("Doctor checks:");
    let mut failures = Vec::new();

    if plugin::hooks_installed()? {
        println!("✅ hooks installed");
    } else {
        println!("❌ hooks missing");
        failures.push("hooks not installed".to_string());
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
        println!("❌ tmux unavailable");
        failures.push("tmux unavailable".to_string());
    }

    if UnixStream::connect(&config.socket_path).await.is_ok() {
        println!("✅ daemon socket reachable");
    } else {
        println!("❌ daemon socket unreachable ({})", config.socket_path);
        failures.push("daemon socket unreachable".to_string());
    }

    let pid_path = config::pid_path()?;
    if Path::new(&pid_path).exists() {
        let pid_text = fs::read_to_string(&pid_path).unwrap_or_default();
        let pid = pid_text.trim().to_string();
        if !pid.is_empty() {
            let alive = Command::new("kill")
                .args(["-0", &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|status| status.success());
            if alive {
                println!("✅ daemon pid alive ({pid})");
            } else {
                println!("❌ daemon pid not alive ({pid})");
                failures.push("pid file exists but process is not alive".to_string());
            }
        } else {
            println!("❌ daemon pid file empty ({})", pid_path.display());
            failures.push("pid file empty".to_string());
        }
    } else {
        println!("❌ daemon pid file missing ({})", pid_path.display());
        failures.push("pid file missing".to_string());
    }

    match db::connect(&config).await {
        Ok(_) => println!("✅ sqlite reachable"),
        Err(err) => {
            println!("❌ sqlite failure ({err})");
            failures.push("sqlite unreachable".to_string());
        }
    }

    match daemon::get_bot_username(config.token()?).await {
        Ok(username) => println!("✅ telegram auth ok (@{username})"),
        Err(err) => {
            println!("❌ telegram auth failed ({err})");
            failures.push("telegram auth failed".to_string());
        }
    }

    if failures.is_empty() {
        println!("Healthy ✅");
        Ok(())
    } else {
        Err(AppError::DoctorUnhealthy(failures.join("; ")))
    }
}

async fn apply_fixes(config: &config::Config) -> Result<()> {
    println!("Applying safe fixes...");

    if !plugin::hooks_installed()? {
        let binary_path = std::env::current_exe()?;
        plugin::install_hooks(&binary_path)?;
        plugin::write_plugin_artifacts(&binary_path)?;
        println!("✅ Reinstalled hooks");
    }

    if UnixStream::connect(&config.socket_path).await.is_err() {
        let exe = std::env::current_exe()?;
        std::process::Command::new(exe)
            .arg("start")
            .arg("--background")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        println!("✅ Restarted daemon");
    }

    Ok(())
}
