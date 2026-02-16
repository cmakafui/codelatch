use std::{ffi::OsString, path::PathBuf};

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

use super::{ServiceArgs, ServiceCommand};
use crate::errors::{AppError, Result};

pub async fn execute(args: ServiceArgs) -> Result<()> {
    let mut manager = <dyn ServiceManager>::native()?;
    if !manager.available()? {
        return Err(AppError::ServiceManager(
            "native service manager is unavailable on this machine".to_string(),
        ));
    }
    manager.set_level(ServiceLevel::User)?;

    let label = service_label()?;
    match args.command {
        ServiceCommand::Install => {
            let exe = std::env::current_exe()?;
            let install_ctx = ServiceInstallCtx {
                label: label.clone(),
                program: exe,
                args: vec![OsString::from("start"), OsString::from("--foreground")],
                contents: None,
                username: None,
                working_directory: current_dir(),
                environment: None,
                autostart: true,
                restart_policy: RestartPolicy::OnFailure {
                    delay_secs: Some(2),
                },
            };
            manager.install(install_ctx)?;
            manager.start(ServiceStartCtx {
                label: label.clone(),
            })?;
            println!(
                "Service installed and started: {}",
                label.to_qualified_name()
            );
        }
        ServiceCommand::Uninstall => {
            let _ = manager.stop(ServiceStopCtx {
                label: label.clone(),
            });
            manager.uninstall(ServiceUninstallCtx {
                label: label.clone(),
            })?;
            println!("Service uninstalled: {}", label.to_qualified_name());
        }
        ServiceCommand::Status => {
            let status = manager.status(ServiceStatusCtx {
                label: label.clone(),
            })?;
            println!(
                "Service status ({}): {:?}",
                label.to_qualified_name(),
                status
            );
        }
    }

    Ok(())
}

fn service_label() -> Result<service_manager::ServiceLabel> {
    "com.codelatch.codelatchd"
        .parse::<service_manager::ServiceLabel>()
        .map_err(|err| AppError::ServiceManager(err.to_string()))
}

fn current_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}
