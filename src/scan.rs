//! The `scan` command: discover Terraform directories and index them.
//!
//! A scan reads files and runs read-only Git commands. It never runs
//! Terraform, contacts a remote, or writes anywhere but the index.
//!
//! A directory whose configuration files are as the previous scan found them
//! is not read again: what the index holds about it is kept.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use rayon::prelude::*;
use serde::Serialize;

use crate::config::{self, Backend, Configuration, Outline, ParseFallback};
use crate::discovery::{self, Classification};
use crate::error::{Error, Result};
use crate::git::{self, Repository};
use crate::identity::{RepoKey, RootId};
use crate::index::{Index, KnownDirectory, NewRoot, Writer};
use crate::state::{self, StateLocation};
use crate::{paths, time};

/// What a scan found.
#[derive(Debug, Default, Serialize)]
pub(crate) struct ScanReport {
    /// Directories containing Terraform configuration, modules included.
    pub(crate) indexed_directories: usize,
    /// How many of them were not read again, because their configuration
    /// files are as the previous scan found them.
    pub(crate) unchanged_directories: usize,
    pub(crate) repositories: usize,
    pub(crate) confirmed_roots: usize,
    pub(crate) probable_roots: usize,
    pub(crate) modules: usize,
    /// Checkouts left out because another checkout of the same repository was
    /// indexed instead.
    pub(crate) duplicate_checkouts: usize,
    /// Directories that were in the index and no longer exist under the
    /// scanned paths.
    pub(crate) pruned_roots: usize,
    /// Files with syntax errors, indexed on a best-effort basis.
    pub(crate) parse_fallbacks: Vec<ParseFallback>,
}

impl ScanReport {
    fn count(&mut self, classification: Classification) {
        match classification {
            Classification::Confirmed => self.confirmed_roots += 1,
            Classification::Probable => self.probable_roots += 1,
            Classification::Module => self.modules += 1,
            Classification::Unknown => {}
        }
    }
}

/// Index every Terraform directory under `paths`, replacing what the index
/// held for those paths.
///
/// With `full`, every directory is read again, whether or not its files
/// look unchanged.
pub(crate) fn scan(index: &mut Index, paths: &[Utf8PathBuf], full: bool) -> Result<ScanReport> {
    let bases = resolve_bases(paths)?;
    let directories = discovery::terraform_directories(&bases);
    let repositories =
        git::repositories_of(&directories, |directory| enclosing_base(&bases, directory));
    let selection = Selection::of(&repositories);
    let known = if full {
        HashMap::new()
    } else {
        index.known_directories()?
    };
    let directories = read_directories(&selection.directories, &known);
    let local_modules: HashSet<&Utf8Path> = directories
        .iter()
        .flat_map(|directory| &directory.outline.local_modules)
        .map(Utf8PathBuf::as_path)
        .collect();
    let indexed_at = time::now();

    let mut indexed_roots = HashSet::new();
    let mut report = ScanReport {
        indexed_directories: directories.len(),
        repositories: selection.repositories.len(),
        duplicate_checkouts: selection.duplicate_checkouts,
        ..ScanReport::default()
    };

    let writer = index.writer()?;
    for repository in &selection.repositories {
        writer.put_repository(repository, &indexed_at)?;
    }
    for directory in &directories {
        let evidence = discovery::classify(directory.path, &directory.outline, &local_modules);
        let backend = directory.outline.backend.as_ref();
        writer.put_root(&NewRoot {
            id: &directory.id,
            repository: &directory.repository.key,
            relative_path: &directory.relative_path,
            directory: directory.path,
            evidence,
            backend,
            has_provider: directory.outline.has_provider,
            fingerprint: directory.fingerprint.as_deref(),
            indexed_at: &indexed_at,
        })?;
        if let Some(configuration) = &directory.configuration {
            writer.replace_configuration(&directory.id, configuration)?;
            report
                .parse_fallbacks
                .extend(configuration.fallbacks.iter().cloned());
        } else {
            report.unchanged_directories += 1;
        }
        // States change without the configuration changing, so they are
        // looked at whether or not the directory was read.
        index_states(&writer, &directory.id, directory.path, backend)?;

        report.count(evidence.classification());
        indexed_roots.insert(directory.id.clone());
    }
    report.pruned_roots = writer.prune(&bases, &indexed_roots)?;
    writer.commit()?;
    Ok(report)
}

