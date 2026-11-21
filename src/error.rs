//! The error type shared by every tfex operation.

use std::io;

use camino::{Utf8Path, Utf8PathBuf};

/// Result alias used throughout the crate.
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can make a tfex command fail.
///
/// Problems confined to a single Terraform file, state file, or Git call are
/// not errors: they are recorded in the index or the output instead, so that
/// one bad input never hides the rest.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("Not a directory: {0}")]
    NotADirectory(Utf8PathBuf),

    #[error("Root not found: {0}; run 'roots' first")]
    RootNotFound(String),

    #[error("Ambiguous root: {0}; use its ID")]
    AmbiguousRoot(String),

    #[error("Nothing in {root} is declared or referred to as {address}; see 'graph {root}'")]
    AddressNotFound { address: String, root: String },

    #[error("Unsupported state URI scheme: {0}")]
    UnsupportedStateScheme(String),

    #[error("Cannot locate the index: set --db, TFEX_DB, XDG_DATA_HOME, or HOME")]
    IndexLocationUnknown,

    #[error(
        "{path} is not an index this version can use (schema version {found}, expected \
         {supported}); delete it and scan again"
    )]
    IncompatibleIndex {
        path: Utf8PathBuf,
        found: i32,
        supported: i32,
    },

    #[error("{path}: {source}")]
    Io {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("Cannot write output: {0}")]
    Output(#[source] io::Error),

    #[error(transparent)]
    Index(#[from] rusqlite::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// An I/O failure on `path`.
    pub(crate) fn io(path: &Utf8Path, source: io::Error) -> Self {
        Error::Io {
            path: path.to_owned(),
            source,
        }
    }

    /// Stable machine-readable name of the failure, reported next to the
    /// human-readable message.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Error::NotADirectory(_) => "not_a_directory",
            Error::RootNotFound(_) => "root_not_found",
            Error::AmbiguousRoot(_) => "ambiguous_root",
            Error::AddressNotFound { .. } => "address_not_found",
            Error::UnsupportedStateScheme(_) => "unsupported_state_scheme",
            Error::IndexLocationUnknown => "index_location_unknown",
            Error::IncompatibleIndex { .. } => "incompatible_index",
            Error::Io { .. } | Error::Output(_) => "io",
            Error::Index(_) => "index",
            Error::Json(_) => "json",
        }
    }
}
