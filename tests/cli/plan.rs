//! `tfex plan`, against a stand-in for `terraform`.

use serde_json::{Value, json};

use crate::support::{SECRET, Workspace};

const ROOT: &str =
    "terraform {\n  backend \"local\" {}\n}\n\nresource \"aws_s3_bucket\" \"logs\" {}\n";

/// What `terraform plan -json` prints for a plan that creates one resource
/// and replaces another, after finding the latter changed outside Terraform.
fn plan_output() -> String {
    [
        json!({"@message": "Terraform 1.9.0", "terraform": "1.9.0", "type": "version"}),
        json!({
            "type": "resource_drift",
            "@message": format!("aws_s3_bucket.logs: Drift detected (update) {SECRET}"),
            "change": {"resource": {"addr": "aws_s3_bucket.logs"}, "action": "update"},
        }),
        json!({
            "type": "planned_change",
            "change": {"resource": {"addr": "aws_s3_bucket.logs"}, "action": "replace"},
        }),
        json!({
            "type": "planned_change",
            "change": {"resource": {"addr": "aws_sqs_queue.jobs[\"a\"]"}, "action": "create"},
        }),
        json!({
            "type": "change_summary",
            "changes": {"add": 2, "change": 0, "import": 0, "remove": 1, "operation": "plan"},
        }),
        json!({
            "type": "outputs",
            "outputs": {"password": {"sensitive": true, "action": "create", "value": SECRET}},
        }),
    ]
    .map(|message| message.to_string())
    .join("\n")
}

fn refresh_output() -> String {
    [
        json!({"terraform": "1.9.0", "type": "version"}),
        json!({
            "type": "resource_drift",
            "change": {"resource": {"addr": "aws_s3_bucket.logs"}, "action": "delete"},
        }),
        json!({
            "type": "change_summary",
            "changes": {"add": 0, "change": 0, "remove": 0, "operation": "plan"},
        }),
    ]
    .map(|message| message.to_string())
    .join("\n")
}

/// A scanned workspace with the roots `infra/prod` and `infra/dev`, and a
/// stand-in for `terraform` that reports changes and drift in `prod` and
/// nothing in `dev`.
fn scanned_roots() -> Workspace {
    let workspace = Workspace::new();
    workspace.fake_terraform();
    workspace.write("infra/prod/main.tf", ROOT);
    workspace.write("infra/prod/fake-plan.jsonl", &plan_output());
    workspace.write("infra/prod/fake-refresh.jsonl", &refresh_output());
    workspace.write("infra/dev/main.tf", ROOT);
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace
}

fn change(address: &str, action: &str) -> Value {
    json!({"address": address, "action": action})
}

#[test]
fn nothing_but_plan_runs_terraform() {
    let workspace = scanned_roots();
    workspace.tfex(&["scan", "--full", workspace.path("infra").as_str()]);
    workspace.tfex(&["inspect", "infra/prod"]);
    workspace.tfex(&["graph", "infra/prod"]);
    workspace.tfex(&["status"]);
    workspace.tfex(&["fetch"]);
    assert_eq!(workspace.terraform_calls(), Vec::<String>::new());
}

