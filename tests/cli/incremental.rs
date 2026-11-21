//! Rescanning: `tfex scan` leaves unchanged directories unread.

use serde_json::{Value, json};

use crate::support::{Workspace, infra_repository, state_file};

fn addresses(workspace: &Workspace, query: &str) -> Vec<String> {
    workspace
        .tfex(&["search", query])
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["address"].as_str().unwrap().to_owned())
        .collect()
}

/// Everything the queries say about the index, to compare two scans by.
fn snapshot(workspace: &Workspace) -> Value {
    let mut roots = workspace.tfex(&["roots", "--all"]);
    let inspections: Vec<Value> = roots
        .as_array()
        .unwrap()
        .iter()
        .map(|root| {
            let mut inspection = workspace.tfex(&["inspect", root["id"].as_str().unwrap()]);
            let graph = workspace.tfex(&["graph", root["id"].as_str().unwrap()]);
            inspection["indexed_at"].take();
            json!([inspection, graph])
        })
        .collect();
    for root in roots.as_array_mut().unwrap() {
        root["indexed_at"].take();
    }
    json!([roots, inspections])
}

#[test]
fn a_rescan_reads_only_the_directories_that_changed() {
    let workspace = infra_repository();
    let infra = workspace.path("infra");
    let first = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(first["unchanged_directories"], 0);
    let indexed = snapshot(&workspace);

    let second = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(
        second,
        json!({
            "indexed_directories": 2,
            "unchanged_directories": 2,
            "repositories": 1,
            "confirmed_roots": 1,
            "probable_roots": 0,
            "modules": 1,
            "duplicate_checkouts": 0,
            "pruned_roots": 0,
            "parse_fallbacks": [],
        })
    );
    assert_eq!(snapshot(&workspace), indexed);

    workspace.write(
        "infra/modules/network/main.tf",
        "resource \"aws_vpc\" \"main\" {}\nresource \"aws_subnet\" \"a\" {}\n",
    );
    workspace.write("infra/envs/dev/main.tf", "provider \"aws\" {}\n");
    let third = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(third["indexed_directories"], 3);
    assert_eq!(third["unchanged_directories"], 1);
    assert_eq!(addresses(&workspace, "aws_subnet"), ["aws_subnet.a"]);
    assert_eq!(
        addresses(&workspace, "aws_s3_bucket"),
        ["aws_s3_bucket.logs"]
    );
}

#[test]
fn full_scan_reads_what_looks_unchanged() {
    let workspace = Workspace::new();
    workspace.write("app/main.tf", "resource \"null_resource\" \"old\" {}\n");
    let app = workspace.path("app");
    workspace.tfex(&["scan", app.as_str()]);

    // A change that leaves size and modification time as they were is what
    // an incremental scan cannot see: the directory is not read.
    workspace.overwrite_unnoticed("app/main.tf", "resource \"null_resource\" \"new\" {}\n");
    let incremental = workspace.tfex(&["scan", app.as_str()]);
    assert_eq!(incremental["unchanged_directories"], 1);
    assert_eq!(
        addresses(&workspace, "null_resource"),
        ["null_resource.old"]
    );

    let full = workspace.tfex(&["scan", "--full", app.as_str()]);
    assert_eq!(full["unchanged_directories"], 0);
    assert_eq!(
        addresses(&workspace, "null_resource"),
        ["null_resource.new"]
    );
}

#[test]
fn an_unchanged_directory_is_still_classified_and_its_states_read() {
    let workspace = Workspace::new();
    workspace.write("infra/app/main.tf", "resource \"null_resource\" \"x\" {}\n");
    let infra = workspace.path("infra");
    workspace.tfex(&["scan", infra.as_str()]);
    let classification = |path: &str| {
        workspace.tfex(&["inspect", &format!("infra/{path}")])["classification"].clone()
    };
    assert_eq!(classification("app"), "probable");

    // Another directory starts calling it: it is a module now.
    workspace.write(
        "infra/root/main.tf",
        "module \"app\" {\n  source = \"../app\"\n}\n",
    );
    let report = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(report["unchanged_directories"], 1);
    assert_eq!(classification("app"), "module");

    // The caller is still a caller when it is the one left unread.
    let report = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(report["unchanged_directories"], 2);
    assert_eq!(report["modules"], 1);
    assert_eq!(classification("app"), "module");

    // A state appears next to configuration that did not change.
    workspace.write(
        "infra/root/terraform.tfstate",
        &state_file(3, 2).to_string(),
    );
    let report = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(report["unchanged_directories"], 2);
    let root = workspace.tfex(&["inspect", "infra/root"]);
    assert_eq!(root["classification"], "confirmed");
    assert_eq!(root["states"][0]["status"], "available");
    assert_eq!(root["states"][0]["serial"], 3);
}

#[test]
fn a_file_with_a_syntax_error_is_reported_by_every_scan() {
    let workspace = Workspace::new();
    workspace.write("app/main.tf", "resource \"null_resource\" \"broken\" {\n");
    workspace.write(
        "app/sub/main.tf",
        "resource \"null_resource\" \"fine\" {}\n",
    );
    let app = workspace.path("app");
    workspace.tfex(&["scan", app.as_str()]);

    let report = workspace.tfex(&["scan", app.as_str()]);
    assert_eq!(report["unchanged_directories"], 1);
    assert_eq!(
        report["parse_fallbacks"][0]["file"],
        workspace.path("app/main.tf").as_str()
    );
}

#[test]
fn a_directory_that_moved_is_read_again() {
    let workspace = Workspace::new();
    workspace.git_init("infra");
    workspace.git(
        "infra",
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/acme/infra.git",
        ],
    );
    workspace.write(
        "infra/root/main.tf",
        "module \"net\" {\n  source = \"../net\"\n}\n",
    );
    workspace.write("infra/net/main.tf", "resource \"aws_vpc\" \"main\" {}\n");
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    let id = workspace.root_id("root");

    // Same repository, same files, same modification times, another place:
    // the module the root calls is now somewhere else too.
    std::fs::rename(workspace.path("infra"), workspace.path("moved")).unwrap();
    let report = workspace.tfex(&["scan", workspace.path("moved").as_str()]);
    assert_eq!(report["unchanged_directories"], 0);
    assert_eq!(workspace.root_id("root"), id);
    let graph = workspace.tfex(&["graph", &id]);
    assert_eq!(graph["path"], workspace.path("moved/root").as_str());
    assert_eq!(graph["depends_on"][0]["relative_path"], "net");
}
