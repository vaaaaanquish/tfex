//! Writing to the index.
//!
//! All writes go through a [`Writer`], which holds one transaction: a scan
//! either replaces what it covers completely or leaves the index untouched.

use std::collections::HashSet;

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{Transaction, named_params};

use super::{Index, UNKNOWN_BACKEND};
use crate::config::{Backend, Configuration};
use crate::discovery::Evidence;
use crate::error::Result;
use crate::git::Repository;
use crate::identity::{RepoKey, RootId};
use crate::state::{StateLocation, StateStatus, StateSummary};

/// A Terraform directory to record.
#[derive(Debug)]
pub(crate) struct NewRoot<'a> {
    pub(crate) id: &'a RootId,
    pub(crate) repository: &'a RepoKey,
    /// Path inside the repository, `.` for its top level.
    pub(crate) relative_path: &'a str,
    pub(crate) directory: &'a Utf8Path,
    pub(crate) evidence: Evidence,
    pub(crate) backend: Option<&'a Backend>,
    pub(crate) indexed_at: &'a str,
}

/// A state location registered with `map-state`.
#[derive(Debug)]
pub(crate) struct StateMapping {
    pub(crate) workspace: String,
    pub(crate) location: StateLocation,
}

/// An open write transaction. Dropping it without [`Writer::commit`] rolls
/// everything back.
#[derive(Debug)]
pub(crate) struct Writer<'index> {
    transaction: Transaction<'index>,
}

impl Index {
    /// Start a write transaction.
    pub(crate) fn writer(&mut self) -> Result<Writer<'_>> {
        Ok(Writer {
            transaction: self.connection.transaction()?,
        })
    }
}

