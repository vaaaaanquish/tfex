//! Reading from the index.
//!
//! The row types here are what the commands print: their field order is the
//! JSON key order, and every query has a total order so that output is
//! deterministic.

use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::types::Type;
use rusqlite::{Params, Row, named_params};
use serde::Serialize;
use serde_json::{Map, Value};

use super::{Index, UNKNOWN_BACKEND};
use crate::config::{Backend, Outline};
use crate::error::{Error, Result};
use crate::identity::RootId;
use crate::paths;
use crate::state::StateLocation;

/// A Terraform directory as listed by `roots`.
#[derive(Debug, Serialize)]
pub(crate) struct RootSummary {
    pub(crate) id: RootId,
    pub(crate) relative_path: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) classification: String,
    pub(crate) reason: String,
    pub(crate) backend_type: String,
    pub(crate) indexed_at: String,
    pub(crate) repository: String,
    pub(crate) remote: Option<String>,
    /// Number of resources and data sources declared in the directory.
    pub(crate) resources: usize,
}

impl RootSummary {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(RootSummary {
            id: row.get("id")?,
            relative_path: row.get("relative_path")?,
            path: path_column(row, "abs_path")?,
            classification: row.get("classification")?,
            reason: row.get("reason")?,
            backend_type: row.get("backend_type")?,
            indexed_at: row.get("indexed_at")?,
            repository: row.get("repository")?,
            remote: row.get("remote")?,
            resources: row.get("resources")?,
        })
    }
}

/// Everything recorded about one Terraform directory itself.
#[derive(Debug)]
pub(crate) struct RootRow {
    pub(crate) id: RootId,
    pub(crate) repository: String,
    /// Top-level directory of the repository's working tree.
    pub(crate) checkout: Utf8PathBuf,
    pub(crate) relative_path: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) classification: String,
    pub(crate) reason: String,
    pub(crate) backend_type: String,
    /// The allowlisted backend settings.
    pub(crate) backend: Map<String, Value>,
    pub(crate) indexed_at: String,
}

impl RootRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(RootRow {
            id: row.get("id")?,
            repository: row.get("repository")?,
            checkout: path_column(row, "checkout")?,
            relative_path: row.get("relative_path")?,
            path: path_column(row, "abs_path")?,
            classification: row.get("classification")?,
            reason: row.get("reason")?,
            backend_type: row.get("backend_type")?,
            backend: json_column(row, "backend_config")?,
            indexed_at: row.get("indexed_at")?,
        })
    }
}

/// A resource or data source declared in a directory.
#[derive(Debug, Serialize)]
pub(crate) struct ResourceRow {
    pub(crate) address: String,
    pub(crate) kind: String,
    #[serde(rename = "type")]
    pub(crate) type_name: String,
    pub(crate) name: String,
    pub(crate) file: String,
    pub(crate) line: usize,
}

impl ResourceRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(ResourceRow {
            address: row.get("address")?,
            kind: row.get("kind")?,
            type_name: row.get("type")?,
            name: row.get("name")?,
            file: row.get("file")?,
            line: row.get("line")?,
        })
    }
}

/// A module call made from a directory.
#[derive(Debug, Serialize)]
pub(crate) struct ModuleCallRow {
    pub(crate) name: String,
    pub(crate) source: Option<String>,
    pub(crate) file: String,
    pub(crate) line: usize,
}

impl ModuleCallRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(ModuleCallRow {
            name: row.get("name")?,
            source: row.get("source")?,
            file: row.get("file")?,
            line: row.get("line")?,
        })
    }
}

/// The state of one workspace of a directory.
#[derive(Debug, Serialize)]
pub(crate) struct StateRow {
    pub(crate) workspace: String,
    pub(crate) backend_type: String,
    pub(crate) location: Option<String>,
    pub(crate) status: String,
    pub(crate) serial: Option<i64>,
    pub(crate) lineage: Option<String>,
    pub(crate) terraform_version: Option<String>,
    pub(crate) resource_count: Option<usize>,
    pub(crate) fetched_at: Option<String>,
    pub(crate) error: Option<String>,
}

impl StateRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(StateRow {
            workspace: row.get("workspace")?,
            backend_type: row.get("backend_type")?,
            location: row.get("location")?,
            status: row.get("status")?,
            serial: row.get("serial")?,
            lineage: row.get("lineage")?,
            terraform_version: row.get("terraform_version")?,
            resource_count: row.get("resource_count")?,
            fetched_at: row.get("fetched_at")?,
            error: row.get("error")?,
        })
    }
}

