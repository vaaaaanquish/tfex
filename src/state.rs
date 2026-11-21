//! Terraform state: where it lives, and the little that is recorded about it.
//!
//! A state can contain credentials in plain text, so only metadata (serial,
//! lineage, Terraform version, number of managed resource instances) is ever
//! read out of a state. Resource attributes and outputs are skipped by the
//! deserializer and never stored or printed.

use std::collections::BTreeMap;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;
use serde_json::error::Category;

use crate::config::{self, Backend};
use crate::error::{Error, Result};
use crate::{paths, time};

/// The workspace every Terraform directory starts with.
pub(crate) const DEFAULT_WORKSPACE: &str = "default";

const STATE_FILE: &str = "terraform.tfstate";
const WORKSPACES_DIRECTORY: &str = "terraform.tfstate.d";
const LOCAL_BACKEND: &str = "local";

/// URI schemes of the remote locations that can be registered with
/// `map-state`, each with the Terraform backend that stores state there.
const REMOTE_SCHEMES: [(&str, &str); 4] = [
    ("s3", "s3"),
    ("gs", "gcs"),
    ("azurerm", "azurerm"),
    ("remote", "remote"),
];

/// The workspace key prefix of the `s3` backend when none is configured.
const S3_DEFAULT_WORKSPACE_KEY_PREFIX: &str = "env:";

/// Whether `directory` holds a state written by the local backend.
pub(crate) fn has_local_state(directory: &Utf8Path) -> bool {
    directory.join(STATE_FILE).is_file() || directory.join(WORKSPACES_DIRECTORY).is_dir()
}

/// A state file of the local backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalState {
    pub(crate) workspace: String,
    pub(crate) path: Utf8PathBuf,
}

/// The local state files of `directory`: the default workspace first, then
/// the named workspaces in name order.
///
/// A directory whose backend is remote has none; whatever state file happens
/// to lie around there is not what Terraform uses.
pub(crate) fn local_states(directory: &Utf8Path, backend: Option<&Backend>) -> Vec<LocalState> {
    if backend.is_some_and(|backend| !backend.is_local()) {
        return Vec::new();
    }
    let custom_path = backend
        .and_then(|backend| backend.settings.get("path"))
        .and_then(Value::as_str);
    let default = match custom_path {
        Some(path) => paths::canonical(&directory.join(path)),
        None => directory.join(STATE_FILE),
    };

    let mut states = Vec::new();
    if default.is_file() {
        states.push(LocalState {
            workspace: DEFAULT_WORKSPACE.to_owned(),
            path: default,
        });
    }
    states.extend(named_workspace_states(directory));
    states
}

fn named_workspace_states(directory: &Utf8Path) -> Vec<LocalState> {
    let Ok(entries) = directory.join(WORKSPACES_DIRECTORY).read_dir_utf8() else {
        return Vec::new();
    };
    let mut states: Vec<LocalState> = entries
        .flatten()
        .map(|entry| LocalState {
            workspace: entry.file_name().to_owned(),
            path: entry.path().join(STATE_FILE),
        })
        .filter(|state| state.path.is_file())
        .collect();
    states.sort_unstable_by(|left, right| left.workspace.cmp(&right.workspace));
    states
}

/// Where the state of a workspace is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateLocation {
    /// A state file on the local disk.
    Local(Utf8PathBuf),
    /// The URI of an object in a remote store. Only `tfex fetch` reads it.
    Remote(String),
}

impl StateLocation {
    /// Interpret a user-supplied location: a URI with a supported scheme, or
    /// otherwise a local path.
    pub(crate) fn parse(location: &str) -> Result<Self> {
        match location.split_once("://") {
            Some((scheme, _)) if backend_of_scheme(scheme).is_some() => {
                Ok(StateLocation::Remote(location.to_owned()))
            }
            Some((scheme, _)) => Err(Error::UnsupportedStateScheme(scheme.to_owned())),
            None => {
                let path = paths::expand_user(Utf8Path::new(location));
                Ok(StateLocation::Local(paths::resolve(&path)?))
            }
        }
    }

    /// Rebuild a location from the two columns it is stored in.
    pub(crate) fn from_stored(backend_type: &str, location: String) -> Self {
        if backend_type == LOCAL_BACKEND {
            StateLocation::Local(Utf8PathBuf::from(location))
        } else {
            StateLocation::Remote(location)
        }
    }

