//! The `plan` command: ask Terraform what it would change.
//!
//! This is the only place where tfex runs Terraform, and it only happens
//! when `tfex plan` is asked for by name. `terraform plan` is run in the
//! directory as it is: tfex does not run `terraform init`, does not take the
//! state lock, and saves no plan file. With `--refresh-only` the plan is a
//! drift check: it reports what changed outside Terraform since the state
//! was written, not what the configuration would change.
//!
//! A plan can show secrets, so, as with state, only addresses, actions and
//! counts are read out of Terraform's output. Planned values are skipped by
//! the deserializer and never stored or printed.

use std::collections::HashSet;
use std::fs;
use std::process::Command;
use std::time::Duration;

use camino::Utf8Path;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::RootId;
use crate::index::{Index, RootRow};
use crate::process::{self, Failure, Finished};
use crate::state::DEFAULT_WORKSPACE;
use crate::time;

/// Longest error text, in characters, kept from Terraform's output.
const ERROR_LIMIT: usize = 300;

/// Where Terraform records the selected workspace of a directory.
const SELECTED_WORKSPACE_FILE: &str = ".terraform/environment";

/// What kind of plan to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// What applying the configuration would change.
    Plan,
    /// What changed outside Terraform: `terraform plan -refresh-only`.
    RefreshOnly,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::RefreshOnly => "refresh-only",
        }
    }
}

/// How to run Terraform.
#[derive(Debug)]
pub(crate) struct Options {
    pub(crate) mode: Mode,
    /// The program to run: `terraform`, `tofu`, or a path to either.
    pub(crate) program: String,
    /// The workspace to plan, instead of the one selected in the directory.
    pub(crate) workspace: Option<String>,
    /// How long one plan may take.
    pub(crate) timeout: Duration,
}

/// What a plan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanStatus {
    /// Nothing to change, and (for a drift check) nothing drifted.
    NoChanges,
    /// Applying the configuration would change something.
    Changes,
    /// A drift check found resources changed outside Terraform.
    Drift,
    /// Terraform could not make the plan.
    Error,
}

impl PlanStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PlanStatus::NoChanges => "no_changes",
            PlanStatus::Changes => "changes",
            PlanStatus::Drift => "drift",
            PlanStatus::Error => "error",
        }
    }
}

/// One resource instance Terraform would change, or found changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResourceChange {
    /// The full address of the instance, such as
    /// `module.network.aws_subnet.main[0]`.
    pub(crate) address: String,
    /// Terraform's name for the change: `create`, `update`, `delete`,
    /// `replace`, `read`, `move`, `import`, ...
    pub(crate) action: String,
}

/// The outcome of one run of `terraform plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanOutcome {
    pub(crate) status: PlanStatus,
    pub(crate) terraform_version: Option<String>,
    pub(crate) add: usize,
    pub(crate) change: usize,
    pub(crate) remove: usize,
    pub(crate) import: usize,
    /// What applying the configuration would do, in Terraform's order.
    pub(crate) changes: Vec<ResourceChange>,
    /// What changed outside Terraform since the state was written.
    pub(crate) drift: Vec<ResourceChange>,
    pub(crate) planned_at: String,
    pub(crate) error: Option<String>,
}

/// What `tfex plan` did.
#[derive(Debug, Default, Serialize)]
pub(crate) struct PlanReport {
    /// Directories Terraform made a plan for.
    pub(crate) planned: usize,
    /// Directories Terraform could not make a plan for.
    pub(crate) failed: usize,
    pub(crate) plans: Vec<Plan>,
}

/// The plan of one directory.
#[derive(Debug, Serialize)]
pub(crate) struct Plan {
    pub(crate) repository: String,
    pub(crate) root_id: RootId,
    pub(crate) relative_path: String,
    pub(crate) workspace: String,
    pub(crate) mode: &'static str,
    pub(crate) status: &'static str,
    pub(crate) terraform_version: Option<String>,
    pub(crate) add: usize,
    pub(crate) change: usize,
    pub(crate) remove: usize,
    pub(crate) import: usize,
    pub(crate) changes: Vec<ResourceChange>,
    pub(crate) drift: Vec<ResourceChange>,
    pub(crate) planned_at: String,
    pub(crate) error: Option<String>,
}

