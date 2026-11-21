//! Native-syntax (`.tf`) files, read with a real HCL parser.
//!
//! Because the file is fully parsed, text that merely looks like a block (in
//! a comment, a heredoc, a string, or a nested block) is never indexed.

use std::collections::BTreeMap;

use hcl_edit::Span;
use hcl_edit::expr::Expression;
use hcl_edit::parser;
use hcl_edit::structure::{Block, BlockLabel, Body};
use serde_json::Value;

use super::{
    Backend, Configuration, LineIndex, ModuleCall, Resource, ResourceKind, is_safe_backend_setting,
};

/// Add the declarations in `text` to `configuration`.
///
/// On a syntax error nothing is added and the parser's message is returned.
pub(super) fn read(
    text: &str,
    file: &str,
    configuration: &mut Configuration,
) -> Result<(), String> {
    let body: Body = text.parse().map_err(|error| describe(&error))?;
    let lines = LineIndex::new(text);
    // Terraform allows a single backend; the first one in the file is taken.
    let mut backend = None;
    for block in body.blocks() {
        let labels = labels_of(block);
        let line = lines.line_at(offset_of(block));
        match (block.ident.as_str(), labels.as_slice()) {
            ("resource", [type_name, name]) => configuration.resources.push(Resource::new(
                ResourceKind::Managed,
                type_name,
                name,
                file,
                line,
            )),
            ("data", [type_name, name]) => configuration.resources.push(Resource::new(
                ResourceKind::Data,
                type_name,
                name,
                file,
                line,
            )),
            ("provider", [_]) => configuration.has_provider = true,
            ("module", [name]) => configuration.module_calls.push(ModuleCall {
                name: (*name).to_owned(),
                source: block
                    .body
                    .get_attribute("source")
                    .and_then(|attribute| string_value(&attribute.value, text)),
                file: file.to_owned(),
                line,
            }),
            ("terraform", []) if backend.is_none() => backend = declared_backend(&block.body, text),
            _ => {}
        }
    }
    if backend.is_some() {
        configuration.backend = backend;
    }
    Ok(())
}

fn describe(error: &parser::Error) -> String {
    let location = error.location();
    format!(
        "line {}, column {}: {}",
        location.line(),
        location.column(),
        error.message()
    )
}

fn labels_of(block: &Block) -> Vec<&str> {
    block.labels.iter().map(BlockLabel::as_str).collect()
}

/// Byte offset of the block keyword.
fn offset_of(block: &Block) -> usize {
    block
        .ident
        .span()
        .or_else(|| block.span())
        .map_or(0, |span| span.start)
}

/// The `backend "<type>" {}` block, or HCP Terraform's `cloud {}` block,
/// inside a `terraform` block.
fn declared_backend(terraform: &Body, text: &str) -> Option<Backend> {
    terraform.blocks().find_map(|block| {
        let labels = labels_of(block);
        let kind = match (block.ident.as_str(), labels.as_slice()) {
            ("backend", [kind]) => *kind,
            ("cloud", []) => "cloud",
            _ => return None,
        };
        let mut settings = BTreeMap::new();
        collect_safe_settings(&block.body, text, &mut settings);
        Some(Backend {
            kind: kind.to_owned(),
            settings,
        })
    })
}

/// Safe settings with string values, including those in nested blocks such as
/// `workspaces { name = "..." }`.
fn collect_safe_settings(body: &Body, text: &str, settings: &mut BTreeMap<String, Value>) {
    for attribute in body.attributes() {
        let name = attribute.key.as_str();
        if is_safe_backend_setting(name)
            && let Some(value) = string_value(&attribute.value, text)
        {
            settings.insert(name.to_owned(), Value::String(value));
        }
    }
    for block in body.blocks() {
        collect_safe_settings(&block.body, text, settings);
    }
}

