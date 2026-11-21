//! `tfex fetch`, against stand-ins for the `gcloud` and `aws` tools.

use std::fs;

use serde_json::{Value, json};

use crate::support::{SECRET, Workspace, state_file};

const GCS_ROOT: &str = r#"terraform {
  backend "gcs" {
    bucket = "tf-state"
    prefix = "envs/prod"
  }
}
"#;

const S3_ROOT: &str = r#"terraform {
  backend "s3" {
    bucket  = "tf-state"
    key     = "network/terraform.tfstate"
    region  = "ap-northeast-1"
    profile = "infra"
  }
}
"#;

/// A scanned workspace with one root per readable backend, `infra/gcs` and
/// `infra/s3`, whose states exist remotely with serials 11 and 22.
fn scanned_remote_roots() -> Workspace {
    let workspace = Workspace::new();
    workspace.fake_remote();
    workspace.write("infra/gcs/main.tf", GCS_ROOT);
    workspace.write("infra/s3/main.tf", S3_ROOT);
    workspace.write(
        "remote/gs/tf-state/envs/prod/default.tfstate",
        &state_file(11, 4).to_string(),
    );
    workspace.write(
        "remote/s3/tf-state/network/terraform.tfstate",
        &state_file(22, 2).to_string(),
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace
}

/// `(relative path, workspace, status, serial)` of each row of `tfex status`.
fn overview(workspace: &Workspace) -> Vec<(String, String, String, Value)> {
    workspace
        .tfex(&["status"])
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let state = workspace.tfex(&["status", row["root_id"].as_str().unwrap()]);
            let serial = state["states"]
                .as_array()
                .unwrap()
                .iter()
                .find(|state| state["workspace"] == row["workspace"])
                .unwrap()["serial"]
                .clone();
            (
                row["relative_path"].as_str().unwrap().to_owned(),
                row["workspace"].as_str().unwrap().to_owned(),
                row["status"].as_str().unwrap().to_owned(),
                serial,
            )
        })
        .collect()
}

fn row(
    path: &str,
    workspace: &str,
    status: &str,
    serial: Value,
) -> (String, String, String, Value) {
    (
        path.to_owned(),
        workspace.to_owned(),
        status.to_owned(),
        serial,
    )
}

#[test]
fn scan_locates_remote_states_without_reading_them() {
    let workspace = scanned_remote_roots();
    let status = workspace.tfex(&["status"]);
    assert_eq!(
        status,
        json!([
            {
                "repository": "infra",
                "root_id": workspace.root_id("gcs"),
                "relative_path": "gcs",
                "workspace": "default",
                "backend_type": "gcs",
                "location": "gs://tf-state/envs/prod/default.tfstate",
                "status": "unresolved",
                "resource_count": null,
                "fetched_at": null,
            },
            {
                "repository": "infra",
                "root_id": workspace.root_id("s3"),
                "relative_path": "s3",
                "workspace": "default",
                "backend_type": "s3",
                "location": "s3://tf-state/network/terraform.tfstate",
                "status": "unresolved",
                "resource_count": null,
                "fetched_at": null,
            },
        ])
    );
    assert_eq!(workspace.remote_calls(), Vec::<String>::new());
}

#[test]
fn fetch_reads_metadata_from_every_readable_backend() {
    let workspace = scanned_remote_roots();
    let report = workspace.tfex(&["fetch"]);
    assert_eq!(report["fetched"], 2);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["unresolved"], 0);

    let states = report["states"].as_array().unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0]["relative_path"], "gcs");
    assert_eq!(states[0]["status"], "available");
    assert_eq!(states[0]["serial"], 11);
    assert_eq!(states[0]["resource_count"], 4);
    assert!(states[0]["fetched_at"].is_string());
    assert_eq!(states[1]["relative_path"], "s3");
    assert_eq!(states[1]["serial"], 22);

    // The tools were asked for exactly these objects, with the identity
    // settings of the backend passed on.
    assert_eq!(
        workspace.remote_calls(),
        [
            "aws s3 cp s3://tf-state/network/terraform.tfstate - --region=ap-northeast-1 \
             --profile=infra",
            "gcloud storage cat gs://tf-state/envs/prod/default.tfstate",
        ]
    );

    let inspected = workspace.tfex(&["inspect", "infra/gcs"]);
    assert_eq!(inspected["states"][0]["lineage"], "test-lineage");
    assert_eq!(inspected["states"][0]["terraform_version"], "1.9.0");
    assert!(!workspace.index_contents().contains(SECRET));
    assert!(!report.to_string().contains(SECRET));
}

