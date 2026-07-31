use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod badge;
mod github;
mod import;
mod serve;
mod site;
mod snapshot;
mod store;
mod time;
mod widths;

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
    Serve {
        /// Address to bind
        #[arg(
            long,
            env = "AFTERGLOW_LISTEN",
            default_value = "127.0.0.1:8080",
            value_name = "ADDR"
        )]
        listen: SocketAddr,
        /// PEM certificate chain; without the pair, plain HTTP for a proxy to front
        #[arg(
            long,
            env = "AFTERGLOW_TLS_CERT",
            requires = "tls_key",
            value_name = "PATH"
        )]
        tls_cert: Option<PathBuf>,
        /// PEM private key for --tls-cert
        #[arg(
            long,
            env = "AFTERGLOW_TLS_KEY",
            requires = "tls_cert",
            value_name = "PATH"
        )]
        tls_key: Option<PathBuf>,
    },
    /// Snapshot the tracked set
    Snapshot,
    /// Stop showing a repo, at its maintainer's request
    Optout {
        /// The repo to drop, as owner/name
        #[arg(value_name = "OWNER/REPO")]
        repo: String,
        /// Put it back in the tracked set
        #[arg(long)]
        undo: bool,
    },
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
        Command::Serve {
            listen,
            tls_cert,
            tls_key,
        } => serve::run(&cli.db, listen, tls_cert.zip(tls_key)),
        Command::Snapshot => {
            let gh = GitHub::from_env()?;
            snapshot::run(&mut Store::open(&cli.db)?, &gh)
        }
        Command::Optout { repo, undo } => store::opt_out(&Store::open(&cli.db)?, &repo, undo),
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
