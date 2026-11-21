//! Repository identity and history, read through the `git` command.
//!
//! Only read-only Git commands are run, and never ones that contact a remote.
//! Everything degrades gracefully: without Git, or outside a repository,
//! directories are grouped by the scanned folder and have no history.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use rayon::prelude::*;
use serde::Serialize;

use crate::identity::RepoKey;
use crate::{paths, process};

/// Longest time a single Git command may take before it is abandoned.
const TIMEOUT: Duration = Duration::from_secs(15);

/// `git log` format: fields separated by the ASCII unit separator, records
/// terminated by the record separator, so that no subject can break parsing.
const LOG_FORMAT: &str = "--format=%H%x1f%aI%x1f%an%x1f%s%x1e";
const FIELD_SEPARATOR: char = '\x1f';
const RECORD_SEPARATOR: char = '\x1e';

/// A checkout of a Git repository, or a plain folder standing in for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repository {
    pub(crate) key: RepoKey,
    /// Name of the checkout directory.
    pub(crate) name: String,
    /// The origin remote, without credentials.
    pub(crate) remote: Option<String>,
    /// Top-level directory of the working tree.
    pub(crate) checkout: Utf8PathBuf,
    /// Commit checked out when the repository was read.
    pub(crate) head: Option<String>,
    /// Whether the checkout is a linked worktree (`git worktree add`) rather
    /// than the main working tree of its repository.
    pub(crate) is_linked_worktree: bool,
}

impl Repository {
    /// The checkout that `directory` is in, according to Git; `None` when Git
    /// is unavailable or does not consider it part of a working tree.
    fn open(directory: &Utf8Path) -> Option<Self> {
        let located = run(
            directory,
            &[
                "rev-parse",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
            ],
        )?;
        let mut lines = located.lines();
        let checkout = paths::canonical(Utf8Path::new(lines.next()?));
        // Both are printed relative to `directory` unless they are absolute.
        // A linked worktree keeps its own Git directory inside the common
        // one; anywhere else, submodules included, the two are the same.
        let git_directory = paths::canonical(&directory.join(lines.next()?));
        let common_directory = paths::canonical(&directory.join(lines.next()?));

        let remote = run(&checkout, &["remote", "get-url", "origin"])
            .filter(|remote| !remote.is_empty())
            .map(|remote| sanitize_remote(&remote));
        // Clones of one origin are the same repository, wherever they are.
        // Without an origin, a repository is where its main working tree is,
        // which its linked worktrees share.
        let origin = remote
            .as_deref()
            .unwrap_or_else(|| main_working_tree(&common_directory).as_str());
        Some(Repository {
            key: RepoKey::derive(origin),
            name: directory_name(&checkout),
            head: run(&checkout, &["rev-parse", "--verify", "--quiet", "HEAD"]),
            remote,
            checkout,
            is_linked_worktree: git_directory != common_directory,
        })
    }

    /// A folder outside any Git repository.
    fn plain(directory: &Utf8Path) -> Self {
        Repository {
            key: RepoKey::derive(directory.as_str()),
            name: directory_name(directory),
            remote: None,
            checkout: directory.to_owned(),
            head: None,
            is_linked_worktree: false,
        }
    }
}

/// The main working tree of the repository whose common Git directory is
/// `common_directory`: its parent when it is the usual `.git`, otherwise
/// (a bare repository, or a submodule) the Git directory itself.
fn main_working_tree(common_directory: &Utf8Path) -> &Utf8Path {
    match (common_directory.file_name(), common_directory.parent()) {
        (Some(".git"), Some(working_tree)) => working_tree,
        _ => common_directory,
    }
}

fn directory_name(directory: &Utf8Path) -> String {
    directory
        .file_name()
        .unwrap_or(directory.as_str())
        .to_owned()
}

/// The repository of each of `directories`.
///
/// A directory outside Git belongs to the folder that `enclosing_folder`
/// returns for it, which then stands in as its repository.
///
/// Git is asked once per checkout rather than once per directory, and the
/// checkouts are examined in parallel.
pub(crate) fn repositories_of<'a>(
    directories: &'a BTreeSet<Utf8PathBuf>,
    enclosing_folder: impl Fn(&'a Utf8Path) -> &'a Utf8Path,
) -> BTreeMap<&'a Utf8Path, Arc<Repository>> {
    let candidates: BTreeSet<&Utf8Path> = directories
        .iter()
        .filter_map(|directory| checkout_candidate(directory))
        .collect();
    let checkouts: HashMap<&Utf8Path, Arc<Repository>> = candidates
        .into_par_iter()
        .filter_map(|candidate| Some((candidate, Arc::new(Repository::open(candidate)?))))
        .collect();

    let mut folders: HashMap<&Utf8Path, Arc<Repository>> = HashMap::new();
    directories
        .iter()
        .map(|directory| {
            let checkout = checkout_candidate(directory).and_then(|found| checkouts.get(found));
            let repository = if let Some(checkout) = checkout {
                Arc::clone(checkout)
            } else {
                let folder = enclosing_folder(directory);
                let plain = folders
                    .entry(folder)
                    .or_insert_with(|| Arc::new(Repository::plain(folder)));
                Arc::clone(plain)
            };
            (directory.as_path(), repository)
        })
        .collect()
}

/// The nearest directory at or above `directory` that has a `.git` entry,
/// and so is probably the top of a working tree.
fn checkout_candidate(directory: &Utf8Path) -> Option<&Utf8Path> {
    directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
}

