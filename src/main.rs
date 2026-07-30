use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod github;
mod import;
mod snapshot;
mod store;
mod time;

use github::GitHub;
use store::Store;

#[derive(Parser)]
#[command(name = "afterglow", version, about)]
struct Cli {
    /// SQLite store to open, created and migrated if absent
    #[arg(
        long,
        global = true,
        env = "AFTERGLOW_DB",
        default_value = "data/afterglow.db",
        value_name = "PATH"
    )]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve badges and rankings
    Serve,
    /// Snapshot the tracked set
    Snapshot,
    /// Bring existing history into the store
    Import {
        #[command(subcommand)]
        source: Source,
    },
}

#[derive(Subcommand)]
enum Source {
    /// Snapshot TSV from the launchd collector; 4- and 7-column rows both load
    Tsv {
        /// TSV to read, `ts repo stars created [forks open_issues pushed_at]`
        path: PathBuf,
    },
    /// Monthly gross WatchEvent rollups from the archive harvest
    Prehistory {
        /// TSV to read, `repo_name month watch_events`
        path: PathBuf,
        /// Manifest holding the capture-ratio table
        /// [default: prehistory-MANIFEST.md beside PATH]
        #[arg(long, value_name = "PATH")]
        manifest: Option<PathBuf>,
    },
}

/// Counts in the run reports land on 1 often enough to be worth the four lines.
pub fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => anyhow::bail!("serve is not implemented yet"),
        Command::Snapshot => {
            let gh = GitHub::from_env()?;
            snapshot::run(&mut Store::open(&cli.db)?, &gh)
        }
        Command::Import { source } => match source {
            Source::Tsv { path } => import::run_tsv(&mut Store::open(&cli.db)?, &path),
            Source::Prehistory { path, manifest } => {
                import::run_prehistory(&mut Store::open(&cli.db)?, &path, manifest)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn db_flag_is_accepted_after_every_subcommand() {
        for argv in [
            vec!["afterglow", "snapshot", "--db", "x.db"],
            vec!["afterglow", "serve", "--db", "x.db"],
            vec!["afterglow", "import", "tsv", "s.tsv", "--db", "x.db"],
            vec!["afterglow", "import", "prehistory", "p.tsv", "--db", "x.db"],
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(cli.db, std::path::Path::new("x.db"));
        }
    }
}