#[test]
fn plan_reports_what_would_change_without_values() {
    let workspace = scanned_roots();
    let assert = workspace
        .command(&["plan", "infra/prod"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains(SECRET));
    let report: Value = serde_json::from_str(&stdout).unwrap();

    let planned_at = report["plans"][0]["planned_at"].clone();
    assert!(planned_at.is_string());
    assert_eq!(
        report,
        json!({
            "planned": 1,
            "failed": 0,
            "plans": [{
                "repository": "infra",
                "root_id": workspace.root_id("prod"),
                "relative_path": "prod",
                "workspace": "default",
                "mode": "plan",
                "status": "changes",
                "terraform_version": "1.9.0",
                "add": 2,
                "change": 0,
                "remove": 1,
                "import": 0,
                "changes": [
                    change("aws_s3_bucket.logs", "replace"),
                    change("aws_sqs_queue.jobs[\"a\"]", "create"),
                ],
                "drift": [change("aws_s3_bucket.logs", "update")],
                "planned_at": planned_at,
                "error": null,
            }],
        })
    );
    assert_eq!(
        workspace.terraform_calls(),
        [format!(
            "{}||plan -json -input=false -lock=false",
            workspace.path("infra/prod")
        )]
    );
    assert!(!workspace.index_contents().contains(SECRET));
}

#[test]
fn refresh_only_plan_detects_drift() {
    let workspace = scanned_roots();
    let report = workspace.tfex(&[
        "plan",
        "--refresh-only",
        "--workspace",
        "staging",
        "infra/prod",
        "infra/dev",
        // The same root again, named another way.
        &workspace.root_id("prod"),
    ]);
    assert_eq!(report["planned"], 2);
    let statuses: Vec<(&str, &str, &Value)> = report["plans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plan| {
            (
                plan["relative_path"].as_str().unwrap(),
                plan["status"].as_str().unwrap(),
                &plan["drift"],
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [
            (
                "prod",
                "drift",
                &json!([change("aws_s3_bucket.logs", "delete")])
            ),
            ("dev", "no_changes", &json!([])),
        ]
    );
    assert_eq!(report["plans"][0]["mode"], "refresh-only");
    assert_eq!(report["plans"][0]["workspace"], "staging");

    let mut calls = workspace.terraform_calls();
    calls.sort();
    let arguments = "plan -json -input=false -lock=false -refresh-only";
    assert_eq!(
        calls,
        [
            format!("{}|staging|{arguments}", workspace.path("infra/dev")),
            format!("{}|staging|{arguments}", workspace.path("infra/prod")),
        ]
    );
}

#[test]
fn the_last_plan_of_each_kind_is_kept_and_survives_a_rescan() {
    let workspace = scanned_roots();
    workspace.tfex(&["plan", "infra/prod"]);
    workspace.tfex(&["plan", "--refresh-only", "infra/prod"]);
    workspace.tfex(&["scan", "--full", workspace.path("infra").as_str()]);

    let plans = workspace.tfex(&["inspect", "infra/prod"])["plans"].take();
    let planned_at = plans[0]["planned_at"].clone();
    assert_eq!(
        plans[0],
        json!({
            "workspace": "default",
            "mode": "plan",
            "status": "changes",
            "terraform_version": "1.9.0",
            "add": 2,
            "change": 0,
            "remove": 1,
            "import": 0,
            "drift": 1,
            "planned_at": planned_at,
            "error": null,
        })
    );
    assert_eq!(plans[1]["mode"], "refresh-only");
    assert_eq!(plans[1]["status"], "drift");
    assert_eq!(plans[1]["drift"], 1);
    assert_eq!(plans.as_array().unwrap().len(), 2);
    assert_eq!(
        workspace.tfex(&["inspect", "infra/dev"])["plans"],
        json!([])
    );

    // Planning again replaces the plan of the same kind.
    workspace.write("infra/prod/fake-plan.jsonl", "");
    workspace.tfex(&["plan", "infra/prod"]);
    let plans = workspace.tfex(&["inspect", "infra/prod"])["plans"].take();
    assert_eq!(plans[0]["status"], "no_changes");
    assert_eq!(plans[0]["terraform_version"], Value::Null);
    assert_eq!(plans.as_array().unwrap().len(), 2);
}

#[test]
fn a_plan_terraform_cannot_make_is_an_error_on_that_plan() {
    let workspace = scanned_roots();
    let diagnostic = json!({
        "type": "diagnostic",
        "diagnostic": {
            "severity": "error",
            "summary": "Backend initialization required, please run \"terraform init\"",
            "detail": SECRET,
        },
    });
    workspace.write("infra/dev/fake-plan.jsonl", &diagnostic.to_string());
    workspace.write("infra/dev/fake-failure", "");

    let assert = workspace
        .command(&["plan", "infra/prod", "infra/dev"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains(SECRET));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["planned"], 1);
    assert_eq!(report["failed"], 1);
    assert_eq!(report["plans"][0]["status"], "changes");
    assert_eq!(report["plans"][1]["status"], "error");
    assert_eq!(
        report["plans"][1]["error"],
        "Backend initialization required, please run \"terraform init\""
    );
    assert_eq!(
        workspace.tfex(&["inspect", "infra/dev"])["plans"][0]["status"],
        "error"
    );

    // Without JSON to go by, the last line Terraform wrote is the reason.
    workspace.write("infra/dev/fake-plan.jsonl", "");
    workspace.write(
        "infra/dev/fake-failure",
        "Usage: ...\nError: no such flag\n",
    );
    let report = workspace.tfex(&["plan", "infra/dev"]);
    assert_eq!(
        report["plans"][0]["error"],
        "`terraform` failed: Error: no such flag"
    );
}

#[test]
fn a_plan_that_hangs_is_given_up() {
    let workspace = scanned_roots();
    workspace.write("infra/dev/fake-slow", "");
    let report = workspace.tfex(&["plan", "--timeout", "1", "infra/dev"]);
    assert_eq!(report["failed"], 1);
    assert_eq!(
        report["plans"][0]["error"],
        "`terraform` did not finish within 1 seconds"
    );
}

#[test]
fn another_program_can_stand_in_for_terraform() {
    let workspace = scanned_roots();
    let report = workspace.tfex(&["plan", "--terraform", "tfex-no-such-terraform", "infra/dev"]);
    assert_eq!(report["plans"][0]["status"], "error");
    let error = report["plans"][0]["error"].as_str().unwrap();
    assert!(
        error.starts_with("cannot run `tfex-no-such-terraform`: "),
        "{error}"
    );

    let report = workspace
        .command(&["plan", "infra/prod"])
        .env("TFEX_TERRAFORM", workspace.path("bin/terraform"))
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&report.get_output().stdout).unwrap();
    assert_eq!(report["plans"][0]["status"], "changes");
}

#[test]
fn plan_fails_on_an_unknown_root_before_running_anything() {
    let workspace = scanned_roots();
    let failure = workspace.tfex_failure(&["plan", "infra/prod", "no-such-root"]);
    assert_eq!(failure["kind"], "root_not_found");
    assert_eq!(workspace.terraform_calls(), Vec::<String>::new());
}