/// Run `terraform plan` in each of the directories that `selectors` name,
/// and record the outcome.
///
/// A directory Terraform cannot plan (not initialized, no credentials, no
/// `terraform`, timeout) is an error on that plan, not a failed command.
pub(crate) fn plan(
    index: &mut Index,
    selectors: &[String],
    options: &Options,
) -> Result<PlanReport> {
    let mut selected = HashSet::new();
    let mut roots = Vec::new();
    for selector in selectors {
        let root = index.find_root(selector)?;
        if selected.insert(root.id.clone()) {
            roots.push(root);
        }
    }
    // A plan is mostly waiting for Terraform and for the providers' APIs, so
    // the directories are planned side by side.
    let outcomes: Vec<(String, PlanOutcome)> = roots
        .par_iter()
        .map(|root| {
            let workspace = workspace_of(&root.path, options);
            let outcome = run(&root.path, options);
            (workspace, outcome)
        })
        .collect();

    let mut report = PlanReport::default();
    let writer = index.writer()?;
    for (root, (workspace, outcome)) in roots.into_iter().zip(outcomes) {
        writer.put_plan(&root.id, &workspace, options.mode, &outcome)?;
        match outcome.status {
            PlanStatus::Error => report.failed += 1,
            PlanStatus::NoChanges | PlanStatus::Changes | PlanStatus::Drift => report.planned += 1,
        }
        report
            .plans
            .push(Plan::new(root, workspace, options.mode, outcome));
    }
    writer.commit()?;
    Ok(report)
}

impl Plan {
    fn new(root: RootRow, workspace: String, mode: Mode, outcome: PlanOutcome) -> Self {
        Plan {
            repository: root.repository,
            root_id: root.id,
            relative_path: root.relative_path,
            workspace,
            mode: mode.as_str(),
            status: outcome.status.as_str(),
            terraform_version: outcome.terraform_version,
            add: outcome.add,
            change: outcome.change,
            remove: outcome.remove,
            import: outcome.import,
            changes: outcome.changes,
            drift: outcome.drift,
            planned_at: outcome.planned_at,
            error: outcome.error,
        }
    }
}

