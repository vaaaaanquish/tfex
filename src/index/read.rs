//! Reading from the index.
//!
//! The row types here are what the commands print: their field order is the
//! JSON key order, and every query has a total order so that output is
//! deterministic.

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::types::Type;
use rusqlite::{Params, Row, named_params};
use serde::Serialize;
use serde_json::{Map, Value};

use super::Index;
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
