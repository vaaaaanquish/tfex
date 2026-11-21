//! `tfex search`, `inspect`, `log` and `status`.

use serde_json::{Value, json};

use crate::support::{SECRET, Workspace, infra_repository};

fn scanned_infra() -> Workspace {
    let workspace = infra_repository();
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace
}

#[test]
fn search_points_at_the_declaration() {
    let workspace = scanned_infra();
    let hits = workspace.tfex(&["search", "aws_s3_bucket"]);
    // The commented-out block is not a hit.
    assert_eq!(
        hits,
        json!([{
            "repository": "infra",
            "root_id": workspace.root_id("envs/prod"),
            "relative_path": "envs/prod",
            "path": workspace.path("infra/envs/prod").as_str(),
            "address": "aws_s3_bucket.logs",
            "kind": "resource",
            "file": "main.tf",
            "line": 7,
        }])
    );
}

#[test]
fn search_matches_substrings_literally_and_honors_the_limit() {
    let workspace = Workspace::new();
    workspace.write(
        "app/main.tf",
        r#"
resource "aws_s3_bucket" "a" {}
resource "aws_s3_bucket" "b" {}
resource "awsXs3Xbucket" "lookalike" {}
data "aws_s3_bucket" "existing" {}
"#,
    );
    workspace.tfex(&["scan", workspace.path("app").as_str()]);

    let addresses = |arguments: &[&str]| -> Vec<String> {
        workspace
            .tfex(arguments)
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["address"].as_str().unwrap().to_owned())
            .collect()
    };
    // `_` is not a wildcard, and both the type and the address are searched.
    assert_eq!(
        addresses(&["search", "s3_bucket"]),
        [
            "aws_s3_bucket.a",
            "aws_s3_bucket.b",
            "data.aws_s3_bucket.existing"
        ]
    );
    assert_eq!(addresses(&["search", "bucket.b"]), ["aws_s3_bucket.b"]);
    assert_eq!(
        addresses(&["search", "aws_s3", "--limit", "2"]),
        ["aws_s3_bucket.a", "aws_s3_bucket.b"]
    );
    assert_eq!(addresses(&["search", "%"]), Vec::<String>::new());
}

#[test]
fn inspect_shows_state_metadata_but_no_state_values() {
    let workspace = scanned_infra();
    let root = workspace.tfex(&["inspect", &workspace.root_id("envs/prod")]);

    assert_eq!(root["repository"], "infra");
    assert_eq!(root["relative_path"], "envs/prod");
    assert_eq!(root["classification"], "confirmed");
    assert_eq!(
        root["module_calls"],
        json!([{
            "name": "network",
            "source": "../../modules/network",
            "file": "main.tf",
            "line": 9,
        }])
    );

    let states = root["states"].as_array().unwrap();
    assert_eq!(states.len(), 1);
    let state = &states[0];
    assert_eq!(state["workspace"], "default");
    assert_eq!(state["backend_type"], "local");
    assert_eq!(
        state["location"],
        workspace.path("infra/envs/prod/terraform.tfstate").as_str()
    );
    assert_eq!(state["status"], "available");
    assert_eq!(state["serial"], 7);
    assert_eq!(state["lineage"], "test-lineage");
    assert_eq!(state["terraform_version"], "1.9.0");
    assert_eq!(state["resource_count"], 1);
    assert!(state["fetched_at"].is_string());

    assert_eq!(root["last_commit"]["subject"], "Create production infra");
    assert_eq!(root["last_commit"]["author"], "Test Person");
    assert!(!root.to_string().contains(SECRET));
}

#[test]
fn log_is_scoped_to_the_root() {
    let workspace = scanned_infra();
    workspace.write(
        "infra/modules/network/outputs.tf",
        "output \"id\" {\n  value = aws_vpc.main.id\n}\n",
    );
    workspace.git_commit("infra", "Expose the network id");
    workspace.write(
        "infra/envs/prod/versions.tf",
        "terraform {\n  required_version = \">= 1.9\"\n}\n",
    );
    workspace.git_commit("infra", "Pin the Terraform version");

    let subjects = |arguments: &[&str]| -> Vec<String> {
        workspace
            .tfex(arguments)
            .as_array()
            .unwrap()
            .iter()
            .map(|commit| commit["subject"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        subjects(&["log", "infra/envs/prod"]),
        ["Pin the Terraform version", "Create production infra"]
    );
    assert_eq!(
        subjects(&["log", "infra/modules/network"]),
        ["Expose the network id", "Create production infra"]
    );
    assert_eq!(
        subjects(&["log", "infra/envs/prod", "--limit", "1"]),
        ["Pin the Terraform version"]
    );
}

#[test]
fn log_is_empty_outside_git() {
    let workspace = Workspace::new();
    workspace.write("plain/main.tf", "resource \"terraform_data\" \"x\" {}");
    workspace.tfex(&["scan", workspace.path("plain").as_str()]);

    assert_eq!(workspace.tfex(&["log", "plain"]), json!([]));
    assert_eq!(
        workspace.tfex(&["inspect", "plain"])["last_commit"],
        Value::Null
    );
}

#[test]
fn a_root_can_be_selected_in_several_ways() {
    let workspace = scanned_infra();
    let id = workspace.root_id("envs/prod");
    let directory = workspace.path("infra/envs/prod");

    for selector in [&id, &id[..6], "infra/envs/prod", directory.as_str()] {
        let root = workspace.tfex(&["inspect", selector]);
        assert_eq!(root["id"], id.as_str(), "selected with {selector}");
    }
}

#[test]
fn unknown_and_ambiguous_selectors_fail() {
    let workspace = scanned_infra();
    assert_eq!(
        workspace.tfex_failure(&["inspect", "no-such-root"]),
        json!({
            "error": "Root not found: no-such-root; run 'roots' first",
            "kind": "root_not_found",
        })
    );
    // The empty prefix matches every root.
    assert_eq!(
        workspace.tfex_failure(&["log", ""]),
        json!({
            "error": "Ambiguous root: ; use its ID",
            "kind": "ambiguous_root",
        })
    );
}

#[test]
fn status_lists_every_workspace_or_those_of_one_root() {
    let workspace = scanned_infra();
    let overview = workspace.tfex(&["status"]);
    assert_eq!(
        overview
            .as_array()
            .unwrap()
            .iter()
            .map(|row| (
                row["relative_path"].as_str().unwrap(),
                row["workspace"].as_str().unwrap(),
                row["status"].as_str().unwrap(),
            ))
            .collect::<Vec<_>>(),
        [
            ("envs/prod", "default", "available"),
            ("modules/network", "default", "unresolved"),
        ]
    );

    let one = workspace.tfex(&["status", "infra/envs/prod"]);
    assert_eq!(one["root"], "infra/envs/prod");
    assert_eq!(one["states"][0]["serial"], 7);
}
