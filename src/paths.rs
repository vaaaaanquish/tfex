//! Path helpers.
//!
//! Paths are stored in `SQLite` and printed as JSON, so the crate works with
//! UTF-8 paths throughout; anything else is skipped at the boundary where it
//! is discovered.

use std::env;

use camino::{FromPathBufError, Utf8Component, Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};

/// The user's home directory, when `HOME` is set.
pub(crate) fn home() -> Option<Utf8PathBuf> {
    directory_from_env("HOME")
}

/// A non-empty path taken from an environment variable.
pub(crate) fn directory_from_env(variable: &str) -> Option<Utf8PathBuf> {
    env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
        .map(Utf8PathBuf::from)
}

/// Expand a leading `~` to the home directory.
pub(crate) fn expand_user(path: &Utf8Path) -> Utf8PathBuf {
    match (path.strip_prefix("~"), home()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_owned(),
    }
}

/// Canonical absolute form of `path`, which may be relative to the current
/// directory. See [`canonical`].
pub(crate) fn resolve(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        Ok(canonical(path))
    } else {
        Ok(canonical(&current_directory()?.join(path)))
    }
}

/// Canonical form of an absolute path: symlinks are resolved when the path
/// exists, otherwise `.` and `..` are removed lexically so that the result is
/// also usable for files that are yet to be created.
pub(crate) fn canonical(absolute: &Utf8Path) -> Utf8PathBuf {
    absolute
        .canonicalize_utf8()
        .unwrap_or_else(|_| normalize(absolute))
}

fn current_directory() -> Result<Utf8PathBuf> {
    env::current_dir()
        .and_then(|directory| {
            Utf8PathBuf::try_from(directory).map_err(FromPathBufError::into_io_error)
        })
        .map_err(|source| Error::io(Utf8Path::new("."), source))
}

fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut normalized = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// `path` relative to `base` with `/` separators, or `.` when they are equal.
/// `None` when `path` is not inside `base`.
pub(crate) fn relative_to(path: &Utf8Path, base: &Utf8Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let parts: Vec<&str> = relative.components().map(|part| part.as_str()).collect();
    Some(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_normalizes_paths_that_do_not_exist() {
        let resolved = resolve(Utf8Path::new("/nonexistent-tfex/a/./b/../c")).unwrap();
        assert_eq!(resolved, "/nonexistent-tfex/a/c");
    }

    #[test]
    fn resolve_makes_relative_paths_absolute() {
        let resolved = resolve(Utf8Path::new("nonexistent-tfex-relative")).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("nonexistent-tfex-relative"));
    }

    #[test]
    fn relative_to_uses_dot_for_the_base_itself() {
        let base = Utf8Path::new("/repo");
        assert_eq!(relative_to(base, base).as_deref(), Some("."));
        assert_eq!(
            relative_to(Utf8Path::new("/repo/envs/prod"), base).as_deref(),
            Some("envs/prod")
        );
        assert_eq!(relative_to(Utf8Path::new("/elsewhere"), base), None);
    }

    #[test]
    fn expand_user_only_touches_a_leading_tilde() {
        let untouched = Utf8Path::new("/srv/~/state");
        assert_eq!(expand_user(untouched), untouched);
        if let Some(home) = home() {
            assert_eq!(expand_user(Utf8Path::new("~/x")), home.join("x"));
            assert_eq!(expand_user(Utf8Path::new("~")), home);
        }
    }
}