/// One commit in the history of a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Commit {
    pub(crate) sha: String,
    /// Author date, ISO 8601 with the author's UTC offset.
    pub(crate) date: String,
    pub(crate) author: String,
    pub(crate) subject: String,
}

/// The most recent commits, newest first, that touched `relative_path` in the
/// repository checked out at `checkout`. Empty when there is no history.
pub(crate) fn history(checkout: &Utf8Path, relative_path: &str, limit: u32) -> Vec<Commit> {
    let max_count = format!("--max-count={limit}");
    let mut arguments = vec!["log", max_count.as_str(), LOG_FORMAT];
    if relative_path != "." {
        arguments.extend(["--", relative_path]);
    }
    run(checkout, &arguments).map_or_default(|output| parse_log(&output))
}

fn parse_log(output: &str) -> Vec<Commit> {
    output
        .split(RECORD_SEPARATOR)
        .filter_map(|record| {
            let mut fields = record.trim().split(FIELD_SEPARATOR);
            let commit = Commit {
                sha: fields.next().filter(|sha| !sha.is_empty())?.to_owned(),
                date: fields.next()?.to_owned(),
                author: fields.next()?.to_owned(),
                subject: fields.next()?.to_owned(),
            };
            fields.next().is_none().then_some(commit)
        })
        .collect()
}

/// Remove credentials from a remote URL: user information, query string and
/// fragment are dropped. SCP-like remotes (`git@host:path`) cannot carry a
/// password and are kept as they are.
fn sanitize_remote(remote: &str) -> String {
    let Some((scheme, rest)) = remote.split_once("://") else {
        return remote.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_user_information, host)| host);
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    format!(
        "{scheme}://{}{}",
        host.to_ascii_lowercase(),
        &tail[..path_end]
    )
}

/// Run a read-only Git command in `directory` and return its trimmed output,
/// or `None` if Git is missing, fails, or takes longer than [`TIMEOUT`].
fn run(directory: &Utf8Path, arguments: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(arguments)
        // Never wait for input, and never take locks in the repository.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = process::output(&mut command, TIMEOUT).ok()?;
    Some(String::from_utf8_lossy(&output).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_drops_credentials_from_urls() {
        let cases = [
            (
                "https://password-token@github.com/acme/infra.git?token=secret#frag",
                "https://github.com/acme/infra.git",
            ),
            (
                "https://user:pass@GitHub.com:8443/acme/infra",
                "https://github.com:8443/acme/infra",
            ),
            (
                "ssh://git@github.com/acme/infra.git",
                "ssh://github.com/acme/infra.git",
            ),
            ("https://example.com?token=secret", "https://example.com"),
            ("file:///srv/git/infra.git", "file:///srv/git/infra.git"),
        ];
        for (remote, expected) in cases {
            assert_eq!(sanitize_remote(remote), expected, "{remote}");
        }
    }

    #[test]
    fn sanitizing_keeps_scp_like_remotes() {
        let remote = "git@github.com:acme/infra.git";
        assert_eq!(sanitize_remote(remote), remote);
    }

    #[test]
    fn parses_log_records() {
        let output = "aaa\x1f2026-01-02T03:04:05+09:00\x1fTest Person\x1fFirst: subject\x1e\n\
                      bbb\x1f2026-01-01T00:00:00+00:00\x1fOther\x1f\x1e";
        assert_eq!(
            parse_log(output),
            [
                Commit {
                    sha: "aaa".to_owned(),
                    date: "2026-01-02T03:04:05+09:00".to_owned(),
                    author: "Test Person".to_owned(),
                    subject: "First: subject".to_owned(),
                },
                Commit {
                    sha: "bbb".to_owned(),
                    date: "2026-01-01T00:00:00+00:00".to_owned(),
                    author: "Other".to_owned(),
                    subject: String::new(),
                },
            ]
        );
        assert_eq!(parse_log(""), []);
    }

    #[test]
    fn folder_outside_git_is_its_own_repository() {
        let repository = Repository::plain(Utf8Path::new("/srv/terraform"));
        assert_eq!(repository.name, "terraform");
        assert_eq!(repository.remote, None);
        assert_eq!(repository.head, None);
        assert!(!repository.is_linked_worktree);
        assert_eq!(repository.key, RepoKey::derive("/srv/terraform"));
    }

    #[test]
    fn main_working_tree_is_next_to_the_usual_git_directory() {
        assert_eq!(
            main_working_tree(Utf8Path::new("/work/infra/.git")),
            "/work/infra"
        );
        assert_eq!(
            main_working_tree(Utf8Path::new("/srv/git/infra.git")),
            "/srv/git/infra.git"
        );
        assert_eq!(
            main_working_tree(Utf8Path::new("/work/super/.git/modules/sub")),
            "/work/super/.git/modules/sub"
        );
    }

    #[test]
    fn directories_outside_git_share_their_enclosing_folder() {
        let folder = Utf8Path::new("/nonexistent-tfex/plain");
        let directories: BTreeSet<Utf8PathBuf> = [folder.join("one"), folder.join("two")].into();

        let repositories = repositories_of(&directories, |_| folder);
        let one = &repositories[folder.join("one").as_path()];
        let two = &repositories[folder.join("two").as_path()];
        assert!(Arc::ptr_eq(one, two));
        assert_eq!(one.checkout, folder);
    }
}