/// The value of a quoted string expression.
///
/// A template such as `"state-${var.env}"` is returned as written, without
/// interpolation, so that readers can tell the value is dynamic.
fn string_value(expression: &Expression, text: &str) -> Option<String> {
    match expression {
        Expression::String(string) => Some(string.value().clone()),
        Expression::StringTemplate(template) => {
            let quoted = text.get(template.span()?)?;
            let unquoted = quoted.strip_prefix('"')?.strip_suffix('"')?;
            Some(unquoted.to_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(text: &str) -> Configuration {
        let mut configuration = Configuration::default();
        read(text, "main.tf", &mut configuration).unwrap();
        configuration
    }

    fn declared(configuration: &Configuration) -> Vec<(String, usize)> {
        configuration
            .resources
            .iter()
            .map(|resource| (resource.address(), resource.line))
            .collect()
    }

    fn expected(resources: &[(&str, usize)]) -> Vec<(String, usize)> {
        resources
            .iter()
            .map(|(address, line)| ((*address).to_owned(), *line))
            .collect()
    }

    #[test]
    fn reads_blocks_with_their_lines() {
        let configuration = parse(
            r#"# leading comment
resource "aws_s3_bucket" "logs" {}

  data "aws_caller_identity" "current" {}

provider "aws" {
  region = "ap-northeast-1"
}

module "network" {
  source = "../../modules/network"
  cidr   = "10.0.0.0/16"
}
"#,
        );
        assert_eq!(
            declared(&configuration),
            expected(&[
                ("aws_s3_bucket.logs", 2),
                ("data.aws_caller_identity.current", 4)
            ])
        );
        assert!(configuration.has_provider);
        assert_eq!(
            configuration.module_calls,
            [ModuleCall {
                name: "network".to_owned(),
                source: Some("../../modules/network".to_owned()),
                file: "main.tf".to_owned(),
                line: 10,
            }]
        );
    }

    #[test]
    fn ignores_every_comment_style() {
        let configuration = parse(
            r#"# resource "a" "hash" {}
// resource "a" "slashes" {}
/*
resource "a" "block" {}
*/
resource "a" "real" {} # resource "a" "trailing" {}
"#,
        );
        assert_eq!(declared(&configuration), expected(&[("a.real", 6)]));
    }

    #[test]
    fn ignores_block_lookalikes_in_heredocs_and_strings() {
        let configuration = parse(
            r#"resource "null_resource" "script" {
  triggers = {
    body = <<-EOT
      resource "fake_heredoc" "nope" {
      }
    EOT
    escaped = "quote \" then\nresource \"fake_string\" \"nope\" {"
  }
}

resource "null_resource" "after" {}
"#,
        );
        assert_eq!(
            declared(&configuration),
            expected(&[("null_resource.script", 1), ("null_resource.after", 11)])
        );
    }

    #[test]
    fn ignores_nested_blocks_and_expressions() {
        let configuration = parse(
            r#"resource "aws_instance" "web" {
  for_each = { for k, v in var.hosts : k => v if v.enabled }
  tags     = merge(var.tags, { Name = "web-${each.key}" })

  dynamic "ebs_block_device" {
    for_each = var.disks
    content {
      device_name = ebs_block_device.value["name"]
    }
  }

  provisioner "local-exec" {
    command = "echo ${self.id}"
  }
}
"#,
        );
        assert_eq!(
            declared(&configuration),
            expected(&[("aws_instance.web", 1)])
        );
        assert!(!configuration.has_provider);
    }

    #[test]
    fn backend_keeps_only_safe_string_settings() {
        let configuration = parse(
            r#"terraform {
  required_version = ">= 1.5"
  backend "s3" {
    bucket     = "infra-state"
    key        = "envs/${var.env}/terraform.tfstate"
    region     = var.region
    access_key = "SECRET_ACCESS_KEY"
    secret_key = "SECRET_SECRET_KEY"
  }
}
"#,
        );
        let backend = configuration.backend.unwrap();
        assert_eq!(backend.kind, "s3");
        assert_eq!(
            json!(backend.settings),
            json!({"bucket": "infra-state", "key": "envs/${var.env}/terraform.tfstate"})
        );
    }

    #[test]
    fn backend_settings_include_nested_workspaces() {
        let configuration = parse(
            r#"terraform {
  backend "remote" {
    organization = "acme"
    workspaces {
      name = "prod"
    }
  }
}
"#,
        );
        let backend = configuration.backend.unwrap();
        assert_eq!(backend.kind, "remote");
        assert_eq!(
            json!(backend.settings),
            json!({"organization": "acme", "name": "prod"})
        );
    }

    #[test]
    fn cloud_block_counts_as_a_backend() {
        let configuration = parse(
            r#"terraform {
  cloud {
    organization = "acme"
    token        = "SECRET"
  }
}
"#,
        );
        let backend = configuration.backend.unwrap();
        assert_eq!(backend.kind, "cloud");
        assert_eq!(json!(backend.settings), json!({"organization": "acme"}));
    }

    #[test]
    fn terraform_block_without_backend_declares_none() {
        let configuration = parse("terraform {\n  required_version = \">= 1.5\"\n}\n");
        assert_eq!(configuration.backend, None);
    }

    #[test]
    fn module_source_must_be_a_literal() {
        let configuration = parse("module \"dynamic\" {\n  source = local.source\n}\n");
        assert_eq!(configuration.module_calls[0].source, None);
    }

    #[test]
    fn syntax_error_adds_nothing_and_reports_the_position() {
        let mut configuration = Configuration::default();
        let error = read(
            "resource \"a\" \"ok\" {}\nresource \"a\" \"broken\" {\n",
            "main.tf",
            &mut configuration,
        )
        .unwrap_err();
        assert!(error.starts_with("line "), "unexpected message: {error}");
        assert_eq!(configuration.resources, Vec::new());
    }
}
