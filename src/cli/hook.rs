use std::{env, io::Read};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::info;
use ulid::Ulid;

use super::HookArgs;
use crate::{
    config,
    errors::{AppError, Result},
    models::envelope::{HookEnvelope, HookResponseEnvelope},
};

pub async fn execute(args: HookArgs) -> Result<()> {
    let config = config::load()?;
    let mut payload_text = String::new();
    std::io::stdin()
        .read_to_string(&mut payload_text)
        .map_err(|_| AppError::HookReadStdin)?;

    let payload: Value = if payload_text.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&payload_text)?
    };

    let blocking = args.event == "PermissionRequest";
    let session_id = env::var("CODELATCH_SESSION_ID").unwrap_or_else(|_| Ulid::new().to_string());
    let session_name =
        env::var("CODELATCH_SESSION_NAME").unwrap_or_else(|_| "unmanaged-session".to_string());
    let tmux_pane = env::var("TMUX_PANE").ok();
    let cwd = env::current_dir()?.display().to_string();

    let envelope = HookEnvelope {
        version: 1,
        request_id: Ulid::new().to_string(),
        session_id,
        session_name,
        tmux_pane,
        hook_event_name: args.event,
        blocking,
        cwd,
        payload,
    };

    let stream = match UnixStream::connect(&config.socket_path).await {
        Ok(stream) => stream,
        Err(_) if blocking => {
            eprintln!("Codelatch daemon unavailable — denied for safety");
            std::process::exit(2);
        }
        Err(_) => return Err(AppError::DaemonUnavailable),
    };

    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
    let payload = serde_json::to_vec(&envelope)?;
    framed.send(Bytes::from(payload)).await?;

    if blocking {
        let Some(frame) = framed.next().await else {
            eprintln!("Codelatch daemon closed permission channel — denied for safety");
            std::process::exit(2);
        };
        let bytes = frame?;
        let response: HookResponseEnvelope = serde_json::from_slice(&bytes)?;
        let output = serde_json::to_string(&response.hook_output)?;
        println!("{output}");
        return Ok(());
    }

    info!(
        event = %envelope.hook_event_name,
        request_id = %envelope.request_id,
        "forwarded hook event"
    );
    Ok(())
}
