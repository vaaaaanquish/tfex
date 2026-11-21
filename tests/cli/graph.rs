//! `tfex graph` and `tfex deps`.

use serde_json::{Value, json};

use crate::support::Workspace;

/// A repository `infra` scanned into the index:
///
/// - `envs/prod` calls `modules/app` and a registry module;
/// - `modules/app` calls `modules/network`;
/// - `envs/dev` calls `modules/network` directly.
fn scanned_infra() -> Workspace {
    let workspace = Workspace::new();
    workspace.write(
        "infra/envs/prod/main.tf",
        r#"terraform {
  backend "local" {}
}

variable "region" {}

locals {
  name   = "prod-${var.region}"
  unused = local.name
}

data "aws_ami" "latest" {
  owners = [var.region]
}

module "app" {
  source = "../../modules/app"
  name   = local.name
  ami    = data.aws_ami.latest.id
}

module "registry" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "5.0.0"
}

resource "aws_route53_record" "app" {
  for_each = toset(["a", "b"])
  name     = "${each.key}.${local.name}"
  records  = [module.app.address]
}

output "record" {
  value = aws_route53_record.app
}
"#,
    );
    workspace.write(
        "infra/envs/dev/main.tf",
        "provider \"aws\" {}\n\nmodule \"network\" {\n  source = \"../../modules/network\"\n}\n",
    );
    workspace.write(
        "infra/modules/app/main.tf",
        "module \"network\" {\n  source = \"../network\"\n}\n",
    );
    workspace.write(
        "infra/modules/network/main.tf",
        "resource \"aws_vpc\" \"main\" {}\n",
    );
    workspace.tfex(&["scan", workspace.path("infra").as_str()]);
    workspace
}

fn directory(workspace: &Workspace, relative_path: &str, classification: &str) -> Value {
    json!({
        "root_id": workspace.root_id(relative_path),
        "repository": "infra",
        "relative_path": relative_path,
        "classification": classification,
    })
}

fn related(workspace: &Workspace, relative_path: &str, classification: &str, depth: u32) -> Value {
    let mut related = directory(workspace, relative_path, classification);
    related["depth"] = json!(depth);
    related
}

#[test]
fn graph_lists_every_module_call_with_the_directory_it_resolves_to() {
    let workspace = scanned_infra();
    let app = directory(&workspace, "modules/app", "module");
    let network = directory(&workspace, "modules/network", "module");
    let prod = directory(&workspace, "envs/prod", "confirmed");
    assert_eq!(
        workspace.tfex(&["graph"]),
        json!([
            {
                "caller": directory(&workspace, "envs/dev", "probable"),
                "module": "network",
                "source": "../../modules/network",
                "file": "main.tf",
                "line": 3,
                "target": network,
            },
            {
                "caller": prod,
                "module": "app",
                "source": "../../modules/app",
                "file": "main.tf",
                "line": 16,
                "target": app,
            },
            {
                "caller": prod,
                "module": "registry",
                "source": "terraform-aws-modules/vpc/aws",
                "file": "main.tf",
                "line": 22,
                "target": null,
            },
            {
                "caller": app,
                "module": "network",
                "source": "../network",
                "file": "main.tf",
                "line": 1,
                "target": network,
            },
        ])
    );
}

#[test]
fn graph_of_a_root_follows_module_calls_and_lists_references() {
    let workspace = scanned_infra();
    let graph = workspace.tfex(&["graph", "infra/envs/prod"]);
    assert_eq!(graph["id"], workspace.root_id("envs/prod").as_str());
    assert_eq!(graph["classification"], "confirmed");
    assert_eq!(graph["module_calls"].as_array().unwrap().len(), 2);
    assert_eq!(
        graph["depends_on"],
        json!([
            related(&workspace, "modules/app", "module", 1),
            related(&workspace, "modules/network", "module", 2),
        ])
    );
    assert_eq!(graph["dependents"], json!([]));

    // `each.key` and the registry module's inputs refer to nothing declared.
    let reference = |from: &str, to: &str, line: u32| json!({"from": from, "to": to, "file": "main.tf", "line": line});
    assert_eq!(
        graph["references"],
        json!([
            reference("aws_route53_record.app", "local.name", 29),
            reference("aws_route53_record.app", "module.app", 30),
            reference("data.aws_ami.latest", "var.region", 13),
            reference("local.name", "var.region", 8),
            reference("local.unused", "local.name", 9),
            reference("module.app", "data.aws_ami.latest", 19),
            reference("module.app", "local.name", 18),
            reference("output.record", "aws_route53_record.app", 34),
        ])
    );
}

