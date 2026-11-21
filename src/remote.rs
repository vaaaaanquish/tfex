//! Reading state from remote stores.
//!
//! State objects are read through the vendors' own command-line tools
//! (`gcloud storage cat`, `aws s3 cp`), which use whatever credentials the
//! user has already set up for them: tfex handles no credentials of its own.
//!
//! This is the only module that reaches the network, and only `tfex fetch`
//! calls it. The object is streamed into memory and reduced to a
//! [`StateSummary`] at once; it is never written to disk.

use std::process::Command;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::config;
use crate::process::{self, Failure};
use crate::state::StateSummary;

/// Longest error text, in characters, kept from a tool's standard error.
const ERROR_LIMIT: usize = 300;

/// A remote store tfex can read state from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    Gcs,
    S3,
}

impl Store {
    fn of(uri: &str) -> Option<Self> {
        match uri.split_once("://")?.0 {
            "gs" => Some(Store::Gcs),
            "s3" => Some(Store::S3),
            _ => None,
        }
    }

    /// The command-line tool that reads from the store.
    fn program(self) -> &'static str {
        match self {
            Store::Gcs => "gcloud",
            Store::S3 => "aws",
        }
    }

    /// The command that writes the object at `uri` to standard output.
    ///
    /// `settings` are the backend settings of the directory, which may say
    /// whose identity to use.
    fn command(self, uri: &str, settings: &Map<String, Value>) -> Command {
        let setting = |name: &str| {
            settings
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !config::is_template(value))
        };
        let mut command = Command::new(self.program());
        match self {
            Store::Gcs => {
                command.args(["storage", "cat", uri]);
                if let Some(account) = setting("impersonate_service_account") {
                    command.arg(format!("--impersonate-service-account={account}"));
                }
            }
            Store::S3 => {
                command.args(["s3", "cp", uri, "-"]);
                for option in ["region", "profile"] {
                    if let Some(value) = setting(option) {
                        command.arg(format!("--{option}={value}"));
                    }
                }
            }
        }
        command
    }
}

/// Whether `tfex fetch` can read the state at `uri`.
pub(crate) fn is_readable(uri: &str) -> bool {
    Store::of(uri).is_some()
}

/// Read the state at `uri` and summarize it, giving up after `timeout`.
///
/// A state that cannot be read (no access, no such object, tool missing,
/// timeout) yields an error summary. `None` when the store is not one tfex
/// can read.
pub(crate) fn read(
    uri: &str,
    settings: &Map<String, Value>,
    timeout: Duration,
) -> Option<StateSummary> {
    let store = Store::of(uri)?;
    let contents = process::output(&mut store.command(uri, settings), timeout)
        .map_err(|failure| describe(store.program(), &failure, timeout));
    Some(StateSummary::from_contents(contents))
}

fn describe(program: &str, failure: &Failure, timeout: Duration) -> String {
    match failure {
        Failure::NotStarted(reason) => format!("cannot run `{program}`: {reason}"),
        Failure::TimedOut => format!(
            "`{program}` did not finish within {} seconds",
            timeout.as_secs()
        ),
        Failure::Failed(errors) => format!("`{program}` failed: {}", last_line(errors)),
    }
}

/// The last non-blank line of a tool's error output, which is where these
/// tools put the actual cause, cut to a bounded length.
fn last_line(errors: &str) -> String {
    let line = errors
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("no error message");
    line.chars().take(ERROR_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn arguments(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn settings(value: &Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn knows_which_stores_it_can_read() {
        assert!(is_readable("gs://bucket/default.tfstate"));
        assert!(is_readable("s3://bucket/key"));
        assert!(!is_readable("azurerm://account/container/key"));
        assert!(!is_readable("/srv/terraform.tfstate"));
    }

    #[test]
    fn gcs_objects_are_read_with_gcloud() {
        let uri = "gs://bucket/prod/default.tfstate";
        let plain = Store::Gcs.command(uri, &Map::new());
        assert_eq!(arguments(&plain), ["gcloud", "storage", "cat", uri]);

        let impersonating = Store::Gcs.command(
            uri,
            &settings(&json!({"impersonate_service_account": "tf@project.iam.example"})),
        );
        assert_eq!(
            arguments(&impersonating),
            [
                "gcloud",
                "storage",
                "cat",
                uri,
                "--impersonate-service-account=tf@project.iam.example"
            ]
        );
    }

    #[test]
    fn s3_objects_are_read_with_the_aws_cli() {
        let uri = "s3://bucket/prod/terraform.tfstate";
        let plain = Store::S3.command(uri, &Map::new());
        assert_eq!(arguments(&plain), ["aws", "s3", "cp", uri, "-"]);

        let configured = Store::S3.command(
            uri,
            &settings(&json!({"region": "ap-northeast-1", "profile": "infra", "bucket": "b"})),
        );
        assert_eq!(
            arguments(&configured),
            [
                "aws",
                "s3",
                "cp",
                uri,
                "-",
                "--region=ap-northeast-1",
                "--profile=infra"
            ]
        );
    }

    #[test]
    fn templated_settings_are_not_passed_on() {
        let uri = "s3://bucket/key";
        let command = Store::S3.command(uri, &settings(&json!({"region": "${var.region}"})));
        assert_eq!(arguments(&command), ["aws", "s3", "cp", uri, "-"]);
    }

    #[test]
    fn unreadable_stores_are_not_attempted() {
        let summary = read(
            "remote://app.terraform.io/acme/prod",
            &Map::new(),
            Duration::from_secs(1),
        );
        assert_eq!(summary, None);
    }

    #[test]
    fn failures_are_described_briefly() {
        let timeout = Duration::from_secs(60);
        assert_eq!(
            describe("aws", &Failure::TimedOut, timeout),
            "`aws` did not finish within 60 seconds"
        );
        assert_eq!(
            describe(
                "gcloud",
                &Failure::NotStarted("not found".to_owned()),
                timeout
            ),
            "cannot run `gcloud`: not found"
        );
        let errors = "Traceback...\n  detail\nERROR: 403 caller lacks storage.objects.get\n\n";
        assert_eq!(
            describe("gcloud", &Failure::Failed(errors.to_owned()), timeout),
            "`gcloud` failed: ERROR: 403 caller lacks storage.objects.get"
        );
        assert_eq!(
            describe("aws", &Failure::Failed(String::new()), timeout),
            "`aws` failed: no error message"
        );
        let long = "x".repeat(ERROR_LIMIT * 2);
        assert_eq!(last_line(&long).chars().count(), ERROR_LIMIT);
    }
}
