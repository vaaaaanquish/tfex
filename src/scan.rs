//! The `scan` command: discover Terraform directories and index them.
//!
//! A scan reads files and runs read-only Git commands. It never runs
//! Terraform, contacts a remote, or writes anywhere but the index.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use rayon::prelude::*;
use serde::Serialize;

use crate::config::{self, Backend, Configuration, ParseFallback};
use crate::discovery::{self, Classification};
use crate::error::{Error, Result};
use crate::git::{self, Repository};
use crate::identity::{RepoKey, RootId};
use crate::index::{Index, NewRoot, Writer};
use crate::state::{self, StateLocation};
use crate::{paths, time};

/// What a scan found.
#[derive(Debug, Default, Serialize)]
pub(crate) struct ScanReport {
    /// Directories containing Terraform configuration, modules included.
    pub(crate) indexed_directories: usize,
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
pub(crate) fn scan(index: &mut Index, paths: &[Utf8PathBuf]) -> Result<ScanReport> {
    let bases = resolve_bases(paths)?;
    let directories = discovery::terraform_directories(&bases);
    let repositories =
        git::repositories_of(&directories, |directory| enclosing_base(&bases, directory));
    let selection = Selection::of(&repositories);
    let configurations = read_configurations(&selection.directories);
    let local_modules = local_modules(&selection.directories, &configurations);
    let indexed_at = time::now();

    let mut indexed_roots = HashSet::new();
    let mut report = ScanReport {
        indexed_directories: selection.directories.len(),
        repositories: selection.repositories.len(),
        duplicate_checkouts: selection.duplicate_checkouts,
        ..ScanReport::default()
    };

    let writer = index.writer()?;
    for repository in &selection.repositories {
        writer.put_repository(repository, &indexed_at)?;
    }
    for ((directory, repository), configuration) in
        selection.directories.iter().zip(&configurations)
    {
        let relative_path =
            paths::relative_to(directory, &repository.checkout).unwrap_or_else(|| ".".to_owned());
        let id = RootId::derive(&repository.key, &relative_path);
        let evidence = discovery::classify(directory, configuration, &local_modules);
        let backend = configuration.backend.as_ref();

        writer.put_root(&NewRoot {
            id: &id,
            repository: &repository.key,
            relative_path: &relative_path,
            directory,
            evidence,
            backend,
            indexed_at: &indexed_at,
        })?;
        writer.replace_configuration(&id, configuration)?;
        index_states(&writer, &id, directory, backend)?;

        report.count(evidence.classification());
        report
            .parse_fallbacks
            .extend(configuration.fallbacks.iter().cloned());
        indexed_roots.insert(id);
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

/// Read the configuration of each directory, in parallel and in order.
fn read_configurations(directories: &[(&Utf8Path, &Repository)]) -> Vec<Configuration> {
    directories
        .par_iter()
        .map(|(directory, _)| config::read(directory))
        .collect()
}

/// Every directory that some configuration calls as a local module.
fn local_modules(
    directories: &[(&Utf8Path, &Repository)],
    configurations: &[Configuration],
) -> HashSet<Utf8PathBuf> {
    directories
        .iter()
        .zip(configurations)
        .flat_map(|((directory, _), configuration)| {
            configuration
                .module_calls
                .iter()
                .filter_map(|call| discovery::local_module_directory(directory, call))
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
