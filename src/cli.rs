//! The command line: arguments, dispatch, and JSON output.

use std::io::{self, ErrorKind, Write};
use std::process::ExitCode;
use std::time::Duration;

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::index::Index;
use crate::plan::{self, Mode};
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

        /// Read every directory again, even those whose files look unchanged
        #[arg(long)]
        full: bool,
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

    /// Show module calls between directories, and references inside one
    ///
    /// Without a root: every module call in the index, with the directory
    /// it resolves to. With a root: its module calls, the directories it
    /// depends on and that depend on it, and the references between its
    /// declarations.
    Graph {
        #[arg(help = ROOT_HELP)]
        root: Option<String>,
    },

    /// Show what a declaration of a root depends on, and what depends on it
    Deps {
        #[arg(help = ROOT_HELP)]
        root: String,

        /// A resource (`aws_vpc.main`), data source (`data.x.y`), module call
        /// (`module.x`), local value (`local.x`), variable (`var.x`) or
        /// output (`output.x`)
        address: String,
    },

    /// Show state availability and freshness, for one root or for all
    Status {
        #[arg(help = ROOT_HELP)]
        root: Option<String>,
    },

    /// Read each state from where it is, remote backends included
    ///
    /// Apart from `plan`, this is the only command that uses the network.
    /// Remote states are read with the gcloud and aws command-line tools and
    /// the credentials they are already set up with. Only metadata is kept.
    Fetch {
        /// Roots whose states to read [default: every root]
        #[arg(value_name = "ROOT")]
        roots: Vec<String>,

        /// Seconds to wait for each state
        #[arg(long, default_value_t = 60, value_name = "SECONDS")]
        timeout: u64,
    },

    /// Run `terraform plan` in roots and report what would change
    ///
    /// This is the only command that runs Terraform, with whatever
    /// credentials it finds. The roots must already be initialized: tfex
    /// does not run `terraform init`. The state is not locked and no plan
    /// file is saved. Only addresses, actions and counts are kept.
    Plan {
        /// Roots to plan
        #[arg(required = true, value_name = "ROOT")]
        roots: Vec<String>,

        /// Detect drift: report what changed outside Terraform since the
        /// state was written, instead of what the configuration would change
        #[arg(long)]
        refresh_only: bool,

        /// The workspace to plan [default: the one selected in the root]
        #[arg(long, env = "TF_WORKSPACE", value_name = "NAME")]
        workspace: Option<String>,

        /// Seconds to wait for each plan
        #[arg(long, default_value_t = 600, value_name = "SECONDS")]
        timeout: u64,

        /// The program to run, such as `tofu` or a path to a terraform binary
        #[arg(
            long,
            env = "TFEX_TERRAFORM",
            default_value = "terraform",
            value_name = "PROGRAM"
        )]
        terraform: String,
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
        Command::Scan { paths, full } => print(&scan::scan(&mut index, &paths, full)?),
        Command::Roots { all } => print(&index.roots(all)?),
        Command::Search { query, limit } => print(&index.search(&query, limit)?),
        Command::Inspect { root } => print(&commands::inspect(&index, &root)?),
        Command::Log { root, limit } => print(&commands::log(&index, &root, limit)?),
        Command::Graph { root: Some(root) } => print(&commands::graph(&index, &root)?),
        Command::Graph { root: None } => print(&index.module_edges(None)?),
        Command::Deps { root, address } => print(&commands::dependencies(&index, &root, &address)?),
        Command::Status { root: Some(root) } => print(&commands::status(&index, &root)?),
        Command::Status { root: None } => print(&index.state_overview()?),
        Command::Fetch { roots, timeout } => print(&fetch::fetch(
            &mut index,
            &roots,
            Duration::from_secs(timeout),
        )?),
        Command::Plan {
            roots,
            refresh_only,
            workspace,
            timeout,
            terraform,
        } => {
            let options = plan::Options {
                mode: if refresh_only {
                    Mode::RefreshOnly
                } else {
                    Mode::Plan
                },
                program: terraform,
                workspace: workspace.filter(|workspace| !workspace.is_empty()),
                timeout: Duration::from_secs(timeout),
            };
            print(&plan::plan(&mut index, &roots, &options)?)
        }
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
