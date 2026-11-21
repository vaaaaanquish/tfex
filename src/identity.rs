//! Stable identifiers for repositories and Terraform directories.
//!
//! Identifiers are derived from what a repository *is* (its origin remote)
//! rather than where it happens to be checked out, so they stay the same
//! across rescans, machines, and clones of the same repository.

use std::fmt::{self, Write as _};

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::Serialize;
use sha2::{Digest, Sha256};

const REPO_KEY_LENGTH: usize = 16;
const ROOT_ID_LENGTH: usize = 12;

/// Identifies a repository: a hash of its normalized origin remote, or of its
/// location on disk when it has none.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct RepoKey(String);

impl RepoKey {
    /// Derive the key from a remote URL or a directory path.
    pub(crate) fn derive(origin: &str) -> Self {
        let trimmed = origin.trim_end_matches('/');
        let normalized = trimmed.strip_suffix(".git").unwrap_or(trimmed);
        RepoKey(sha256_prefix(normalized, REPO_KEY_LENGTH))
    }
}

/// Identifies a Terraform directory: a hash of its repository and its path
/// inside that repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct RootId(String);

impl RootId {
    /// Derive the identifier of the directory at `relative_path` in `repository`.
    pub(crate) fn derive(repository: &RepoKey, relative_path: &str) -> Self {
        RootId(sha256_prefix(
            &format!("{repository}\0{relative_path}"),
            ROOT_ID_LENGTH,
        ))
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Display for RootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl ToSql for RepoKey {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl ToSql for RootId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for RootId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        String::column_result(value).map(RootId)
    }
}

/// First `length` hexadecimal digits of the SHA-256 of `input`.
pub(crate) fn sha256_prefix(input: &str, length: usize) -> String {
    let mut hex = String::with_capacity(Sha256::output_size() * 2);
    for byte in Sha256::digest(input.as_bytes()) {
        // Writing to a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex.truncate(length);
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_sha256_prefixes() {
        assert_eq!(sha256_prefix("abc", 16), "ba7816bf8f01cfea");
    }

    #[test]
    fn repo_key_ignores_trailing_slash_and_dot_git() {
        let key = RepoKey::derive("https://github.com/acme/infra");
        assert_eq!(key, RepoKey::derive("https://github.com/acme/infra.git"));
        assert_eq!(key, RepoKey::derive("https://github.com/acme/infra/"));
        assert_ne!(key, RepoKey::derive("https://github.com/acme/other"));
        assert_eq!(key.to_string().len(), REPO_KEY_LENGTH);
    }

    #[test]
    fn root_id_depends_on_repository_and_path() {
        let repository = RepoKey::derive("https://github.com/acme/infra");
        let other = RepoKey::derive("https://github.com/acme/other");
        let root = RootId::derive(&repository, "envs/prod");
        assert_eq!(root, RootId::derive(&repository, "envs/prod"));
        assert_ne!(root, RootId::derive(&repository, "envs/dev"));
        assert_ne!(root, RootId::derive(&other, "envs/prod"));
        assert_eq!(root.to_string().len(), ROOT_ID_LENGTH);
    }
}
