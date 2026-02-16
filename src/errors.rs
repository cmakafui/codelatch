use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum AppError {
    #[error("could not resolve user home/config directory")]
    #[diagnostic(
        code(codelatch::config::paths),
        help("Set HOME, then retry `codelatch status`.")
    )]
    HomeDirUnavailable,

    #[error("failed to load config")]
    #[diagnostic(
        code(codelatch::config::load),
        help("Fix the config file syntax or run `codelatch init` to rewrite a template.")
    )]
    ConfigLoad,

    #[error("not configured")]
    #[diagnostic(
        code(codelatch::config::not_configured),
        help("Run `codelatch init` to bootstrap local configuration.")
    )]
    NotConfigured,

    #[error("failed to prepare config directory: {0}")]
    #[diagnostic(code(codelatch::config::mkdir))]
    CreateConfigDir(String),

    #[error("failed to write config file: {0}")]
    #[diagnostic(code(codelatch::config::write))]
    WriteConfig(String),

    #[error("failed to read stdin for hook event")]
    #[diagnostic(code(codelatch::hook::stdin))]
    HookReadStdin,

    #[error("daemon socket unavailable")]
    #[diagnostic(
        code(codelatch::daemon::socket_unavailable),
        help("Run `codelatch start` or retry with `codelatch run`.")
    )]
    DaemonUnavailable,

    #[error("timed out waiting for daemon socket")]
    #[diagnostic(
        code(codelatch::daemon::startup_timeout),
        help("Inspect logs with `RUST_LOG=codelatch=debug codelatch start`.")
    )]
    DaemonStartupTimeout,

    #[error("daemon already running")]
    #[diagnostic(
        code(codelatch::daemon::already_running),
        help("Use `codelatch status` to inspect current daemon state.")
    )]
    DaemonAlreadyRunning,

    #[error("failed to acquire daemon singleton lock: {0}")]
    #[diagnostic(code(codelatch::daemon::lock))]
    DaemonLock(String),

    #[error("tmux is not available")]
    #[diagnostic(code(codelatch::tmux::missing), help("Install tmux and retry."))]
    TmuxMissing,

    #[error("tmux command failed: {0}")]
    #[diagnostic(code(codelatch::tmux::failed))]
    TmuxFailed(String),

    #[error("Telegram token is invalid or unauthorized")]
    #[diagnostic(code(codelatch::telegram::auth))]
    TelegramAuthFailed,

    #[error("timed out waiting for `/start` from Telegram")]
    #[diagnostic(
        code(codelatch::telegram::pairing_timeout),
        help("Send `/start` to your bot, then rerun `codelatch init`.")
    )]
    TelegramPairingTimeout,

    #[error("telegram API error: {0}")]
    #[diagnostic(code(codelatch::telegram::api))]
    TelegramApi(String),

    #[error("invalid sqlite database path/config: {0}")]
    #[diagnostic(code(codelatch::db::config))]
    DbConfig(String),

    #[error("failed to serialize config")]
    #[diagnostic(code(codelatch::config::serialize))]
    ConfigSerialize,

    #[error("failed to parse existing Claude settings JSON")]
    #[diagnostic(code(codelatch::plugin::settings_parse))]
    PluginSettingsParse,

    #[error("doctor check failed: {0}")]
    #[diagnostic(code(codelatch::doctor::unhealthy))]
    DoctorUnhealthy(String),

    #[error("service manager error: {0}")]
    #[diagnostic(code(codelatch::service::manager))]
    ServiceManager(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, AppError>;
