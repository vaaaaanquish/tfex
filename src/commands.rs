//! The commands that look at, or annotate, one indexed Terraform directory.

use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::Result;
use crate::git::{self, Commit};
use crate::identity::RootId;
use crate::index::{Index, ModuleCallRow, ResourceRow, StateRow};
use crate::remote;
use crate::state::StateLocation;

/// Everything known about one Terraform directory.
#[derive(Debug, Serialize)]
pub(crate) struct Inspection {
    pub(crate) id: RootId,
    pub(crate) repository: String,
    pub(crate) relative_path: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) classification: String,
    pub(crate) reason: String,
    pub(crate) backend_type: String,
    /// The backend settings that locate the state; never credentials.
    pub(crate) backend: Map<String, Value>,
    pub(crate) indexed_at: String,
    pub(crate) resources: Vec<ResourceRow>,
    pub(crate) module_calls: Vec<ModuleCallRow>,
    pub(crate) states: Vec<StateRow>,
    /// The latest commit that touched the directory.
    pub(crate) last_commit: Option<Commit>,
}

/// `inspect`: the configuration, states and latest commit of a directory.
pub(crate) fn inspect(index: &Index, selector: &str) -> Result<Inspection> {
    let root = index.find_root(selector)?;
    let last_commit = git::history(&root.checkout, &root.relative_path, 1).pop();
    Ok(Inspection {
        resources: index.resources(&root.id)?,
        module_calls: index.module_calls(&root.id)?,
        states: index.states(&root.id)?,
        last_commit,
        id: root.id,
        repository: root.repository,
        relative_path: root.relative_path,
        path: root.path,
        classification: root.classification,
        reason: root.reason,
        backend_type: root.backend_type,
        backend: root.backend,
        indexed_at: root.indexed_at,
    })
}

/// `log`: the latest commits that touched a directory, newest first.
pub(crate) fn log(index: &Index, selector: &str, limit: u32) -> Result<Vec<Commit>> {
    let root = index.find_root(selector)?;
    Ok(git::history(&root.checkout, &root.relative_path, limit))
}

/// The states of one Terraform directory.
#[derive(Debug, Serialize)]
pub(crate) struct RootStatus {
    /// The selector the directory was asked for with.
    pub(crate) root: String,
    pub(crate) states: Vec<StateRow>,
}

/// `status <root>`: the states of a directory.
pub(crate) fn status(index: &Index, selector: &str) -> Result<RootStatus> {
    let root = index.find_root(selector)?;
    Ok(RootStatus {
        root: selector.to_owned(),
        states: index.states(&root.id)?,
    })
}

/// The outcome of registering a state location.
#[derive(Debug, Serialize)]
pub(crate) struct StateMappingReport {
    pub(crate) root_id: RootId,
    pub(crate) workspace: String,
    pub(crate) backend_type: String,
    pub(crate) location: String,
    pub(crate) status: &'static str,
    pub(crate) note: Option<&'static str>,
}

/// `map-state`: declare where the state of a workspace of a directory is.
///
/// The mapping outlives rescans and takes precedence over what the directory
/// itself says. A local state is summarized right away; a remote one is
/// recorded as unresolved until it is fetched.
pub(crate) fn map_state(
    index: &mut Index,
    selector: &str,
    workspace: &str,
    location: &str,
) -> Result<StateMappingReport> {
    let root = index.find_root(selector)?;
    let location = StateLocation::parse(location)?;
    let summary = location.summarize_offline();

    let writer = index.writer()?;
    writer.put_state_mapping(&root.id, workspace, &location)?;
    writer.put_state(&root.id, workspace, &location, &summary)?;
    writer.commit()?;

    let note = match &location {
        StateLocation::Local(_) => None,
        StateLocation::Remote(uri) if remote::is_readable(uri) => {
            Some("Run 'tfex fetch' to read this state")
        }
        StateLocation::Remote(_) => Some("tfex cannot read states from this kind of store"),
    };
    Ok(StateMappingReport {
        root_id: root.id,
        workspace: workspace.to_owned(),
        backend_type: location.backend_type().to_owned(),
        location: location.as_str().to_owned(),
        status: summary.status.as_str(),
        note,
    })
}