/// A resource found by `search`. The declaration is at `path/file:line`.
#[derive(Debug, Serialize)]
pub(crate) struct SearchHit {
    pub(crate) repository: String,
    pub(crate) root_id: RootId,
    pub(crate) relative_path: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) address: String,
    pub(crate) kind: String,
    pub(crate) file: String,
    pub(crate) line: usize,
}

impl SearchHit {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(SearchHit {
            repository: row.get("repository")?,
            root_id: row.get("root_id")?,
            relative_path: row.get("relative_path")?,
            path: path_column(row, "abs_path")?,
            address: row.get("address")?,
            kind: row.get("kind")?,
            file: row.get("file")?,
            line: row.get("line")?,
        })
    }
}

/// One line of the `status` overview: a workspace of a directory.
#[derive(Debug, Serialize)]
pub(crate) struct StateOverviewRow {
    pub(crate) repository: String,
    pub(crate) root_id: RootId,
    pub(crate) relative_path: String,
    pub(crate) workspace: String,
    pub(crate) backend_type: String,
    pub(crate) location: Option<String>,
    pub(crate) status: String,
    pub(crate) resource_count: Option<usize>,
    pub(crate) fetched_at: Option<String>,
}

impl StateOverviewRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(StateOverviewRow {
            repository: row.get("repository")?,
            root_id: row.get("root_id")?,
            relative_path: row.get("relative_path")?,
            workspace: row.get("workspace")?,
            backend_type: row.get("backend_type")?,
            location: row.get("location")?,
            status: row.get("status")?,
            resource_count: row.get("resource_count")?,
            fetched_at: row.get("fetched_at")?,
        })
    }
}

/// The outcome of the last plan of one kind for a workspace of a directory.
#[derive(Debug, Serialize)]
pub(crate) struct PlanRow {
    pub(crate) workspace: String,
    /// `plan`, or `refresh-only` for a drift check.
    pub(crate) mode: String,
    pub(crate) status: String,
    pub(crate) terraform_version: Option<String>,
    pub(crate) add: usize,
    pub(crate) change: usize,
    pub(crate) remove: usize,
    pub(crate) import: usize,
    /// Number of resources found changed outside Terraform.
    pub(crate) drift: usize,
    pub(crate) planned_at: String,
    pub(crate) error: Option<String>,
}

impl PlanRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(PlanRow {
            workspace: row.get("workspace")?,
            mode: row.get("mode")?,
            status: row.get("status")?,
            terraform_version: row.get("terraform_version")?,
            add: row.get("to_add")?,
            change: row.get("to_change")?,
            remove: row.get("to_remove")?,
            import: row.get("to_import")?,
            drift: row.get("drifted")?,
            planned_at: row.get("planned_at")?,
            error: row.get("error")?,
        })
    }
}

/// An indexed directory, as one end of a module call.
#[derive(Debug, Serialize)]
pub(crate) struct DirectoryRef {
    pub(crate) root_id: RootId,
    pub(crate) repository: String,
    pub(crate) relative_path: String,
    pub(crate) classification: String,
}

impl DirectoryRef {
    /// Read the directory from the columns `<prefix>_root_id` and so on.
    fn from_row(row: &Row<'_>, prefix: &str) -> rusqlite::Result<Self> {
        let column = |name: &str| format!("{prefix}_{name}");
        Ok(DirectoryRef {
            root_id: row.get(column("root_id").as_str())?,
            repository: row.get(column("repository").as_str())?,
            relative_path: row.get(column("relative_path").as_str())?,
            classification: row.get(column("classification").as_str())?,
        })
    }
}

/// A module call, with the indexed directory it resolves to.
#[derive(Debug, Serialize)]
pub(crate) struct ModuleEdge {
    /// The directory making the call.
    pub(crate) caller: DirectoryRef,
    /// Name of the `module` block.
    pub(crate) module: String,
    pub(crate) source: Option<String>,
    pub(crate) file: String,
    pub(crate) line: usize,
    /// The directory called. `None` when the source is not a local path
    /// (a registry or Git module), is not a literal, or names a directory
    /// that is not indexed.
    pub(crate) target: Option<DirectoryRef>,
}