#[test]
fn graph_of_a_module_shows_every_directory_that_uses_it() {
    let workspace = scanned_infra();
    let graph = workspace.tfex(&["graph", "infra/modules/network"]);
    assert_eq!(graph["depends_on"], json!([]));
    assert_eq!(
        graph["dependents"],
        json!([
            related(&workspace, "envs/dev", "probable", 1),
            related(&workspace, "modules/app", "module", 1),
            related(&workspace, "envs/prod", "confirmed", 2),
        ])
    );
    assert_eq!(graph["references"], json!([]));
}

#[test]
fn deps_follows_references_in_both_directions() {
    let workspace = scanned_infra();
    let related = |address: &str, depth: u32| json!({"address": address, "depth": depth});
    assert_eq!(
        workspace.tfex(&["deps", "infra/envs/prod", "module.app"]),
        json!({
            "root_id": workspace.root_id("envs/prod"),
            "repository": "infra",
            "relative_path": "envs/prod",
            "address": "module.app",
            "depends_on": [
                related("data.aws_ami.latest", 1),
                related("local.name", 1),
                related("var.region", 2),
            ],
            "dependents": [
                related("aws_route53_record.app", 1),
                related("output.record", 2),
            ],
        })
    );

    let variable = workspace.tfex(&["deps", "infra/envs/prod", "var.region"]);
    assert_eq!(variable["depends_on"], json!([]));
    assert_eq!(
        variable["dependents"],
        json!([
            related("data.aws_ami.latest", 1),
            related("local.name", 1),
            related("aws_route53_record.app", 2),
            related("local.unused", 2),
            related("module.app", 2),
            related("output.record", 3),
        ])
    );

    // Declared, but neither referring to anything nor referred to.
    let isolated = workspace.tfex(&["deps", "infra/envs/prod", "module.registry"]);
    assert_eq!(isolated["depends_on"], json!([]));
    assert_eq!(isolated["dependents"], json!([]));
}

#[test]
fn deps_fails_on_an_address_the_root_does_not_have() {
    let workspace = scanned_infra();
    assert_eq!(
        workspace.tfex_failure(&["deps", "infra/envs/prod", "aws_vpc.main"]),
        json!({
            "error": "Nothing in infra/envs/prod is declared or referred to as aws_vpc.main; \
                      see 'graph infra/envs/prod'",
            "kind": "address_not_found",
        })
    );
}

#[test]
fn reference_cycles_end() {
    let workspace = Workspace::new();
    workspace.write(
        "loop/main.tf",
        "locals {\n  a = local.b\n  b = local.a\n}\nmodule \"self\" {\n  source = \"../loop\"\n}\n",
    );
    workspace.tfex(&["scan", workspace.path("loop").as_str()]);

    let deps = workspace.tfex(&["deps", "loop", "local.a"]);
    assert_eq!(
        deps["depends_on"],
        json!([{"address": "local.b", "depth": 1}])
    );
    assert_eq!(
        deps["dependents"],
        json!([{"address": "local.b", "depth": 1}])
    );
    let graph = workspace.tfex(&["graph", "loop"]);
    assert_eq!(graph["depends_on"], json!([]));
    assert_eq!(graph["dependents"], json!([]));
    assert_eq!(graph["module_calls"][0]["target"]["relative_path"], ".");
}

#[test]
fn a_module_indexed_by_another_scan_is_still_resolved() {
    let workspace = Workspace::new();
    workspace.write(
        "live/main.tf",
        "module \"shared\" {\n  source = \"../shared\"\n}\n",
    );
    workspace.write("shared/main.tf", "resource \"aws_vpc\" \"main\" {}\n");
    workspace.tfex(&["scan", workspace.path("live").as_str()]);
    assert_eq!(workspace.tfex(&["graph", "live"])["depends_on"], json!([]));

    workspace.tfex(&["scan", workspace.path("shared").as_str()]);
    let graph = workspace.tfex(&["graph", "live"]);
    assert_eq!(graph["depends_on"][0]["repository"], "shared");
    assert_eq!(
        workspace.tfex(&["graph", "shared"])["dependents"][0]["repository"],
        "live"
    );
}
