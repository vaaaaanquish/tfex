//! tfex: a read-only, multi-repository Terraform code and state explorer.
//!
//! tfex keeps a local index of the Terraform configurations scattered across
//! directories and Git repositories, so that a coding agent (or a person) can
//! quickly answer "where is this resource declared?", "which directories are
//! roots?", "what is known about their state?" and "what changed recently?".
//!
//! The crate is organized by what each module knows about:
//!
//! - [`cli`]: the command line, and nothing below it knows it exists.
//! - [`scan`], [`fetch`] and [`commands`]: one function per command.
//! - [`discovery`]: which directories hold Terraform, and which are roots.
//! - [`config`]: what a directory's configuration files declare.
//! - [`state`]: where states are, and the metadata kept about them.
//! - [`remote`]: reading states from remote stores; the only network access.
//! - [`git`]: repository identity and history, through the `git` command.
//! - [`process`]: running those external tools with a time limit.
//! - [`index`]: the `SQLite` index, the only thing tfex writes.
//! - [`identity`], [`paths`], [`time`], [`error`]: shared vocabulary.

mod cli;
mod commands;
mod config;
mod discovery;
mod error;
mod fetch;
mod git;
mod identity;
mod index;
mod paths;
mod process;
mod remote;
mod scan;
mod state;
mod time;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main()
}