impl ModuleEdge {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        // The target columns are null when no indexed directory was joined.
        let has_target = row.get::<_, Option<RootId>>("target_root_id")?.is_some();
        Ok(ModuleEdge {
            caller: DirectoryRef::from_row(row, "caller")?,
            module: row.get("module")?,
            source: row.get("source")?,
            file: row.get("file")?,
            line: row.get("line")?,
            target: has_target
                .then(|| DirectoryRef::from_row(row, "target"))
                .transpose()?,
        })
    }
}

/// A directory reached by following module calls from another one.
#[derive(Debug, Serialize)]
pub(crate) struct RelatedDirectory {
    pub(crate) root_id: RootId,
    pub(crate) repository: String,
    pub(crate) relative_path: String,
    pub(crate) classification: String,
    /// Number of module calls in between: 1 for a direct call.
    pub(crate) depth: u32,
}

impl RelatedDirectory {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(RelatedDirectory {
            root_id: row.get("root_id")?,
            repository: row.get("repository")?,
            relative_path: row.get("relative_path")?,
            classification: row.get("classification")?,
            depth: row.get("depth")?,
        })
    }
}

/// A reference between two declarations of a directory, at `file:line`.
#[derive(Debug, Serialize)]
pub(crate) struct ReferenceRow {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) file: String,
    pub(crate) line: usize,
}

impl ReferenceRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(ReferenceRow {
            from: row.get("source")?,
            to: row.get("target")?,
            file: row.get("file")?,
            line: row.get("line")?,
        })
    }
}

/// A declaration reached by following references from another one.
#[derive(Debug, Serialize)]
pub(crate) struct RelatedAddress {
    pub(crate) address: String,
    /// Number of references in between: 1 for a direct reference.
    pub(crate) depth: u32,
}

impl RelatedAddress {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(RelatedAddress {
            address: row.get("address")?,
            depth: row.get("depth")?,
        })
    }
}

/// Which way to follow the edges of a graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// From what depends to what it depends on.
    DependsOn,
    /// From what is depended on to what depends on it.
    Dependents,
}

/// Longest chain of module calls or references that is followed. Terraform
/// rejects cycles, but tfex reads configurations Terraform never validated,
/// and must not follow one forever.
const MAX_DEPTH: u32 = 64;

/// What the index holds about a directory whose configuration a scan may
/// leave unread.
#[derive(Debug)]
pub(crate) struct KnownDirectory {
    pub(crate) path: Utf8PathBuf,
    /// The fingerprint of the configuration files the index was built from.
    pub(crate) fingerprint: String,
    pub(crate) outline: Outline,
}

/// A recorded state together with what is needed to read it again.
#[derive(Debug)]
pub(crate) struct StateTarget {
    pub(crate) repository: String,
    pub(crate) root_id: RootId,
    pub(crate) relative_path: String,
    pub(crate) workspace: String,
    /// Where the state is, or `None` when that is not known.
    pub(crate) location: Option<StateLocation>,
    /// The backend declared by the directory, and its allowlisted settings.
    pub(crate) declared_backend_type: String,
    pub(crate) declared_backend: Map<String, Value>,
}

impl StateTarget {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let backend_type: String = row.get("backend_type")?;
        let location: Option<String> = row.get("location")?;
        Ok(StateTarget {
            repository: row.get("repository")?,
            root_id: row.get("root_id")?,
            relative_path: row.get("relative_path")?,
            workspace: row.get("workspace")?,
            location: location.map(|location| StateLocation::from_stored(&backend_type, location)),
            declared_backend_type: row.get("declared_backend_type")?,
            declared_backend: json_column(row, "backend_config")?,
        })
    }
}

impl Index {
    /// The indexed directories, by repository and path. Reusable modules are
    /// left out unless `include_modules` is set.
    pub(crate) fn roots(&self, include_modules: bool) -> Result<Vec<RootSummary>> {
        self.query(
            "SELECT r.id, r.relative_path, r.abs_path, r.classification, r.reason,
                    r.backend_type, r.indexed_at, p.name AS repository, p.remote,
                    (SELECT COUNT(*) FROM resources x WHERE x.root_id = r.id) AS resources
             FROM roots r
             JOIN repositories p ON p.repo_key = r.repo_key
             WHERE :include_modules OR r.classification != 'module'
             ORDER BY p.name, r.relative_path, r.id",
            named_params! { ":include_modules": include_modules },
            RootSummary::from_row,
        )
    }