impl Writer<'_> {
    /// Make every change since the writer was created permanent.
    pub(crate) fn commit(self) -> Result<()> {
        Ok(self.transaction.commit()?)
    }

    /// Insert or update a repository.
    pub(crate) fn put_repository(&self, repository: &Repository, indexed_at: &str) -> Result<()> {
        self.transaction
            .prepare_cached(
                "INSERT INTO repositories (repo_key, name, remote, checkout, head, indexed_at)
                 VALUES (:repo_key, :name, :remote, :checkout, :head, :indexed_at)
                 ON CONFLICT (repo_key) DO UPDATE SET
                     name = excluded.name,
                     remote = excluded.remote,
                     checkout = excluded.checkout,
                     head = excluded.head,
                     indexed_at = excluded.indexed_at",
            )?
            .execute(named_params! {
                ":repo_key": repository.key,
                ":name": repository.name,
                ":remote": repository.remote,
                ":checkout": repository.checkout.as_str(),
                ":head": repository.head,
                ":indexed_at": indexed_at,
            })?;
        Ok(())
    }

    /// Insert or update a Terraform directory. What was recorded under it
    /// (configuration, states, state mappings) is left as it is.
    pub(crate) fn put_root(&self, root: &NewRoot<'_>) -> Result<()> {
        let backend_type = root
            .backend
            .map_or(UNKNOWN_BACKEND, |backend| &backend.kind);
        let backend_config = match root.backend {
            Some(backend) => serde_json::to_string(&backend.settings)?,
            None => "{}".to_owned(),
        };
        self.transaction
            .prepare_cached(
                "INSERT INTO roots (id, repo_key, relative_path, abs_path, classification,
                                    reason, backend_type, backend_config, indexed_at)
                 VALUES (:id, :repo_key, :relative_path, :abs_path, :classification,
                         :reason, :backend_type, :backend_config, :indexed_at)
                 ON CONFLICT (id) DO UPDATE SET
                     repo_key = excluded.repo_key,
                     relative_path = excluded.relative_path,
                     abs_path = excluded.abs_path,
                     classification = excluded.classification,
                     reason = excluded.reason,
                     backend_type = excluded.backend_type,
                     backend_config = excluded.backend_config,
                     indexed_at = excluded.indexed_at",
            )?
            .execute(named_params! {
                ":id": root.id,
                ":repo_key": root.repository,
                ":relative_path": root.relative_path,
                ":abs_path": root.directory.as_str(),
                ":classification": root.evidence.classification().as_str(),
                ":reason": root.evidence.reason(),
                ":backend_type": backend_type,
                ":backend_config": backend_config,
                ":indexed_at": root.indexed_at,
            })?;
        Ok(())
    }

    /// Replace the resources and module calls recorded for `root`.
    pub(crate) fn replace_configuration(
        &self,
        root: &RootId,
        configuration: &Configuration,
    ) -> Result<()> {
        self.transaction
            .prepare_cached("DELETE FROM resources WHERE root_id = ?1")?
            .execute([root])?;
        self.transaction
            .prepare_cached("DELETE FROM module_calls WHERE root_id = ?1")?
            .execute([root])?;

        // A block declared twice at the same place cannot be told apart, so
        // repeats are ignored instead of failing the scan.
        let mut insert_resource = self.transaction.prepare_cached(
            "INSERT OR IGNORE INTO resources (root_id, address, kind, type, name, file, line)
             VALUES (:root_id, :address, :kind, :type, :name, :file, :line)",
        )?;
        for resource in &configuration.resources {
            insert_resource.execute(named_params! {
                ":root_id": root,
                ":address": resource.address(),
                ":kind": resource.kind.as_str(),
                ":type": resource.type_name,
                ":name": resource.name,
                ":file": resource.file,
                ":line": resource.line,
            })?;
        }

        let mut insert_module_call = self.transaction.prepare_cached(
            "INSERT OR IGNORE INTO module_calls (root_id, name, source, file, line)
             VALUES (:root_id, :name, :source, :file, :line)",
        )?;
        for call in &configuration.module_calls {
            insert_module_call.execute(named_params! {
                ":root_id": root,
                ":name": call.name,
                ":source": call.source,
                ":file": call.file,
                ":line": call.line,
            })?;
        }
        Ok(())
    }

    /// Record the state of one workspace of `root` as it was just read,
    /// replacing what was recorded for that workspace.
    pub(crate) fn put_state(
        &self,
        root: &RootId,
        workspace: &str,
        location: &StateLocation,
        summary: &StateSummary,
    ) -> Result<()> {
        self.transaction
            .prepare_cached(
                "INSERT OR REPLACE INTO states
                     (root_id, workspace, backend_type, location, status, serial, lineage,
                      terraform_version, resource_count, fetched_at, error)
                 VALUES (:root_id, :workspace, :backend_type, :location, :status, :serial,
                         :lineage, :terraform_version, :resource_count, :fetched_at, :error)",
            )?
            .execute(named_params! {
                ":root_id": root,
                ":workspace": workspace,
                ":backend_type": location.backend_type(),
                ":location": location.as_str(),
                ":status": summary.status.as_str(),
                ":serial": summary.serial,
                ":lineage": summary.lineage,
                ":terraform_version": summary.terraform_version,
                ":resource_count": summary.resource_count,
                ":fetched_at": summary.fetched_at,
                ":error": summary.error,
            })?;
        Ok(())
    }

    /// Record where the state of one workspace of `root` is expected, without
    /// reading it: at `location` in a remote store, or nowhere known.
    ///
    /// What `tfex fetch` recorded for the workspace is kept as long as the
    /// state is still expected at the same place; otherwise the workspace
    /// becomes unresolved.
    pub(crate) fn expect_state(
        &self,
        root: &RootId,
        workspace: &str,
        backend: Option<&Backend>,
        location: Option<&StateLocation>,
    ) -> Result<()> {
        let backend_type = match (location, backend) {
            (Some(location), _) => location.backend_type(),
            (None, Some(backend)) => &backend.kind,
            (None, None) => UNKNOWN_BACKEND,
        };
        self.transaction
            .prepare_cached(
                "INSERT INTO states (root_id, workspace, backend_type, location, status)
                 VALUES (:root_id, :workspace, :backend_type, :location, :status)
                 ON CONFLICT (root_id, workspace) DO UPDATE SET
                     backend_type = excluded.backend_type,
                     location = excluded.location,
                     status = excluded.status,
                     serial = NULL,
                     lineage = NULL,
                     terraform_version = NULL,
                     resource_count = NULL,
                     fetched_at = NULL,
                     error = NULL
                 WHERE states.backend_type IS NOT excluded.backend_type
                    OR states.location IS NOT excluded.location",
            )?
            .execute(named_params! {
                ":root_id": root,
                ":workspace": workspace,
                ":backend_type": backend_type,
                ":location": location.map(StateLocation::as_str),
                ":status": StateStatus::Unresolved.as_str(),
            })?;
        Ok(())
    }

    /// Forget the states of the workspaces of `root` that are not among
    /// `workspaces`. State mappings are kept.
    pub(crate) fn retain_states<'a>(
        &self,
        root: &RootId,
        workspaces: impl IntoIterator<Item = &'a String>,
    ) -> Result<()> {
        let keep: HashSet<&String> = workspaces.into_iter().collect();
        let recorded: Vec<String> = {
            let mut statement = self
                .transaction
                .prepare_cached("SELECT workspace FROM states WHERE root_id = ?1")?;
            let workspaces = statement.query_map([root], |row| row.get("workspace"))?;
            workspaces.collect::<rusqlite::Result<_>>()?
        };
        let mut delete = self
            .transaction
            .prepare_cached("DELETE FROM states WHERE root_id = ?1 AND workspace = ?2")?;
        for workspace in recorded
            .iter()
            .filter(|workspace| !keep.contains(workspace))
        {
            delete.execute((root, workspace))?;
        }
        Ok(())
    }

    /// Remember where the state of `workspace` of `root` is.
    pub(crate) fn put_state_mapping(
        &self,
        root: &RootId,
        workspace: &str,
        location: &StateLocation,
    ) -> Result<()> {
        self.transaction
            .prepare_cached(
                "INSERT OR REPLACE INTO state_mappings (root_id, workspace, location, backend_type)
                 VALUES (:root_id, :workspace, :location, :backend_type)",
            )?
            .execute(named_params! {
                ":root_id": root,
                ":workspace": workspace,
                ":location": location.as_str(),
                ":backend_type": location.backend_type(),
            })?;
        Ok(())
    }

    /// The state mappings of `root`, in workspace order.
    pub(crate) fn state_mappings(&self, root: &RootId) -> Result<Vec<StateMapping>> {
        let mut statement = self.transaction.prepare_cached(
            "SELECT workspace, backend_type, location FROM state_mappings
             WHERE root_id = ?1 ORDER BY workspace",
        )?;
        let mappings = statement.query_map([root], |row| {
            Ok(StateMapping {
                workspace: row.get("workspace")?,
                location: StateLocation::from_stored(
                    &row.get::<_, String>("backend_type")?,
                    row.get("location")?,
                ),
            })
        })?;
        Ok(mappings.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete the directories under `bases` that are not in `keep`, together
    /// with everything recorded under them, and the repositories left without
    /// any directory. Returns how many directories were deleted.
    pub(crate) fn prune(&self, bases: &[Utf8PathBuf], keep: &HashSet<RootId>) -> Result<usize> {
        let stale: Vec<RootId> = {
            let mut statement = self
                .transaction
                .prepare_cached("SELECT id, abs_path FROM roots")?;
            let roots = statement.query_map([], |row| {
                Ok((
                    row.get::<_, RootId>("id")?,
                    row.get::<_, String>("abs_path")?,
                ))
            })?;
            roots
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(id, path)| {
                    !keep.contains(id)
                        && bases
                            .iter()
                            .any(|base| Utf8Path::new(path).starts_with(base))
                })
                .map(|(id, _)| id)
                .collect()
        };

        let mut delete_root = self
            .transaction
            .prepare_cached("DELETE FROM roots WHERE id = ?1")?;
        for id in &stale {
            delete_root.execute([id])?;
        }
        self.transaction.execute(
            "DELETE FROM repositories WHERE repo_key NOT IN (SELECT repo_key FROM roots)",
            [],
        )?;
        Ok(stale.len())
    }
}
