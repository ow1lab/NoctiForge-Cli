use anyhow::Result;
use clap::{Parser, Subcommand};

mod push;
mod trigger;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Trigger {
        action: String,
        payload: String,
        #[arg(value_name = "KEY=VALUE", trailing_var_arg = true)]
        metadata: Vec<String>,
    },
    Push {
        path: String,
    },
}

#[derive(Parser, Debug)]
struct Commands {
    #[command(subcommand)]
    command: Command,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();

    setup_tracing(cli.verbose)?;

    match cli.command {
        Command::Trigger {
            action,
            payload,
            metadata,
        } => trigger::run(action, payload, metadata).await?,
        Command::Push { path } => {
            push::run(&path).await?;
        }
    }

    Ok(())
}

fn setup_tracing(verbosity: u8) -> Result<()> {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(level))?;

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    Ok(())
}