    /// The directory that `selector` names. A selector is one of:
    ///
    /// - a root ID, or a prefix of one;
    /// - `<repository>/<relative path>`, or `<repository>` for its top level;
    /// - the path of the directory on disk.
    ///
    /// An exact match is preferred to an ID prefix match.
    pub(crate) fn find_root(&self, selector: &str) -> Result<RootRow> {
        let directory = paths::resolve(&paths::expand_user(Utf8Path::new(selector)))
            .ok()
            .filter(|path| path.is_dir());
        let candidates = self.query(
            r"SELECT r.id, r.relative_path, r.abs_path, r.classification, r.reason,
                     r.backend_type, r.backend_config, r.indexed_at,
                     p.name AS repository, p.checkout,
                     (r.id = :selector
                      OR p.name || '/' || r.relative_path = :selector
                      OR (r.relative_path = '.' AND p.name = :selector)
                      OR r.abs_path IS :directory) AS is_exact
              FROM roots r
              JOIN repositories p ON p.repo_key = r.repo_key
              WHERE is_exact OR r.id LIKE :id_prefix ESCAPE '\'
              ORDER BY r.id",
            named_params! {
                ":selector": selector,
                ":directory": directory.as_ref().map(|path| path.as_str()),
                ":id_prefix": format!("{}%", escape_like(selector)),
            },
            |row| Ok((row.get::<_, bool>("is_exact")?, RootRow::from_row(row)?)),
        )?;

        let has_exact = candidates.iter().any(|(is_exact, _)| *is_exact);
        let mut matches = candidates
            .into_iter()
            .filter(|(is_exact, _)| *is_exact == has_exact)
            .map(|(_, root)| root);
        match (matches.next(), matches.next()) {
            (Some(root), None) => Ok(root),
            (None, _) => Err(Error::RootNotFound(selector.to_owned())),
            (Some(_), Some(_)) => Err(Error::AmbiguousRoot(selector.to_owned())),
        }
    }

    /// The resources and data sources declared in `root`, by address.
    pub(crate) fn resources(&self, root: &RootId) -> Result<Vec<ResourceRow>> {
        self.query(
            "SELECT address, kind, type, name, file, line FROM resources
             WHERE root_id = ?1
             ORDER BY address, file, line",
            [root],
            ResourceRow::from_row,
        )
    }

    /// The module calls made from `root`, by name.
    pub(crate) fn module_calls(&self, root: &RootId) -> Result<Vec<ModuleCallRow>> {
        self.query(
            "SELECT name, source, file, line FROM module_calls
             WHERE root_id = ?1
             ORDER BY name, file, line",
            [root],
            ModuleCallRow::from_row,
        )
    }

    /// The states of `root`, by workspace.
    pub(crate) fn states(&self, root: &RootId) -> Result<Vec<StateRow>> {
        self.query(
            "SELECT workspace, backend_type, location, status, serial, lineage,
                    terraform_version, resource_count, fetched_at, error
             FROM states
             WHERE root_id = ?1
             ORDER BY workspace",
            [root],
            StateRow::from_row,
        )
    }

    /// The last plans of `root`, by workspace and kind.
    pub(crate) fn plans(&self, root: &RootId) -> Result<Vec<PlanRow>> {
        self.query(
            "SELECT workspace, mode, status, terraform_version, to_add, to_change, to_remove,
                    to_import, drifted, planned_at, error
             FROM plans
             WHERE root_id = ?1
             ORDER BY workspace, mode",
            [root],
            PlanRow::from_row,
        )
    }

    /// The module calls made from `root`, or from every directory when it is
    /// `None`, each with the indexed directory it resolves to.
    pub(crate) fn module_edges(&self, root: Option<&RootId>) -> Result<Vec<ModuleEdge>> {
        self.query(
            "SELECT r.id AS caller_root_id, p.name AS caller_repository,
                    r.relative_path AS caller_relative_path,
                    r.classification AS caller_classification,
                    c.name AS module, c.source, c.file, c.line,
                    t.id AS target_root_id, q.name AS target_repository,
                    t.relative_path AS target_relative_path,
                    t.classification AS target_classification
             FROM module_calls c
             JOIN roots r ON r.id = c.root_id
             JOIN repositories p ON p.repo_key = r.repo_key
             LEFT JOIN roots t ON t.abs_path = c.target_path
             LEFT JOIN repositories q ON q.repo_key = t.repo_key
             WHERE :root IS NULL OR c.root_id = :root
             ORDER BY p.name, r.relative_path, c.name, c.file, c.line, r.id, t.id",
            named_params! { ":root": root },
            ModuleEdge::from_row,
        )
    }

