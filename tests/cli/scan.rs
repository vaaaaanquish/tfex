//! `tfex scan` and `tfex roots`.

use serde_json::{Value, json};

use crate::support::{SECRET, Workspace, infra_repository};

fn relative_paths(roots: &Value) -> Vec<&str> {
    roots
        .as_array()
        .unwrap()
        .iter()
        .map(|root| root["relative_path"].as_str().unwrap())
        .collect()
}

#[test]
fn distinguishes_roots_from_the_modules_they_call() {
    let workspace = infra_repository();
    let report = workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    assert_eq!(
        report,
        json!({
            "indexed_directories": 2,
            "repositories": 1,
            "confirmed_roots": 1,
            "probable_roots": 0,
            "modules": 1,
            "duplicate_checkouts": 0,
            "pruned_roots": 0,
            "parse_fallbacks": [],
        })
    );

    let roots = workspace.tfex(&["roots"]);
    assert_eq!(relative_paths(&roots), ["envs/prod"]);
    let root = &roots[0];
    assert_eq!(root["classification"], "confirmed");
    assert_eq!(root["reason"], "backend declared");
    assert_eq!(root["backend_type"], "local");
    assert_eq!(root["repository"], "infra");
    assert_eq!(root["path"], workspace.path("infra/envs/prod").as_str());
    assert_eq!(root["resources"], 1);

    let all = workspace.tfex(&["ls", "--all"]);
    assert_eq!(relative_paths(&all), ["envs/prod", "modules/network"]);
    assert_eq!(all[1]["classification"], "module");
}

#[test]
fn indexes_several_repositories_at_once() {
    let workspace = infra_repository();
    workspace.git_init("services");
    workspace.write(
        "services/main.tf",
        "resource \"null_resource\" \"other\" {}\n",
    );

    let report = workspace.tfex(&[
        "scan",
        workspace.path("infra").as_str(),
        workspace.path("services").as_str(),
    ]);
    assert_eq!(report["repositories"], 2);
    assert_eq!(report["indexed_directories"], 3);

    let roots = workspace.tfex(&["roots"]);
    let services = &roots[1];
    assert_eq!(services["repository"], "services");
    assert_eq!(services["relative_path"], ".");
    assert_eq!(services["classification"], "probable");
}

#[test]
fn folder_outside_git_counts_as_one_repository() {
    let workspace = Workspace::new();
    workspace.write(
        "plain/one/main.tf",
        "resource \"terraform_data\" \"first\" {}",
    );
    workspace.write(
        "plain/two/main.tf",
        "resource \"terraform_data\" \"second\" {}",
    );

    let report = workspace.tfex(&["scan", workspace.path("plain").as_str()]);
    assert_eq!(report["repositories"], 1);
    assert_eq!(report["probable_roots"], 2);

    let roots = workspace.tfex(&["roots"]);
    assert_eq!(relative_paths(&roots), ["one", "two"]);
    assert_eq!(roots[0]["repository"], "plain");
    assert_eq!(roots[0]["remote"], Value::Null);
}

#[test]
fn never_persists_credentials() {
    let workspace = infra_repository();
    workspace.write(
        "infra/remote-state/main.tf",
        &format!(
            r#"terraform {{
  backend "s3" {{
    bucket     = "infra-state"
    access_key = "{SECRET}_ACCESS_KEY"
    secret_key = "{SECRET}_SECRET_KEY"
  }}
}}
"#
        ),
    );
    let remote = format!("https://{SECRET}-user@github.com/acme/infra.git?token={SECRET}");
    workspace.git("infra", &["remote", "add", "origin", &remote]);

    workspace.tfex(&["scan", workspace.path("infra").as_str()]);

    // The state file, the backend block and the remote URL all held secrets.
    assert!(!workspace.index_contents().contains(SECRET));
    let root = workspace.tfex(&["inspect", "infra/remote-state"]);
    assert_eq!(root["backend_type"], "s3");
    assert_eq!(root["backend"], json!({"bucket": "infra-state"}));
    let roots = workspace.tfex(&["roots"]);
    assert_eq!(roots[0]["remote"], "https://github.com/acme/infra.git");
}

#[test]
fn indexes_the_main_working_tree_rather_than_linked_worktrees() {
    let workspace = infra_repository();
    workspace.git(
        "infra",
        &[
            "worktree",
            "add",
            "--quiet",
            "worktrees/feature",
            "-b",
            "feature",
        ],
    );
    workspace.write(
        "infra/worktrees/feature/envs/prod/extra.tf",
        "resource \"aws_sqs_queue\" \"only_on_the_branch\" {}\n",
    );

    let report = workspace.tfex(&["scan", workspace.root.as_str()]);
    assert_eq!(report["indexed_directories"], 2);
    assert_eq!(report["repositories"], 1);
    assert_eq!(report["duplicate_checkouts"], 1);

    let roots = workspace.tfex(&["roots"]);
    assert_eq!(relative_paths(&roots), ["envs/prod"]);
    assert_eq!(roots[0]["path"], workspace.path("infra/envs/prod").as_str());
    assert_eq!(workspace.tfex(&["search", "aws_sqs_queue"]), json!([]));
}

