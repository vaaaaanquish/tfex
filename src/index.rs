//! The `SQLite` index: the only thing tfex ever writes.
//!
//! Reading is in [`mod@read`] and writing in [`mod@write`]; this module owns
//! the connection, the file location, and the schema.

mod read;
mod write;

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::Connection;

pub(crate) use self::read::{
    Direction, KnownDirectory, ModuleCallRow, ModuleEdge, PlanRow, ReferenceRow, RelatedAddress,
    RelatedDirectory, ResourceRow, RootRow, StateRow, StateTarget,
};
pub(crate) use self::write::{NewRoot, Writer};
use crate::error::{Error, Result};
use crate::paths;

const SCHEMA: &str = include_str!("index/schema.sql");

/// Version of [`SCHEMA`], stored in the database's `user_version`.
const SCHEMA_VERSION: i32 = 2;

/// Backend type recorded for a directory that declares no backend.
const UNKNOWN_BACKEND: &str = "unknown";

/// An open index.
#[derive(Debug)]
pub(crate) struct Index {
    connection: Connection,
}

impl Index {
    /// Where the index is: `explicit` (from `--db` or `TFEX_DB`) if given,
    /// otherwise `tfex/index.sqlite3` under the XDG data directory.
    pub(crate) fn locate(explicit: Option<&Utf8Path>) -> Result<Utf8PathBuf> {
        if let Some(path) = explicit.filter(|path| !path.as_str().is_empty()) {
            return paths::resolve(&paths::expand_user(path));
        }
        let data_home = paths::directory_from_env("XDG_DATA_HOME")
            .or_else(|| paths::home().map(|home| home.join(".local").join("share")))
            .ok_or(Error::IndexLocationUnknown)?;
        Ok(data_home.join("tfex").join("index.sqlite3"))
    }

    /// Open the index at `path`, creating it if it does not exist yet.
    pub(crate) fn open(path: &Utf8Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
        }
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        prepare_schema(&mut connection, path)?;
        Ok(Index { connection })
    }
}

/// Create the schema in a new database, and refuse a database that holds
/// anything else: an index of another version, or an unrelated file.
fn prepare_schema(connection: &mut Connection, path: &Utf8Path) -> Result<()> {
    let found: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found == SCHEMA_VERSION {
        return Ok(());
    }
    let is_empty: bool = connection.query_one(
        "SELECT NOT EXISTS (SELECT 1 FROM sqlite_schema)",
        [],
        |row| row.get(0),
    )?;
    if !is_empty {
        return Err(Error::IncompatibleIndex {
            path: path.to_owned(),
            found,
            supported: SCHEMA_VERSION,
        });
    }
    let transaction = connection.transaction()?;
    transaction.execute_batch(SCHEMA)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn temporary_path(directory: &TempDir, name: &str) -> Utf8PathBuf {
        Utf8Path::from_path(directory.path()).unwrap().join(name)
    }

    #[test]
    fn creates_missing_directories_and_reopens() {
        let directory = TempDir::new().unwrap();
        let path = temporary_path(&directory, "nested/data/index.sqlite3");
        drop(Index::open(&path).unwrap());
        let index = Index::open(&path).unwrap();
        assert_eq!(index.roots(true).unwrap().len(), 0);
    }

    #[test]
    fn rejects_a_database_it_did_not_create() {
        let directory = TempDir::new().unwrap();
        let path = temporary_path(&directory, "other.sqlite3");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE roots (id TEXT)")
            .unwrap();
        let error = Index::open(&path).unwrap_err();
        assert_eq!(error.kind(), "incompatible_index");
    }

    #[test]
    fn rejects_a_newer_schema() {
        let directory = TempDir::new().unwrap();
        let path = temporary_path(&directory, "newer.sqlite3");
        drop(Index::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = Index::open(&path).unwrap_err();
        assert_eq!(error.kind(), "incompatible_index");
    }

    #[test]
    fn explicit_location_wins() {
        let located =
            Index::locate(Some(Utf8Path::new("/nonexistent-tfex/x/../index.db"))).unwrap();
        assert_eq!(located, "/nonexistent-tfex/index.db");
    }
}