    /// The directories that `root` calls as modules, directly or through
    /// other modules, or the ones that call it, nearest first.
    pub(crate) fn related_directories(
        &self,
        root: &RootId,
        direction: Direction,
    ) -> Result<Vec<RelatedDirectory>> {
        // `calls` holds one row per resolved module call.
        let (from, to) = match direction {
            Direction::DependsOn => ("caller", "callee"),
            Direction::Dependents => ("callee", "caller"),
        };
        let sql = format!(
            "WITH RECURSIVE
                 calls (caller, callee) AS (
                     SELECT c.root_id, t.id
                     FROM module_calls c
                     JOIN roots t ON t.abs_path = c.target_path
                 ),
                 reached (id, depth) AS (
                     SELECT {to}, 1 FROM calls WHERE {from} = :root
                     UNION
                     SELECT calls.{to}, reached.depth + 1
                     FROM reached
                     JOIN calls ON calls.{from} = reached.id
                     WHERE reached.depth < :max_depth
                 )
             SELECT r.id AS root_id, p.name AS repository, r.relative_path, r.classification,
                    MIN(reached.depth) AS depth
             FROM reached
             JOIN roots r ON r.id = reached.id
             JOIN repositories p ON p.repo_key = r.repo_key
             WHERE r.id != :root
             GROUP BY r.id
             ORDER BY depth, p.name, r.relative_path, r.id"
        );
        self.query(
            &sql,
            named_params! { ":root": root, ":max_depth": MAX_DEPTH },
            RelatedDirectory::from_row,
        )
    }

    /// The references between the declarations of `root`, by referrer.
    pub(crate) fn references(&self, root: &RootId) -> Result<Vec<ReferenceRow>> {
        self.query(
            "SELECT source, target, file, line FROM dependencies
             WHERE root_id = ?1
             ORDER BY source, target",
            [root],
            ReferenceRow::from_row,
        )
    }

    /// Whether `address` is a resource, data source or module call declared
    /// in `root`, or either end of a reference there.
    pub(crate) fn has_address(&self, root: &RootId, address: &str) -> Result<bool> {
        Ok(self.connection.query_one(
            "SELECT EXISTS (SELECT 1 FROM resources WHERE root_id = :root AND address = :address)
                 OR EXISTS (SELECT 1 FROM module_calls
                            WHERE root_id = :root AND 'module.' || name = :address)
                 OR EXISTS (SELECT 1 FROM dependencies
                            WHERE root_id = :root AND (source = :address OR target = :address))",
            named_params! { ":root": root, ":address": address },
            |row| row.get(0),
        )?)
    }