#[test]
fn a_linked_worktree_is_indexed_when_scanned_on_its_own() {
    let workspace = infra_repository();
    workspace.git(
        "infra",
        &["worktree", "add", "--quiet", "../feature", "-b", "feature"],
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    let id = workspace.root_id("envs/prod");

    let report = workspace.tfex(&["scan", workspace.path("feature").as_str()]);
    assert_eq!(report["duplicate_checkouts"], 0);

    // Same repository and path, so the same root: it now points at the
    // checkout that was scanned last.
    let roots = workspace.tfex(&["roots"]);
    assert_eq!(relative_paths(&roots), ["envs/prod"]);
    assert_eq!(roots[0]["id"], id.as_str());
    assert_eq!(
        roots[0]["path"],
        workspace.path("feature/envs/prod").as_str()
    );
}

#[test]
fn indexes_one_of_several_clones_of_the_same_origin() {
    let workspace = Workspace::new();
    for clone in ["clone-b", "clone-a"] {
        workspace.git_init(clone);
        workspace.write(
            &format!("{clone}/main.tf"),
            "resource \"terraform_data\" \"x\" {}\n",
        );
        workspace.git(
            clone,
            &["remote", "add", "origin", "git@github.com:acme/shared.git"],
        );
    }

    let report = workspace.tfex(&["scan", workspace.root.as_str()]);
    assert_eq!(report["repositories"], 1);
    assert_eq!(report["duplicate_checkouts"], 1);

    let roots = workspace.tfex(&["roots"]);
    assert_eq!(roots.as_array().unwrap().len(), 1);
    assert_eq!(roots[0]["path"], workspace.path("clone-a").as_str());
    assert_eq!(roots[0]["remote"], "git@github.com:acme/shared.git");
}

#[test]
fn leaves_the_scanned_repository_untouched() {
    let workspace = infra_repository();
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace.tfex(&["inspect", "infra/envs/prod"]);
    workspace.tfex(&["log", "infra/envs/prod"]);

    let status = workspace.git("infra", &["status", "--porcelain", "--ignored"]);
    assert_eq!(status, "");
}

#[test]
fn rescan_prunes_directories_that_are_gone() {
    let workspace = infra_repository();
    workspace.write(
        "infra/envs/dev/main.tf",
        "resource \"aws_s3_bucket\" \"dev\" {}\n",
    );
    let infra = workspace.path("infra");
    workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(
        relative_paths(&workspace.tfex(&["roots"])),
        ["envs/dev", "envs/prod"]
    );

    workspace.remove_directory("infra/envs/dev");
    let report = workspace.tfex(&["scan", infra.as_str()]);
    assert_eq!(report["pruned_roots"], 1);
    assert_eq!(relative_paths(&workspace.tfex(&["roots"])), ["envs/prod"]);
    assert_eq!(workspace.tfex(&["search", "aws_s3_bucket.dev"]), json!([]));
}

#[test]
fn rescan_of_one_path_keeps_what_is_indexed_elsewhere() {
    let workspace = infra_repository();
    workspace.write(
        "services/main.tf",
        "resource \"null_resource\" \"other\" {}\n",
    );
    workspace.tfex(&[
        "scan",
        workspace.path("infra").as_str(),
        workspace.path("services").as_str(),
    ]);

    let report = workspace.tfex(&["scan", workspace.path("infra/envs").as_str()]);
    assert_eq!(report["pruned_roots"], 0);
    assert_eq!(
        relative_paths(&workspace.tfex(&["roots", "--all"])),
        ["envs/prod", "modules/network", "."]
    );
}

#[test]
fn ignores_terraform_working_directories() {
    let workspace = Workspace::new();
    workspace.write(
        "app/main.tf",
        "terraform {\n  backend \"gcs\" {\n    bucket = \"state\"\n  }\n}\n",
    );
    // What `terraform init` leaves behind: backend metadata and module copies.
    workspace.write("app/.terraform/terraform.tfstate", "{\"serial\": 99}");
    workspace.write(
        "app/.terraform/modules/vpc/main.tf",
        "resource \"aws_vpc\" \"copy\" {}\n",
    );

    let report = workspace.tfex(&["scan", workspace.path("app").as_str()]);
    assert_eq!(report["indexed_directories"], 1);

    let root = workspace.tfex(&["inspect", "app"]);
    assert_eq!(root["backend"], json!({"bucket": "state"}));
    assert_eq!(
        root["states"],
        json!([{
            "workspace": "default",
            "backend_type": "gcs",
            "location": "gs://state/default.tfstate",
            "status": "unresolved",
            "serial": null,
            "lineage": null,
            "terraform_version": null,
            "resource_count": null,
            "fetched_at": null,
            "error": null,
        }])
    );
}

#[test]
fn reads_json_syntax_and_survives_syntax_errors() {
    let workspace = Workspace::new();
    workspace.write(
        "mixed/main.tf.json",
        r#"{"resource": {"aws_sqs_queue": {"jobs": {}}}}"#,
    );
    workspace.write(
        "mixed/broken.tf",
        "resource \"aws_sns_topic\" \"alerts\" {\n  name = \"alerts\"\n",
    );

    let report = workspace.tfex(&["scan", workspace.path("mixed").as_str()]);
    let fallbacks = report["parse_fallbacks"].as_array().unwrap();
    assert_eq!(fallbacks.len(), 1);
    assert_eq!(
        fallbacks[0]["file"],
        workspace.path("mixed/broken.tf").as_str()
    );
    assert!(fallbacks[0]["error"].as_str().unwrap().starts_with("line "));

    let root = workspace.tfex(&["inspect", "mixed"]);
    let addresses: Vec<&str> = root["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["address"].as_str().unwrap())
        .collect();
    assert_eq!(addresses, ["aws_sns_topic.alerts", "aws_sqs_queue.jobs"]);
}

#[test]
fn fails_on_a_path_that_is_not_a_directory() {
    let workspace = Workspace::new();
    let missing = workspace.path("missing");
    let failure = workspace.tfex_failure(&["scan", missing.as_str()]);
    assert_eq!(
        failure,
        json!({
            "error": format!("Not a directory: {missing}"),
            "kind": "not_a_directory",
        })
    );
}