/// The workspace a plan of `directory` is about: the one asked for, or the
/// one Terraform has selected there.
fn workspace_of(directory: &Utf8Path, options: &Options) -> String {
    options
        .workspace
        .clone()
        .or_else(|| {
            let selected = fs::read_to_string(directory.join(SELECTED_WORKSPACE_FILE)).ok()?;
            Some(selected.trim().to_owned()).filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_WORKSPACE.to_owned())
}

/// The `terraform plan` invocation for `directory`.
fn command(directory: &Utf8Path, options: &Options) -> Command {
    let mut command = Command::new(&options.program);
    command
        .current_dir(directory)
        // Machine-readable progress on standard output; it names what
        // changes but never shows values. No plan file is written.
        .args(["plan", "-json", "-input=false"])
        // A plan does not write the state, and must not make somebody
        // else's apply wait for it.
        .arg("-lock=false")
        .env("TF_IN_AUTOMATION", "1");
    if options.mode == Mode::RefreshOnly {
        command.arg("-refresh-only");
    }
    if let Some(workspace) = &options.workspace {
        command.env("TF_WORKSPACE", workspace);
    }
    command
}

/// Run Terraform in `directory` and summarize what it said.
fn run(directory: &Utf8Path, options: &Options) -> PlanOutcome {
    let finished = process::run(&mut command(directory, options), options.timeout);
    PlanOutcome::from_run(finished, options)
}

impl PlanOutcome {
    fn from_run(finished: Result<Finished, Failure>, options: &Options) -> Self {
        let program = &options.program;
        let finished = match finished {
            Ok(finished) => finished,
            Err(Failure::TimedOut) => {
                return Self::failed(format!(
                    "`{program}` did not finish within {} seconds",
                    options.timeout.as_secs()
                ));
            }
            Err(Failure::NotStarted(reason) | Failure::Failed(reason)) => {
                return Self::failed(format!("cannot run `{program}`: {reason}"));
            }
        };

        let mut outcome = Self::from_messages(&finished.stdout, options.mode);
        if outcome.error.is_none() && !finished.success {
            // Terraform failed before it could say why in JSON.
            let errors = String::from_utf8_lossy(&finished.stderr);
            outcome.error = Some(format!("`{program}` failed: {}", last_line(&errors)));
        }
        if outcome.error.is_some() {
            outcome.status = PlanStatus::Error;
        }
        outcome
    }

    /// The outcome of a plan Terraform said nothing usable about.
    fn failed(error: String) -> Self {
        PlanOutcome {
            status: PlanStatus::Error,
            terraform_version: None,
            add: 0,
            change: 0,
            remove: 0,
            import: 0,
            changes: Vec::new(),
            drift: Vec::new(),
            planned_at: time::now(),
            error: Some(error),
        }
    }

    /// Summarize the output of `terraform plan -json`: one JSON message per
    /// line. Lines that are not messages are skipped.
    fn from_messages(output: &[u8], mode: Mode) -> Self {
        let mut outcome = PlanOutcome {
            status: PlanStatus::NoChanges,
            error: None,
            ..Self::failed(String::new())
        };
        let mut summary = None;
        for message in output
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Message>(line).ok())
        {
            match message {
                Message::Version { terraform } => outcome.terraform_version = Some(terraform),
                Message::PlannedChange { change } => outcome.changes.push(change.into()),
                Message::ResourceDrift { change } => outcome.drift.push(change.into()),
                Message::ChangeSummary { changes } => summary = Some(changes),
                Message::Diagnostic { diagnostic } => {
                    if diagnostic.severity == "error" && outcome.error.is_none() {
                        outcome.error = Some(truncate(&diagnostic.summary));
                    }
                }
                Message::Other => {}
            }
        }

        if let Some(summary) = summary {
            outcome.add = summary.add;
            outcome.change = summary.change;
            outcome.remove = summary.remove;
            outcome.import = summary.import;
        }
        // Reading a data source during apply changes nothing.
        let is_pending = outcome.add + outcome.change + outcome.remove + outcome.import > 0
            || outcome.changes.iter().any(|change| change.action != "read");
        outcome.status = match mode {
            Mode::Plan if is_pending => PlanStatus::Changes,
            Mode::RefreshOnly if !outcome.drift.is_empty() => PlanStatus::Drift,
            Mode::Plan | Mode::RefreshOnly => PlanStatus::NoChanges,
        };
        outcome
    }
}