/// The canonical form of each path to scan, all of which must be directories.
fn resolve_bases(paths: &[Utf8PathBuf]) -> Result<Vec<Utf8PathBuf>> {
    paths
        .iter()
        .map(|path| {
            let base = paths::resolve(&paths::expand_user(path))?;
            if base.is_dir() {
                Ok(base)
            } else {
                Err(Error::NotADirectory(base))
            }
        })
        .collect()
}

/// What a scan indexes: one checkout of each repository.
#[derive(Debug)]
struct Selection<'a> {
    /// The checkout chosen for each repository.
    repositories: Vec<&'a Repository>,
    /// The directories in those checkouts, in path order.
    directories: Vec<(&'a Utf8Path, &'a Repository)>,
    /// Number of checkouts that were not chosen.
    duplicate_checkouts: usize,
}

impl<'a> Selection<'a> {
    /// Choose one checkout per repository and keep the directories in it.
    ///
    /// The identity of a directory is its repository and its path inside it,
    /// so the same directory in two checkouts of one repository (two clones,
    /// or linked worktrees) is one entry in the index. Indexing both would
    /// leave whichever was written last. Instead the main working tree is
    /// preferred to linked worktrees, and then the checkout whose path sorts
    /// first.
    fn of(repositories: &'a BTreeMap<&'a Utf8Path, Arc<Repository>>) -> Self {
        let mut chosen: HashMap<&RepoKey, &Repository> = HashMap::new();
        let mut checkouts: HashSet<&Utf8Path> = HashSet::new();
        for repository in repositories.values().map(Arc::as_ref) {
            checkouts.insert(&repository.checkout);
            let current = chosen.entry(&repository.key).or_insert(repository);
            if preference(repository) < preference(current) {
                *current = repository;
            }
        }
        let is_chosen = |repository: &Repository| {
            chosen
                .get(&repository.key)
                .is_some_and(|chosen| chosen.checkout == repository.checkout)
        };
        let directories = repositories
            .iter()
            .map(|(directory, repository)| (*directory, repository.as_ref()))
            .filter(|(_, repository)| is_chosen(repository))
            .collect();
        Selection {
            duplicate_checkouts: checkouts.len() - chosen.len(),
            repositories: chosen.into_values().collect(),
            directories,
        }
    }
}

/// Sort key under which the checkout to index comes first.
fn preference(repository: &Repository) -> (bool, &Utf8Path) {
    (repository.is_linked_worktree, &repository.checkout)
}

/// A directory to index.
#[derive(Debug)]
struct ScannedDirectory<'a> {
    path: &'a Utf8Path,
    repository: &'a Repository,
    /// Path inside the repository, `.` for its top level.
    relative_path: String,
    id: RootId,
    outline: Outline,
    /// The fingerprint to record: `None` when the directory has to be read
    /// again by the next scan.
    fingerprint: Option<String>,
    /// What the directory declares. `None` when the directory is unchanged
    /// since the previous scan, and what the index holds stays as it is.
    configuration: Option<Configuration>,
}

/// Read the configuration of each directory that changed since `known` was
/// recorded, in parallel and in order.
fn read_directories<'a>(
    directories: &[(&'a Utf8Path, &'a Repository)],
    known: &HashMap<RootId, KnownDirectory>,
) -> Vec<ScannedDirectory<'a>> {
    directories
        .par_iter()
        .map(|&(path, repository)| {
            let relative_path =
                paths::relative_to(path, &repository.checkout).unwrap_or_else(|| ".".to_owned());
            let id = RootId::derive(&repository.key, &relative_path);
            // The fingerprint is taken before the files are read: a file that
            // changes in between is then read again by the next scan.
            let fingerprint = config::fingerprint(path);
            let unchanged = known.get(&id).filter(|known| {
                // The identity of a directory does not say where it is, and
                // module sources are resolved against its location.
                known.path == path && Some(&known.fingerprint) == fingerprint.as_ref()
            });
            let (outline, fingerprint, configuration) = if let Some(known) = unchanged {
                (known.outline.clone(), fingerprint, None)
            } else {
                let configuration = config::read(path);
                // A file with a syntax error is reported by every scan until
                // it is fixed.
                let fingerprint = fingerprint.filter(|_| configuration.fallbacks.is_empty());
                (configuration.outline(), fingerprint, Some(configuration))
            };
            ScannedDirectory {
                path,
                repository,
                relative_path,
                id,
                outline,
                fingerprint,
                configuration,
            }
        })
        .collect()
}

