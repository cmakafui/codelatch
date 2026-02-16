mod doctor;
mod hook;
mod init;
mod run;
mod sessions;
mod start;
mod status;
mod stop;

use clap::{Args, Parser, Subcommand};
use tracing::info;

use crate::errors::Result;

#[derive(Debug, Parser)]
#[command(
    name = "codelatch",
    version,
    about = "Telegram supervision broker for Claude Code"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Run(RunArgs),
    Init,
    Start(StartArgs),
    Stop,
    Status,
    Doctor,
    Hook(HookArgs),
    Sessions,
}

#[derive(Debug, Args, Default, Clone)]
pub struct RunArgs {
    #[arg(long, default_value_t = false)]
    pub no_attach: bool,
    #[arg(last = true, trailing_var_arg = true)]
    pub claude_args: Vec<String>,
}

#[derive(Debug, Args, Clone)]
pub struct HookArgs {
    pub event: String,
}

#[derive(Debug, Args, Clone, Default)]
pub struct StartArgs {
    #[arg(long, hide = true, default_value_t = false)]
    pub background: bool,
    #[arg(long, hide = true, default_value_t = false)]
    pub foreground: bool,
}

pub async fn dispatch() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run(RunArgs::default())) {
        Command::Run(args) => run::execute(args).await?,
        Command::Init => init::execute().await?,
        Command::Start(args) => start::execute(args).await?,
        Command::Stop => stop::execute().await?,
        Command::Status => status::execute().await?,
        Command::Doctor => doctor::execute().await?,
        Command::Hook(args) => hook::execute(args).await?,
        Command::Sessions => sessions::execute().await?,
    }
    info!("command completed");
    Ok(())
}
