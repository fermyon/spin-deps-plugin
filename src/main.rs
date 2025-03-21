use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod common;
mod language;
use commands::{add::AddCommand, publish::PublishCommand};

/// Main CLI structure for command-line argument parsing.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    /// The command to execute, which can be a subcommand.
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Add a new component dependency
    Add(AddCommand),

    /// Publish dependency to a compatible registry
    Publish(PublishCommand),
}

#[tokio::main]
async fn main() -> Result<()> {
    let app = Cli::parse();

    match app.command {
        Commands::Add(cmd) => cmd.run().await?,
        Commands::Publish(cmd) => cmd.run().await?,
    }

    Ok(())
}