/// The messages of Terraform's machine-readable output that are looked at,
/// reduced to the fields that are safe to keep.
///
/// Fields that are not listed (the human-readable text, output values,
/// source snippets of diagnostics) are skipped while parsing.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Version {
        terraform: String,
    },
    PlannedChange {
        change: Change,
    },
    ResourceDrift {
        change: Change,
    },
    ChangeSummary {
        changes: Summary,
    },
    Diagnostic {
        diagnostic: Diagnostic,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Change {
    resource: ChangedResource,
    action: String,
}

#[derive(Debug, Deserialize)]
struct ChangedResource {
    addr: String,
}

impl From<Change> for ResourceChange {
    fn from(change: Change) -> Self {
        ResourceChange {
            address: change.resource.addr,
            action: change.action,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Summary {
    #[serde(default)]
    add: usize,
    #[serde(default)]
    change: usize,
    #[serde(default)]
    remove: usize,
    /// Reported since Terraform 1.5.
    #[serde(default)]
    import: usize,
}

#[derive(Debug, Deserialize)]
struct Diagnostic {
    severity: String,
    /// The one-line description of the problem. The detail, which can quote
    /// values, is not read.
    summary: String,
}

/// The last non-blank line of Terraform's error output, cut to a bounded
/// length.
fn last_line(errors: &str) -> String {
    let line = errors
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("no error message");
    truncate(line)
}

fn truncate(text: &str) -> String {
    text.chars().take(ERROR_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "SENSITIVE";

    fn options(mode: Mode) -> Options {
        Options {
            mode,
            program: "terraform".to_owned(),
            workspace: None,
            timeout: Duration::from_secs(600),
        }
    }

    fn arguments(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn change(address: &str, action: &str) -> ResourceChange {
        ResourceChange {
            address: address.to_owned(),
            action: action.to_owned(),
        }
    }

    fn finished(success: bool, stdout: &str, stderr: &str) -> Finished {
        Finished {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// The messages of a plan that creates, replaces and reads, after
    /// finding one resource changed and one deleted outside Terraform.
    fn plan_output() -> String {
        [
            r#"{"@level":"info","@message":"Terraform 1.9.0","terraform":"1.9.0","type":"version","ui":"1.2"}"#,
            r#"{"@message":"aws_s3_bucket.logs: Refreshing state...","hook":{"resource":{"addr":"aws_s3_bucket.logs"},"id_key":"id","id_value":"SENSITIVE"},"type":"refresh_start"}"#,
            r#"{"type":"resource_drift","change":{"resource":{"addr":"aws_s3_bucket.logs","resource_type":"aws_s3_bucket"},"action":"update"}}"#,
            r#"{"type":"resource_drift","change":{"resource":{"addr":"module.net.aws_subnet.a[0]"},"action":"delete"}}"#,
            r#"{"type":"planned_change","change":{"resource":{"addr":"module.net.aws_subnet.a[0]"},"action":"create"}}"#,
            r#"{"type":"planned_change","change":{"resource":{"addr":"aws_instance.web"},"action":"replace","reason":"cannot_update"}}"#,
            r#"{"type":"planned_change","change":{"resource":{"addr":"data.aws_ami.latest"},"action":"read"}}"#,
            r#"{"@message":"Plan: 2 to add, 0 to change, 1 to destroy.","changes":{"add":2,"change":0,"import":0,"remove":1,"operation":"plan"},"type":"change_summary"}"#,
            r#"{"type":"outputs","outputs":{"password":{"sensitive":true,"action":"create","value":"SENSITIVE"}}}"#,
            "not json at all",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn runs_a_plan_that_writes_and_locks_nothing() {
        let directory = Utf8Path::new("/nonexistent-tfex/envs/prod");
        let plan = command(directory, &options(Mode::Plan));
        assert_eq!(
            arguments(&plan),
            ["terraform", "plan", "-json", "-input=false", "-lock=false"]
        );
        assert_eq!(plan.get_current_dir(), Some(directory.as_std_path()));

        let drift = command(
            directory,
            &Options {
                program: "tofu".to_owned(),
                workspace: Some("staging".to_owned()),
                ..options(Mode::RefreshOnly)
            },
        );
        assert_eq!(
            arguments(&drift),
            [
                "tofu",
                "plan",
                "-json",
                "-input=false",
                "-lock=false",
                "-refresh-only"
            ]
        );
        assert!(
            drift
                .get_envs()
                .any(|(name, value)| name == "TF_WORKSPACE" && value == Some("staging".as_ref()))
        );
    }

    #[test]
    fn summarizes_changes_and_drift_without_values() {
        let outcome =
            PlanOutcome::from_run(Ok(finished(true, &plan_output(), "")), &options(Mode::Plan));
        assert_eq!(outcome.status, PlanStatus::Changes);
        assert_eq!(outcome.terraform_version.as_deref(), Some("1.9.0"));
        assert_eq!(
            (outcome.add, outcome.change, outcome.remove, outcome.import),
            (2, 0, 1, 0)
        );
        assert_eq!(
            outcome.changes,
            [
                change("module.net.aws_subnet.a[0]", "create"),
                change("aws_instance.web", "replace"),
                change("data.aws_ami.latest", "read"),
            ]
        );
        assert_eq!(
            outcome.drift,
            [
                change("aws_s3_bucket.logs", "update"),
                change("module.net.aws_subnet.a[0]", "delete"),
            ]
        );
        assert_eq!(outcome.error, None);
        assert!(!format!("{outcome:?}").contains(SECRET));
    }

    #[test]
    fn status_depends_on_what_was_asked() {
        let drifted =
            r#"{"type":"resource_drift","change":{"resource":{"addr":"a.b"},"action":"update"}}"#;
        let reads_only = r#"{"type":"planned_change","change":{"resource":{"addr":"data.a.b"},"action":"read"}}
{"type":"change_summary","changes":{"add":0,"change":0,"remove":0,"operation":"plan"}}"#;
        let status =
            |output: &str, mode| PlanOutcome::from_messages(output.as_bytes(), mode).status;

        assert_eq!(status(drifted, Mode::RefreshOnly), PlanStatus::Drift);
        // Drift that the configuration would not undo is not a pending change.
        assert_eq!(status(drifted, Mode::Plan), PlanStatus::NoChanges);
        assert_eq!(status(reads_only, Mode::Plan), PlanStatus::NoChanges);
        assert_eq!(status("", Mode::RefreshOnly), PlanStatus::NoChanges);
        assert_eq!(status(&plan_output(), Mode::RefreshOnly), PlanStatus::Drift);
    }

    #[test]
    fn an_error_diagnostic_fails_the_plan_with_its_summary_only() {
        let output = r#"{"type":"version","terraform":"1.9.0"}
{"type":"diagnostic","diagnostic":{"severity":"warning","summary":"Deprecated attribute","detail":"..."}}
{"type":"diagnostic","diagnostic":{"severity":"error","summary":"Backend initialization required, please run \"terraform init\"","detail":"Reason: SENSITIVE","snippet":{"values":[{"statement":"SENSITIVE"}]}}}
{"type":"diagnostic","diagnostic":{"severity":"error","summary":"A second error"}}"#;
        let outcome = PlanOutcome::from_run(Ok(finished(false, output, "")), &options(Mode::Plan));
        assert_eq!(outcome.status, PlanStatus::Error);
        assert_eq!(
            outcome.error.as_deref(),
            Some("Backend initialization required, please run \"terraform init\"")
        );
        assert_eq!(outcome.terraform_version.as_deref(), Some("1.9.0"));
    }

    #[test]
    fn failures_outside_the_json_output_are_described_briefly() {
        let plan = options(Mode::Plan);
        let failed = PlanOutcome::from_run(
            Ok(finished(
                false,
                "",
                "Usage: terraform ...\n\nError: flag -json unknown\n\n",
            )),
            &plan,
        );
        assert_eq!(failed.status, PlanStatus::Error);
        assert_eq!(
            failed.error.as_deref(),
            Some("`terraform` failed: Error: flag -json unknown")
        );

        let missing =
            PlanOutcome::from_run(Err(Failure::NotStarted("not found".to_owned())), &plan);
        assert_eq!(
            missing.error.as_deref(),
            Some("cannot run `terraform`: not found")
        );
        let slow = PlanOutcome::from_run(Err(Failure::TimedOut), &plan);
        assert_eq!(
            slow.error.as_deref(),
            Some("`terraform` did not finish within 600 seconds")
        );
        assert_eq!(slow.status, PlanStatus::Error);
    }

    #[test]
    fn the_planned_workspace_is_the_requested_or_the_selected_one() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = Utf8Path::from_path(directory.path()).unwrap();
        let plan = options(Mode::Plan);
        assert_eq!(workspace_of(path, &plan), "default");

        fs::create_dir(path.join(".terraform")).unwrap();
        fs::write(path.join(SELECTED_WORKSPACE_FILE), "staging\n").unwrap();
        assert_eq!(workspace_of(path, &plan), "staging");

        let requested = Options {
            workspace: Some("prod".to_owned()),
            ..plan
        };
        assert_eq!(workspace_of(path, &requested), "prod");
    }
}