#[test]
fn fetched_metadata_survives_a_rescan_until_the_state_moves() {
    let workspace = scanned_remote_roots();
    workspace.tfex(&["fetch"]);

    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    assert_eq!(
        overview(&workspace),
        [
            row("gcs", "default", "available", json!(11)),
            row("s3", "default", "available", json!(22)),
        ]
    );

    workspace.write(
        "infra/gcs/main.tf",
        &GCS_ROOT.replace("envs/prod", "envs/staging"),
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    assert_eq!(
        overview(&workspace),
        [
            row("gcs", "default", "unresolved", Value::Null),
            row("s3", "default", "available", json!(22)),
        ]
    );
    let moved = workspace.tfex(&["status", "infra/gcs"]);
    assert_eq!(
        moved["states"][0]["location"],
        "gs://tf-state/envs/staging/default.tfstate"
    );
}

#[test]
fn fetch_can_be_limited_to_some_roots() {
    let workspace = scanned_remote_roots();
    let report = workspace.tfex(&["fetch", "infra/s3"]);
    assert_eq!(report["fetched"], 1);
    assert_eq!(report["states"][0]["relative_path"], "s3");
    assert_eq!(
        overview(&workspace),
        [
            row("gcs", "default", "unresolved", Value::Null),
            row("s3", "default", "available", json!(22)),
        ]
    );

    let failure = workspace.tfex_failure(&["fetch", "infra/s3", "infra/missing"]);
    assert_eq!(failure["kind"], "root_not_found");
}

#[test]
fn a_state_that_cannot_be_read_is_recorded_as_an_error() {
    let workspace = scanned_remote_roots();
    fs::remove_file(workspace.path("remote/gs/tf-state/envs/prod/default.tfstate")).unwrap();
    workspace.write(
        "remote/s3/tf-state/network/terraform.tfstate",
        &format!(r#"{{"serial": "{SECRET}"}}"#),
    );

    let report = workspace.tfex(&["fetch"]);
    assert_eq!(report["fetched"], 0);
    assert_eq!(report["failed"], 2);
    assert_eq!(report["states"][0]["status"], "error");
    assert_eq!(
        report["states"][0]["error"],
        "`gcloud` failed: ERROR: gs://tf-state/envs/prod/default.tfstate: no such object, or \
         access denied"
    );
    assert_eq!(
        report["states"][1]["error"],
        "state does not have the structure of a state file (line 1, column 22)"
    );
    assert!(!workspace.index_contents().contains(SECRET));
}

#[test]
fn unlocated_and_unreadable_states_are_left_unresolved() {
    let workspace = Workspace::new();
    workspace.fake_remote();
    // Settings injected with -backend-config, a dynamic bucket, and a store
    // that tfex does not read.
    workspace.write(
        "infra/partial/main.tf",
        "terraform {\n  backend \"gcs\" {}\n}\n",
    );
    workspace.write(
        "infra/dynamic/main.tf",
        "terraform {\n  backend \"gcs\" {\n    bucket = \"state-${var.env}\"\n  }\n}\n",
    );
    workspace.write(
        "infra/azure/main.tf",
        "terraform {\n  backend \"azurerm\" {\n    container_name = \"state\"\n    key = \
         \"prod.tfstate\"\n  }\n}\n",
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace.tfex(&[
        "map-state",
        "infra/azure",
        "azurerm://account/state/prod.tfstate",
    ]);

    let report = workspace.tfex(&["fetch"]);
    assert_eq!(
        report,
        json!({"fetched": 0, "failed": 0, "unresolved": 3, "states": []})
    );
    assert_eq!(workspace.remote_calls(), Vec::<String>::new());
}

#[test]
fn a_mapped_remote_state_is_fetched_from_where_it_was_mapped() {
    let workspace = Workspace::new();
    workspace.fake_remote();
    workspace.write("infra/app/main.tf", "terraform {\n  backend \"s3\" {}\n}\n");
    workspace.write(
        "remote/gs/elsewhere/app/prod.tfstate",
        &state_file(77, 1).to_string(),
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace.tfex(&[
        "map-state",
        "infra/app",
        "gs://elsewhere/app/prod.tfstate",
        "--workspace",
        "prod",
    ]);

    let report = workspace.tfex(&["fetch", "infra/app"]);
    assert_eq!(report["fetched"], 1);
    assert_eq!(report["states"][0]["workspace"], "prod");
    assert_eq!(report["states"][0]["backend_type"], "gcs");
    assert_eq!(report["states"][0]["serial"], 77);
    // The directory declares an s3 backend, so its settings do not apply.
    assert_eq!(
        workspace.remote_calls(),
        ["gcloud storage cat gs://elsewhere/app/prod.tfstate"]
    );
}

#[test]
fn fetch_also_rereads_local_states() {
    let workspace = Workspace::new();
    workspace.write(
        "infra/local/main.tf",
        "resource \"terraform_data\" \"x\" {}\n",
    );
    workspace.write(
        "infra/local/terraform.tfstate",
        &state_file(1, 1).to_string(),
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);

    workspace.write(
        "infra/local/terraform.tfstate",
        &state_file(2, 5).to_string(),
    );
    let report = workspace.tfex(&["fetch"]);
    assert_eq!(report["fetched"], 1);
    assert_eq!(report["states"][0]["backend_type"], "local");
    assert_eq!(report["states"][0]["serial"], 2);
    assert_eq!(report["states"][0]["resource_count"], 5);
}

#[test]
fn a_read_that_hangs_is_given_up() {
    let workspace = scanned_remote_roots();
    workspace.write("remote/gs/tf-state/envs/prod/default.tfstate.slow", "");

    let report = workspace.tfex(&["fetch", "infra/gcs", "--timeout", "1"]);
    assert_eq!(report["failed"], 1);
    assert_eq!(
        report["states"][0]["error"],
        "`gcloud` did not finish within 1 seconds"
    );
}

#[test]
fn a_missing_tool_is_reported_per_state() {
    let workspace = scanned_remote_roots();
    let assert = workspace
        .command(&["fetch", "infra/gcs"])
        .env("PATH", workspace.path("nowhere"))
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(report["failed"], 1);
    let error = report["states"][0]["error"].as_str().unwrap();
    assert!(error.starts_with("cannot run `gcloud`: "), "{error}");
}
