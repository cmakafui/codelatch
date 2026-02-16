use crate::{
    config, db,
    errors::{AppError, Result},
};

pub async fn execute() -> Result<()> {
    let config = config::load()?;
    if !config.is_configured() {
        return Err(AppError::NotConfigured);
    }

    let pool = db::connect(&config).await?;
    let sessions = db::list_sessions(&pool).await?;
    if sessions.is_empty() {
        println!("No tracked sessions yet.");
        return Ok(());
    }

    for session in sessions {
        println!(
            "{} ({}) | {} | pane={} | last_seen={}",
            session.name, session.session_id, session.cwd, session.tmux_pane, session.last_seen_at
        );
    }
    Ok(())
}