    /// The Terraform backend that keeps a state at this location.
    pub(crate) fn backend_type(&self) -> &str {
        match self {
            StateLocation::Local(_) => LOCAL_BACKEND,
            StateLocation::Remote(uri) => {
                let scheme = uri
                    .split_once("://")
                    .map_or(uri.as_str(), |(scheme, _)| scheme);
                backend_of_scheme(scheme).unwrap_or(scheme)
            }
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            StateLocation::Local(path) => path.as_str(),
            StateLocation::Remote(uri) => uri,
        }
    }

    /// Summarize the state without contacting a remote: a local file is read,
    /// and a remote state is unresolved.
    pub(crate) fn summarize_offline(&self) -> StateSummary {
        match self {
            StateLocation::Local(path) => StateSummary::read(path),
            StateLocation::Remote(_) => StateSummary::unresolved(),
        }
    }
}

fn backend_of_scheme(scheme: &str) -> Option<&'static str> {
    REMOTE_SCHEMES
        .iter()
        .find(|(known, _)| *known == scheme)
        .map(|(_, backend)| *backend)
}

/// Where the state of each workspace of `directory` is, by workspace name;
/// `None` for a workspace whose state cannot be located.
///
/// In increasing order of authority:
///
/// 1. the state files of the local backend found in the directory;
/// 2. for a remote backend, where its settings put the default workspace;
/// 3. `mappings`, the locations declared with `map-state`.
///
/// A directory for which nothing is found still has its default workspace,
/// unlocated, so that "not found" is visible as such.
pub(crate) fn locate(
    directory: &Utf8Path,
    backend: Option<&Backend>,
    mappings: impl IntoIterator<Item = (String, StateLocation)>,
) -> BTreeMap<String, Option<StateLocation>> {
    let mut located = BTreeMap::new();
    for local in local_states(directory, backend) {
        located.insert(local.workspace, Some(StateLocation::Local(local.path)));
    }
    if let Some(backend) = backend.filter(|backend| !backend.is_local()) {
        located.insert(
            DEFAULT_WORKSPACE.to_owned(),
            backend_location(backend, DEFAULT_WORKSPACE),
        );
    }
    for (workspace, location) in mappings {
        located.insert(workspace, Some(location));
    }
    if located.is_empty() {
        located.insert(DEFAULT_WORKSPACE.to_owned(), None);
    }
    located
}

/// Where `backend` keeps the state of `workspace`, following the object
/// naming of the `gcs` and `s3` backends.
///
/// `None` when the backend is of another type, or when its settings do not
/// say: a setting is missing (it is injected with `-backend-config`) or is
/// not a plain literal.
fn backend_location(backend: &Backend, workspace: &str) -> Option<StateLocation> {
    let setting = |name: &str| backend.settings.get(name).and_then(Value::as_str);
    if backend
        .settings
        .values()
        .filter_map(Value::as_str)
        .any(config::is_template)
    {
        return None;
    }
    let uri = match backend.kind.as_str() {
        "gcs" => {
            let object = format!("{workspace}.tfstate");
            let prefix = setting("prefix").unwrap_or_default();
            format!(
                "gs://{}/{}",
                setting("bucket")?,
                join_key(&[prefix, &object])
            )
        }
        "s3" => {
            let key = setting("key")?;
            let object = if workspace == DEFAULT_WORKSPACE {
                join_key(&[key])
            } else {
                let prefix =
                    setting("workspace_key_prefix").unwrap_or(S3_DEFAULT_WORKSPACE_KEY_PREFIX);
                join_key(&[prefix, workspace, key])
            };
            format!("s3://{}/{object}", setting("bucket")?)
        }
        _ => return None,
    };
    Some(StateLocation::Remote(uri))
}

/// Join the non-empty segments of an object key with single slashes.
fn join_key(segments: &[&str]) -> String {
    segments
        .iter()
        .map(|segment| segment.trim_matches('/'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a state could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateStatus {
    /// The state was read and summarized.
    Available,
    /// The state is known to be at a location but could not be read from it.
    Error,
    /// The state was not read: it is in a remote backend and was not fetched,
    /// or nothing says where it is. This says nothing about whether it exists.
    Unresolved,
}

impl StateStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StateStatus::Available => "available",
            StateStatus::Error => "error",
            StateStatus::Unresolved => "unresolved",
        }
    }
}