    /// The declarations of `root` that `address` refers to, directly or
    /// through other declarations, or the ones that refer to it, nearest
    /// first.
    pub(crate) fn related_addresses(
        &self,
        root: &RootId,
        address: &str,
        direction: Direction,
    ) -> Result<Vec<RelatedAddress>> {
        let (from, to) = match direction {
            Direction::DependsOn => ("source", "target"),
            Direction::Dependents => ("target", "source"),
        };
        let sql = format!(
            "WITH RECURSIVE reached (address, depth) AS (
                 SELECT {to}, 1 FROM dependencies WHERE root_id = :root AND {from} = :address
                 UNION
                 SELECT d.{to}, reached.depth + 1
                 FROM reached
                 JOIN dependencies d ON d.root_id = :root AND d.{from} = reached.address
                 WHERE reached.depth < :max_depth
             )
             SELECT address, MIN(depth) AS depth
             FROM reached
             WHERE address != :address
             GROUP BY address
             ORDER BY depth, address"
        );
        self.query(
            &sql,
            named_params! { ":root": root, ":address": address, ":max_depth": MAX_DEPTH },
            RelatedAddress::from_row,
        )
    }

    /// The directories whose configuration need not be read again as long
    /// as they are where they were and their fingerprint is the same.
    pub(crate) fn known_directories(&self) -> Result<HashMap<RootId, KnownDirectory>> {
        let mut known: HashMap<RootId, KnownDirectory> = self
            .query(
                "SELECT r.id, r.abs_path, r.fingerprint, r.backend_type, r.backend_config,
                        r.has_provider,
                        (EXISTS (SELECT 1 FROM resources x WHERE x.root_id = r.id)
                         OR EXISTS (SELECT 1 FROM module_calls c WHERE c.root_id = r.id))
                            AS declares_infrastructure
                 FROM roots r
                 WHERE r.fingerprint IS NOT NULL",
                [],
                |row| {
                    let backend_type: String = row.get("backend_type")?;
                    let settings = json_column(row, "backend_config")?;
                    let backend = (backend_type != UNKNOWN_BACKEND).then(|| Backend {
                        kind: backend_type,
                        settings: settings.into_iter().collect(),
                    });
                    let directory = KnownDirectory {
                        path: path_column(row, "abs_path")?,
                        fingerprint: row.get("fingerprint")?,
                        outline: Outline {
                            backend,
                            has_provider: row.get("has_provider")?,
                            declares_infrastructure: row.get("declares_infrastructure")?,
                            local_modules: Vec::new(),
                        },
                    };
                    Ok((row.get::<_, RootId>("id")?, directory))
                },
            )?
            .into_iter()
            .collect();

        let local_modules = self.query(
            "SELECT root_id, target_path FROM module_calls
             WHERE target_path IS NOT NULL
             ORDER BY root_id, name, file, line",
            [],
            |row| {
                Ok((
                    row.get::<_, RootId>("root_id")?,
                    path_column(row, "target_path")?,
                ))
            },
        )?;
        for (root, directory) in local_modules {
            if let Some(known) = known.get_mut(&root) {
                known.outline.local_modules.push(directory);
            }
        }
        Ok(known)
    }

    /// Up to `limit` resources whose address or type contains `query`.
    pub(crate) fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        self.query(
            r"SELECT p.name AS repository, r.id AS root_id, r.relative_path, r.abs_path,
                     x.address, x.kind, x.file, x.line
              FROM resources x
              JOIN roots r ON r.id = x.root_id
              JOIN repositories p ON p.repo_key = r.repo_key
              WHERE x.address LIKE :pattern ESCAPE '\' OR x.type LIKE :pattern ESCAPE '\'
              ORDER BY p.name, r.relative_path, x.address, x.file, x.line, r.id
              LIMIT :limit",
            named_params! {
                ":pattern": format!("%{}%", escape_like(query)),
                ":limit": limit,
            },
            SearchHit::from_row,
        )
    }

    /// The state of every workspace of every directory.
    pub(crate) fn state_overview(&self) -> Result<Vec<StateOverviewRow>> {
        self.query(
            "SELECT p.name AS repository, r.id AS root_id, r.relative_path, s.workspace,
                    s.backend_type, s.location, s.status, s.resource_count, s.fetched_at
             FROM states s
             JOIN roots r ON r.id = s.root_id
             JOIN repositories p ON p.repo_key = r.repo_key
             ORDER BY p.name, r.relative_path, s.workspace, r.id",
            [],
            StateOverviewRow::from_row,
        )
    }

    /// Every recorded state, with what `fetch` needs to read it again.
    pub(crate) fn state_targets(&self) -> Result<Vec<StateTarget>> {
        self.query(
            "SELECT p.name AS repository, r.id AS root_id, r.relative_path, s.workspace,
                    s.backend_type, s.location,
                    r.backend_type AS declared_backend_type, r.backend_config
             FROM states s
             JOIN roots r ON r.id = s.root_id
             JOIN repositories p ON p.repo_key = r.repo_key
             ORDER BY p.name, r.relative_path, s.workspace, r.id",
            [],
            StateTarget::from_row,
        )
    }

    fn query<T>(
        &self,
        sql: &str,
        parameters: impl Params,
        from_row: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let rows = statement.query_map(parameters, from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Escape the `LIKE` wildcards in `text`, for use with `ESCAPE '\'`.
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn path_column(row: &Row<'_>, name: &str) -> rusqlite::Result<Utf8PathBuf> {
    row.get::<_, String>(name).map(Utf8PathBuf::from)
}

/// Read a column holding a JSON object.
fn json_column(row: &Row<'_>, name: &str) -> rusqlite::Result<Map<String, Value>> {
    let text: String = row.get(name)?;
    serde_json::from_str(&text).map_err(|error| {
        let index = row.as_ref().column_index(name).unwrap_or_default();
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_wildcards() {
        assert_eq!(escape_like("aws_s3_bucket"), r"aws\_s3\_bucket");
        assert_eq!(escape_like(r"100%\x"), r"100\%\\x");
        assert_eq!(escape_like("plain"), "plain");
    }
}
