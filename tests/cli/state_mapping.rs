//! `tfex map-state`.

use serde_json::{Value, json};

use crate::support::{SECRET, Workspace, infra_repository, state_file};

fn scanned_infra() -> Workspace {
    let workspace = infra_repository();
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace
}

fn workspaces(states: &Value) -> Vec<(&str, &str)> {
    states
        .as_array()
        .unwrap()
        .iter()
        .map(|state| {
            (
                state["workspace"].as_str().unwrap(),
                state["status"].as_str().unwrap(),
            )
        })
        .collect()
}

#[test]
fn maps_local_and_remote_states() {
    let workspace = scanned_infra();
    let id = workspace.root_id("envs/prod");
    workspace.write("elsewhere/staging.tfstate", &state_file(42, 3).to_string());
    let state_path = workspace.path("elsewhere/staging.tfstate");

    let local = workspace.tfex(&[
        "map-state",
        &id,
        state_path.as_str(),
        "--workspace",
        "staging",
    ]);
    assert_eq!(
        local,
        json!({
            "root_id": id,
            "workspace": "staging",
            "backend_type": "local",
            "location": state_path.as_str(),
            "status": "available",
            "note": null,
        })
    );

    let remote = workspace.tfex(&[
        "map-state",
        &id,
        "gs://bucket/prod.tfstate",
        "--workspace",
        "remote",
    ]);
    assert_eq!(remote["backend_type"], "gcs");
    assert_eq!(remote["status"], "unresolved");
    assert_eq!(remote["note"], "Run 'tfex fetch' to read this state");

    let unreadable = workspace.tfex(&[
        "map-state",
        &id,
        "azurerm://account/container/prod.tfstate",
        "--workspace",
        "azure",
    ]);
    assert_eq!(unreadable["backend_type"], "azurerm");
    assert_eq!(
        unreadable["note"],
        "tfex cannot read states from this kind of store"
    );

    let status = workspace.tfex(&["status", &id]);
    assert_eq!(
        workspaces(&status["states"]),
        [
            ("azure", "unresolved"),
            ("default", "available"),
            ("remote", "unresolved"),
            ("staging", "available"),
        ]
    );
    assert_eq!(status["states"][3]["serial"], 42);
    assert_eq!(status["states"][3]["resource_count"], 3);
    assert!(!workspace.index_contents().contains(SECRET));
}

#[test]
fn mapping_overrides_a_discovered_state_and_survives_rescans() {
    let workspace = scanned_infra();
    let id = workspace.root_id("envs/prod");
    workspace.write("elsewhere/real.tfstate", &state_file(100, 5).to_string());
    workspace.tfex(&[
        "map-state",
        &id,
        workspace.path("elsewhere/real.tfstate").as_str(),
    ]);

    // The directory still contains its own terraform.tfstate with serial 7.
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);

    let status = workspace.tfex(&["status", &id]);
    assert_eq!(workspaces(&status["states"]), [("default", "available")]);
    assert_eq!(status["states"][0]["serial"], 100);
    assert_eq!(
        status["states"][0]["location"],
        workspace.path("elsewhere/real.tfstate").as_str()
    );
}

#[test]
fn a_mapped_state_that_cannot_be_read_is_an_error_not_a_failure() {
    let workspace = scanned_infra();
    let missing = workspace.path("elsewhere/missing.tfstate");
    let mapped = workspace.tfex(&["map-state", "infra/envs/prod", missing.as_str()]);
    assert_eq!(mapped["status"], "error");

    let status = workspace.tfex(&["status", "infra/envs/prod"]);
    assert_eq!(status["states"][0]["status"], "error");
    assert!(status["states"][0]["error"].is_string());
}

#[test]
fn rejects_an_unsupported_scheme() {
    let workspace = scanned_infra();
    assert_eq!(
        workspace.tfex_failure(&["map-state", "infra/envs/prod", "ftp://host/x.tfstate"]),
        json!({
            "error": "Unsupported state URI scheme: ftp",
            "kind": "unsupported_state_scheme",
        })
    );
}