/// What is recorded about one state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateSummary {
    pub(crate) status: StateStatus,
    pub(crate) serial: Option<i64>,
    pub(crate) lineage: Option<String>,
    pub(crate) terraform_version: Option<String>,
    /// Number of managed resource instances.
    pub(crate) resource_count: Option<usize>,
    /// When the state file was read.
    pub(crate) fetched_at: Option<String>,
    pub(crate) error: Option<String>,
}

impl StateSummary {
    /// The summary of a state that was not read.
    pub(crate) fn unresolved() -> Self {
        StateSummary {
            status: StateStatus::Unresolved,
            serial: None,
            lineage: None,
            terraform_version: None,
            resource_count: None,
            fetched_at: None,
            error: None,
        }
    }

    /// Read the state file at `path`. A missing or malformed file yields a
    /// summary with [`StateStatus::Error`] rather than a failure.
    pub(crate) fn read(path: &Utf8Path) -> Self {
        Self::from_contents(fs::read(path).map_err(|error| error.to_string()))
    }

    /// Summarize the contents of a state, or record why they could not be
    /// obtained. The contents are dropped once the metadata is extracted.
    pub(crate) fn from_contents(contents: Result<Vec<u8>, String>) -> Self {
        let fetched_at = Some(time::now());
        match contents.and_then(|bytes| StateFile::parse(&bytes)) {
            Ok(state) => StateSummary {
                status: StateStatus::Available,
                resource_count: Some(state.managed_instances()),
                serial: state.serial,
                lineage: state.lineage,
                terraform_version: state.terraform_version,
                fetched_at,
                error: None,
            },
            Err(error) => StateSummary {
                status: StateStatus::Error,
                fetched_at,
                error: Some(error),
                ..StateSummary::unresolved()
            },
        }
    }
}

/// The parts of a state file that are safe to keep.
///
/// Fields that are not listed, notably `outputs`, are skipped while parsing.
#[derive(Debug, Deserialize)]
struct StateFile {
    serial: Option<i64>,
    lineage: Option<String>,
    terraform_version: Option<String>,
    #[serde(default)]
    resources: Vec<StateResource>,
}

#[derive(Debug, Deserialize)]
struct StateResource {
    /// `managed` or `data`; absent in some old states, which only had managed
    /// resources.
    mode: Option<String>,
    /// Instance contents are skipped; only their number matters.
    #[serde(default)]
    instances: Vec<IgnoredAny>,
}

impl StateFile {
    /// Parse a state, describing any failure without quoting its contents.
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| describe(&error))
    }

    fn managed_instances(&self) -> usize {
        self.resources
            .iter()
            .filter(|resource| {
                resource
                    .mode
                    .as_deref()
                    .is_none_or(|mode| mode == "managed")
            })
            .map(|resource| resource.instances.len())
            .sum()
    }
}

