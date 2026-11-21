//! The command line: arguments, dispatch, and JSON output.

use std::io::{self, ErrorKind, Write};
use std::process::ExitCode;
use std::time::Duration;

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::index::Index;
use crate::{commands, fetch, scan};

const DB_HELP: &str = "SQLite index file [default: $XDG_DATA_HOME/tfex/index.sqlite3]";
const LOCATION_HELP: &str = "A local state file, or a remote URI such as gs://bucket/path.tfstate";
const ROOT_HELP: &str = "Root ID or unique prefix of one, <repository>/<path>, or a directory";

/// Read-only, multi-repository Terraform code and state explorer.
///
/// Every command prints JSON to standard output. A failure prints
/// {"error": "...", "kind": "..."} to standard error and exits with status 1.
#[derive(Debug, Parser)]
#[command(version)]
struct Arguments {
    #[arg(long, global = true, env = "TFEX_DB", value_name = "PATH", help = DB_HELP)]
    db: Option<Utf8PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover and index Terraform in directories or repositories
    Scan {
        /// Directories to scan recursively
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<Utf8PathBuf>,
    },

    /// List the discovered roots
    #[command(visible_alias = "ls")]
    Roots {
        /// Include reusable modules
        #[arg(long)]
        all: bool,
    },

    /// Search resources by address or type
    Search {
        /// Text that the address or type must contain
        query: String,

        /// Maximum number of results
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },

    /// Show the resources, module calls, states and latest commit of a root
    Inspect {
        #[arg(help = ROOT_HELP)]
        root: String,
    },

    /// Show the recent Git history of a root
    Log {
        #[arg(help = ROOT_HELP)]
        root: String,

        /// Maximum number of commits
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },

    /// Show state availability and freshness, for one root or for all
    Status {
        #[arg(help = ROOT_HELP)]
        root: Option<String>,
    },

    /// Read each state from where it is, remote backends included
    ///
    /// This is the only command that uses the network. Remote states are
    /// read with the gcloud and aws command-line tools and the credentials
    /// they are already set up with. Only metadata is kept.
    Fetch {
        /// Roots whose states to read [default: every root]
        #[arg(value_name = "ROOT")]
        roots: Vec<String>,

        /// Seconds to wait for each state
        #[arg(long, default_value_t = 60, value_name = "SECONDS")]
        timeout: u64,
    },

    /// Declare where the state of a workspace of a root is
    MapState {
        #[arg(help = ROOT_HELP)]
        root: String,

        #[arg(help = LOCATION_HELP)]
        location: String,

        /// The workspace the state belongs to
        #[arg(long, default_value = "default")]
        workspace: String,
    },
}

/// A failure, as printed to standard error.
#[derive(Debug, Serialize)]
struct Failure {
    error: String,
    kind: &'static str,
}

/// Parse the command line, run the command, and report its outcome.
pub(crate) fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let failure = Failure {
                error: error.to_string(),
                kind: error.kind(),
            };
            // If the failure itself cannot be serialized, the plain message
            // is still better than nothing.
            let line = serde_json::to_string(&failure).unwrap_or(failure.error);
            eprintln!("{line}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Arguments) -> Result<()> {
    let location = Index::locate(arguments.db.as_deref())?;
    let mut index = Index::open(&location)?;
    match arguments.command {
        Command::Scan { paths } => print(&scan::scan(&mut index, &paths)?),
        Command::Roots { all } => print(&index.roots(all)?),
        Command::Search { query, limit } => print(&index.search(&query, limit)?),
        Command::Inspect { root } => print(&commands::inspect(&index, &root)?),
        Command::Log { root, limit } => print(&commands::log(&index, &root, limit)?),
        Command::Status { root: Some(root) } => print(&commands::status(&index, &root)?),
        Command::Status { root: None } => print(&index.state_overview()?),
        Command::Fetch { roots, timeout } => print(&fetch::fetch(
            &mut index,
            &roots,
            Duration::from_secs(timeout),
        )?),
        Command::MapState {
            root,
            location,
            workspace,
        } => print(&commands::map_state(
            &mut index, &root, &workspace, &location,
        )?),
    }
}

/// Print `value` as indented JSON on standard output.
fn print<T: Serialize>(value: &T) -> Result<()> {
    let mut json = serde_json::to_string_pretty(value)?;
    json.push('\n');
    match io::stdout().lock().write_all(json.as_bytes()) {
        // The reader went away, as in `tfex roots | head`; nothing to report.
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(Error::Output(error)),
        Ok(()) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_line_definition_is_consistent() {
        Arguments::command().debug_assert();
    }
}