/// The innermost scanned path containing `directory`. It stands in as the
/// repository of directories outside Git.
fn enclosing_base<'a>(bases: &'a [Utf8PathBuf], directory: &'a Utf8Path) -> &'a Utf8Path {
    bases
        .iter()
        .filter(|base| directory.starts_with(base))
        .max_by_key(|base| base.components().count())
        .map_or(directory, Utf8PathBuf::as_path)
}

/// Record where the state of each workspace of a directory is.
///
/// A local state file is read here and now. A remote state is only noted:
/// what `tfex fetch` recorded about it earlier is kept while it stays where
/// it was.
fn index_states(
    writer: &Writer<'_>,
    root: &RootId,
    directory: &Utf8Path,
    backend: Option<&Backend>,
) -> Result<()> {
    let mappings = writer
        .state_mappings(root)?
        .into_iter()
        .map(|mapping| (mapping.workspace, mapping.location));
    let located = state::locate(directory, backend, mappings);
    for (workspace, location) in &located {
        match location {
            Some(local @ StateLocation::Local(_)) => {
                writer.put_state(root, workspace, local, &local.summarize_offline())?;
            }
            remote_or_unknown => {
                writer.expect_state(root, workspace, backend, remote_or_unknown.as_ref())?;
            }
        }
    }
    writer.retain_states(root, located.keys())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enclosing_base_is_the_innermost_one() {
        let bases = [
            Utf8PathBuf::from("/work"),
            Utf8PathBuf::from("/work/infra"),
            Utf8PathBuf::from("/other"),
        ];
        assert_eq!(
            enclosing_base(&bases, Utf8Path::new("/work/infra/envs/prod")),
            "/work/infra"
        );
        assert_eq!(
            enclosing_base(&bases, Utf8Path::new("/work/services/api")),
            "/work"
        );
    }

    #[test]
    fn selection_prefers_the_main_working_tree_then_the_first_path() {
        let checkout = |path: &str, origin: &str, is_linked_worktree: bool| {
            Arc::new(Repository {
                key: RepoKey::derive(origin),
                name: path.to_owned(),
                remote: Some(origin.to_owned()),
                checkout: Utf8PathBuf::from(path),
                head: None,
                is_linked_worktree,
            })
        };
        let worktree = checkout("/a/worktree", "origin-1", true);
        let main = checkout("/b/main", "origin-1", false);
        let second_clone = checkout("/c/clone", "origin-1", false);
        let other = checkout("/d/other", "origin-2", false);
        let repositories: BTreeMap<&Utf8Path, Arc<Repository>> = [
            ("/a/worktree/envs/prod", &worktree),
            ("/b/main/envs/prod", &main),
            ("/b/main/modules/network", &main),
            ("/c/clone/envs/prod", &second_clone),
            ("/d/other", &other),
        ]
        .into_iter()
        .map(|(directory, repository)| (Utf8Path::new(directory), Arc::clone(repository)))
        .collect();

        let selection = Selection::of(&repositories);
        let directories: Vec<&Utf8Path> = selection
            .directories
            .iter()
            .map(|(directory, _)| *directory)
            .collect();
        assert_eq!(
            directories,
            ["/b/main/envs/prod", "/b/main/modules/network", "/d/other"]
        );
        assert_eq!(selection.repositories.len(), 2);
        assert_eq!(selection.duplicate_checkouts, 2);
    }

    #[test]
    fn rejects_a_path_that_is_not_a_directory() {
        let error = resolve_bases(&[Utf8PathBuf::from("/nonexistent-tfex")]).unwrap_err();
        assert_eq!(error.kind(), "not_a_directory");
    }
}