/// Where and roughly why parsing failed.
///
/// The message of a `serde_json` error can quote the offending value, which
/// in a state file may be a secret, so it is not used.
fn describe(error: &serde_json::Error) -> String {
    let problem = match error.classify() {
        Category::Io => "cannot be read",
        Category::Syntax => "is not valid JSON",
        Category::Data => "does not have the structure of a state file",
        Category::Eof => "ends unexpectedly",
    };
    format!(
        "state {problem} (line {}, column {})",
        error.line(),
        error.column()
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    fn temporary_directory() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().unwrap();
        let path = paths::resolve(Utf8Path::from_path(directory.path()).unwrap()).unwrap();
        (directory, path)
    }

    fn write(path: &Utf8Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn backend(kind: &str, settings: &[(&str, &str)]) -> Backend {
        Backend {
            kind: kind.to_owned(),
            settings: settings
                .iter()
                .map(|(name, value)| ((*name).to_owned(), json!(value)))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn summary_holds_metadata_only() {
        let (_guard, directory) = temporary_directory();
        let path = directory.join(STATE_FILE);
        let state = json!({
            "version": 4,
            "terraform_version": "1.9.0",
            "serial": 7,
            "lineage": "3f8a",
            "outputs": {"password": {"value": "SENSITIVE", "type": "string"}},
            "resources": [
                {"mode": "managed", "type": "aws_s3_bucket", "name": "logs",
                 "instances": [{"attributes": {"secret": "SENSITIVE"}}, {"attributes": {}}]},
                {"mode": "data", "type": "aws_region", "name": "current", "instances": [{}]},
                {"type": "legacy", "name": "no_mode", "instances": [{}]},
            ],
        });
        write(&path, &state.to_string());

        let summary = StateSummary::read(&path);
        assert_eq!(summary.status, StateStatus::Available);
        assert_eq!(summary.serial, Some(7));
        assert_eq!(summary.lineage.as_deref(), Some("3f8a"));
        assert_eq!(summary.terraform_version.as_deref(), Some("1.9.0"));
        assert_eq!(summary.resource_count, Some(3), "data sources do not count");
        assert!(summary.fetched_at.is_some());
        assert!(!format!("{summary:?}").contains("SENSITIVE"));
    }

    #[test]
    fn unreadable_state_is_an_error_summary_without_contents() {
        let (_guard, directory) = temporary_directory();
        let missing = StateSummary::read(&directory.join("missing.tfstate"));
        assert_eq!(missing.status, StateStatus::Error);
        assert!(missing.error.is_some());

        let malformed_path = directory.join("malformed.tfstate");
        write(
            &malformed_path,
            r#"{"serial": "SENSITIVE", "resources": []}"#,
        );
        let malformed = StateSummary::read(&malformed_path);
        assert_eq!(malformed.status, StateStatus::Error);
        assert_eq!(
            malformed.error.as_deref(),
            Some("state does not have the structure of a state file (line 1, column 22)")
        );

        write(&malformed_path, "[]");
        assert_eq!(
            StateSummary::read(&malformed_path).status,
            StateStatus::Error
        );
    }

    #[test]
    fn local_states_cover_default_and_named_workspaces() {
        let (_guard, directory) = temporary_directory();
        assert!(!has_local_state(&directory));
        assert_eq!(local_states(&directory, None), []);

        write(&directory.join("terraform.tfstate"), "{}");
        write(
            &directory.join("terraform.tfstate.d/stg/terraform.tfstate"),
            "{}",
        );
        write(
            &directory.join("terraform.tfstate.d/dev/terraform.tfstate"),
            "{}",
        );
        fs::create_dir_all(directory.join("terraform.tfstate.d/empty")).unwrap();

        assert!(has_local_state(&directory));
        let workspaces: Vec<String> = local_states(&directory, None)
            .into_iter()
            .map(|state| state.workspace)
            .collect();
        assert_eq!(workspaces, ["default", "dev", "stg"]);
        assert_eq!(
            local_states(&directory, Some(&backend("local", &[]))).len(),
            3
        );
    }

    #[test]
    fn remote_backend_has_no_local_states() {
        let (_guard, directory) = temporary_directory();
        write(&directory.join("terraform.tfstate"), "{}");
        assert_eq!(local_states(&directory, Some(&backend("gcs", &[]))), []);
    }

    #[test]
    fn local_backend_path_overrides_the_default_file() {
        let (_guard, directory) = temporary_directory();
        write(&directory.join("terraform.tfstate"), "{}");
        write(&directory.join("state/custom.tfstate"), "{}");
        let custom = backend("local", &[("path", "state/custom.tfstate")]);
        assert_eq!(
            local_states(&directory, Some(&custom)),
            [LocalState {
                workspace: "default".to_owned(),
                path: directory.join("state/custom.tfstate"),
            }]
        );
    }

    #[test]
    fn parses_state_locations() {
        let remote = StateLocation::parse("gs://bucket/prod.tfstate").unwrap();
        assert_eq!(remote.backend_type(), "gcs");
        assert_eq!(remote.as_str(), "gs://bucket/prod.tfstate");
        assert_eq!(remote.summarize_offline().status, StateStatus::Unresolved);

        let local = StateLocation::parse("/nonexistent-tfex/a/../x.tfstate").unwrap();
        assert_eq!(local.backend_type(), "local");
        assert_eq!(local.as_str(), "/nonexistent-tfex/x.tfstate");
        assert_eq!(local.summarize_offline().status, StateStatus::Error);

        let unsupported = StateLocation::parse("ftp://host/x.tfstate").unwrap_err();
        assert_eq!(unsupported.kind(), "unsupported_state_scheme");
    }

    #[test]
    fn stored_locations_round_trip() {
        for location in ["s3://bucket/prod.tfstate", "/srv/state/x.tfstate"] {
            let parsed = StateLocation::parse(location).unwrap();
            let restored =
                StateLocation::from_stored(parsed.backend_type(), parsed.as_str().to_owned());
            assert_eq!(restored, parsed);
        }
    }

    fn remote(uri: &str) -> StateLocation {
        StateLocation::Remote(uri.to_owned())
    }

    #[test]
    fn gcs_backend_names_objects_after_workspaces() {
        let with_prefix = backend("gcs", &[("bucket", "state"), ("prefix", "/envs/prod/")]);
        assert_eq!(
            backend_location(&with_prefix, "default"),
            Some(remote("gs://state/envs/prod/default.tfstate"))
        );
        assert_eq!(
            backend_location(&with_prefix, "staging"),
            Some(remote("gs://state/envs/prod/staging.tfstate"))
        );
        let without_prefix = backend("gcs", &[("bucket", "state")]);
        assert_eq!(
            backend_location(&without_prefix, "default"),
            Some(remote("gs://state/default.tfstate"))
        );
    }

    #[test]
    fn s3_backend_prefixes_named_workspaces() {
        let plain = backend(
            "s3",
            &[("bucket", "state"), ("key", "prod/terraform.tfstate")],
        );
        assert_eq!(
            backend_location(&plain, "default"),
            Some(remote("s3://state/prod/terraform.tfstate"))
        );
        assert_eq!(
            backend_location(&plain, "staging"),
            Some(remote("s3://state/env:/staging/prod/terraform.tfstate"))
        );
        let custom = backend(
            "s3",
            &[
                ("bucket", "state"),
                ("key", "app.tfstate"),
                ("workspace_key_prefix", "workspaces"),
            ],
        );
        assert_eq!(
            backend_location(&custom, "staging"),
            Some(remote("s3://state/workspaces/staging/app.tfstate"))
        );
    }

    #[test]
    fn incomplete_or_dynamic_backend_settings_locate_nothing() {
        for backend in [
            backend("gcs", &[]),
            backend("gcs", &[("prefix", "envs/prod")]),
            backend("gcs", &[("bucket", "state-${var.env}")]),
            backend("s3", &[("bucket", "state")]),
            backend(
                "s3",
                &[("bucket", "state"), ("key", "%{ if true }a%{ endif }")],
            ),
            backend("azurerm", &[("container_name", "state"), ("key", "x")]),
        ] {
            assert_eq!(backend_location(&backend, "default"), None, "{backend:?}");
        }
    }

    #[test]
    fn locating_prefers_mappings_then_the_backend_then_files() {
        let (_guard, directory) = temporary_directory();
        write(&directory.join("terraform.tfstate"), "{}");
        let local = Some(StateLocation::Local(directory.join("terraform.tfstate")));
        let mapped = StateLocation::Remote("s3://elsewhere/real.tfstate".to_owned());

        // No backend: the state file in the directory.
        let located = locate(&directory, None, []);
        assert_eq!(located, [("default".to_owned(), local)].into());

        // A remote backend: where its settings say, not the stray file.
        let gcs = backend("gcs", &[("bucket", "state")]);
        let located = locate(&directory, Some(&gcs), []);
        assert_eq!(
            located,
            [(
                "default".to_owned(),
                Some(remote("gs://state/default.tfstate"))
            )]
            .into()
        );

        // A mapping wins for its workspace and adds others.
        let mappings = [
            ("default".to_owned(), mapped.clone()),
            ("staging".to_owned(), mapped.clone()),
        ];
        let located = locate(&directory, Some(&gcs), mappings);
        assert_eq!(
            located,
            [
                ("default".to_owned(), Some(mapped.clone())),
                ("staging".to_owned(), Some(mapped)),
            ]
            .into()
        );
    }

    #[test]
    fn a_directory_without_any_state_has_an_unlocated_default_workspace() {
        let (_guard, directory) = temporary_directory();
        let unlocated: BTreeMap<String, Option<StateLocation>> =
            [("default".to_owned(), None)].into();
        assert_eq!(locate(&directory, None, []), unlocated);
        assert_eq!(
            locate(&directory, Some(&backend("gcs", &[])), []),
            unlocated
        );
    }

    #[test]
    fn summarizes_contents_however_they_were_obtained() {
        let fetched =
            StateSummary::from_contents(Ok(br#"{"serial": 3, "resources": []}"#.to_vec()));
        assert_eq!(fetched.status, StateStatus::Available);
        assert_eq!(fetched.serial, Some(3));
        assert_eq!(fetched.resource_count, Some(0));

        let failed = StateSummary::from_contents(Err("access denied".to_owned()));
        assert_eq!(failed.status, StateStatus::Error);
        assert_eq!(failed.error.as_deref(), Some("access denied"));
        assert!(failed.fetched_at.is_some());
    }
}
