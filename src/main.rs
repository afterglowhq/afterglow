use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "afterglow", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve badges and rankings
    Serve,
    /// Snapshot the tracked set
    Snapshot,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve => anyhow::bail!("serve is not implemented yet"),
        Command::Snapshot => anyhow::bail!("snapshot is not implemented yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
