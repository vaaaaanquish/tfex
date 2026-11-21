//! The `fetch` command: read states from where they are.
//!
//! `scan` only reads what is on the local disk. `fetch` is the explicit step
//! that also goes to remote backends, and the only command that uses the
//! network.

use std::collections::HashSet;
use std::time::Duration;

use rayon::prelude::*;
use serde::Serialize;
use serde_json::Map;

use crate::error::Result;
use crate::identity::RootId;
use crate::index::{Index, StateTarget};
use crate::remote;
use crate::state::{StateLocation, StateStatus, StateSummary};

/// What a fetch did.
#[derive(Debug, Default, Serialize)]
pub(crate) struct FetchReport {
    /// States that were read.
    pub(crate) fetched: usize,
    /// States that are at a known location but could not be read from it.
    pub(crate) failed: usize,
    /// States that were not attempted: nothing says where they are, or they
    /// are in a store tfex cannot read. `map-state` can declare a location.
    pub(crate) unresolved: usize,
    /// The states that were attempted.
    pub(crate) states: Vec<FetchedState>,
}

/// The outcome of reading one state.
#[derive(Debug, Serialize)]
pub(crate) struct FetchedState {
    pub(crate) repository: String,
    pub(crate) root_id: RootId,
    pub(crate) relative_path: String,
    pub(crate) workspace: String,
    pub(crate) backend_type: String,
    pub(crate) location: String,
    pub(crate) status: &'static str,
    pub(crate) serial: Option<i64>,
    pub(crate) resource_count: Option<usize>,
    pub(crate) fetched_at: Option<String>,
    pub(crate) error: Option<String>,
}

/// Read the state of every workspace of the directories that `selectors`
/// name, or of every indexed directory when there are none, and record the
/// metadata. Each remote read is given up after `timeout`.
pub(crate) fn fetch(
    index: &mut Index,
    selectors: &[String],
    timeout: Duration,
) -> Result<FetchReport> {
    let targets = targets(index, selectors)?;
    // Reading is mostly waiting for other processes and for the network, so
    // the states are read side by side.
    let outcomes: Vec<Option<(StateLocation, StateSummary)>> = targets
        .par_iter()
        .map(|target| read(target, timeout))
        .collect();

    let mut report = FetchReport::default();
    let writer = index.writer()?;
    for (target, outcome) in targets.into_iter().zip(outcomes) {
        let Some((location, summary)) = outcome else {
            report.unresolved += 1;
            continue;
        };
        writer.put_state(&target.root_id, &target.workspace, &location, &summary)?;
        match summary.status {
            StateStatus::Available => report.fetched += 1,
            StateStatus::Error | StateStatus::Unresolved => report.failed += 1,
        }
        report.states.push(FetchedState {
            repository: target.repository,
            root_id: target.root_id,
            relative_path: target.relative_path,
            workspace: target.workspace,
            backend_type: location.backend_type().to_owned(),
            location: location.as_str().to_owned(),
            status: summary.status.as_str(),
            serial: summary.serial,
            resource_count: summary.resource_count,
            fetched_at: summary.fetched_at,
            error: summary.error,
        });
    }
    writer.commit()?;
    Ok(report)
}

/// The recorded states of the selected directories.
fn targets(index: &Index, selectors: &[String]) -> Result<Vec<StateTarget>> {
    let all = index.state_targets()?;
    if selectors.is_empty() {
        return Ok(all);
    }
    let selected: HashSet<RootId> = selectors
        .iter()
        .map(|selector| Ok(index.find_root(selector)?.id))
        .collect::<Result<_>>()?;
    Ok(all
        .into_iter()
        .filter(|target| selected.contains(&target.root_id))
        .collect())
}

/// Read one state. `None` when it is not known where the state is, or when
/// it is in a store tfex cannot read.
fn read(target: &StateTarget, timeout: Duration) -> Option<(StateLocation, StateSummary)> {
    let location = target.location.clone()?;
    let summary = match &location {
        StateLocation::Local(path) => StateSummary::read(path),
        StateLocation::Remote(uri) => {
            // The directory's backend settings say how to reach its own
            // backend; they mean nothing for a state mapped to another one.
            let unrelated = Map::new();
            let settings = if target.declared_backend_type == location.backend_type() {
                &target.declared_backend
            } else {
                &unrelated
            };
            remote::read(uri, settings, timeout)?
        }
    };
    Some((location, summary))
}
