//! Shallow reading of the Terraform configuration in one directory.
//!
//! Only what the index needs is extracted: which resources, data sources and
//! module calls are declared and where, whether a provider is configured, and
//! which backend holds the state. Expressions are never evaluated.

mod heuristic;
mod json;
mod native;

use std::collections::BTreeMap;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde_json::Value;

/// Backend settings that say where a state lives or how to reach it, and are
/// never credentials. Every other setting (access keys, tokens, ...) is
/// dropped while reading, so it cannot reach the index.
const SAFE_BACKEND_SETTINGS: [&str; 15] = [
    "bucket",
    "key",
    "prefix",
    "path",
    "region",
    "workspace_key_prefix",
    "container_name",
    "storage_account_name",
    "resource_group_name",
    "organization",
    "hostname",
    "workspace",
    "name",
    // Whose identity to read the state with: names, not secrets.
    "profile",
    "impersonate_service_account",
];

/// Whether a `resource` or a `data` block declared the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceKind {
    Managed,
    Data,
}

impl ResourceKind {
    /// The block keyword that declares this kind.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Managed => "resource",
            ResourceKind::Data => "data",
        }
    }
}

/// A `resource` or `data` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resource {
    pub(crate) kind: ResourceKind,
    pub(crate) type_name: String,
    pub(crate) name: String,
    /// File name inside the directory.
    pub(crate) file: String,
    /// 1-based line of the block keyword.
    pub(crate) line: usize,
}

impl Resource {
    fn new(kind: ResourceKind, type_name: &str, name: &str, file: &str, line: usize) -> Self {
        Resource {
            kind,
            type_name: type_name.to_owned(),
            name: name.to_owned(),
            file: file.to_owned(),
            line,
        }
    }

    /// The address Terraform uses for the object, such as
    /// `aws_s3_bucket.logs` or `data.aws_region.current`.
    pub(crate) fn address(&self) -> String {
        match self.kind {
            ResourceKind::Managed => format!("{}.{}", self.type_name, self.name),
            ResourceKind::Data => format!("data.{}.{}", self.type_name, self.name),
        }
    }
}

/// A `module` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleCall {
    pub(crate) name: String,
    /// The `source` argument, when it is a literal string.
    pub(crate) source: Option<String>,
    /// File name inside the directory.
    pub(crate) file: String,
    /// 1-based line of the block keyword.
    pub(crate) line: usize,
}

/// The backend declared in a `terraform` block.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Backend {
    /// Backend type, such as `s3`, `gcs` or `local`; `cloud` for a `cloud` block.
    pub(crate) kind: String,
    /// The settings in [`SAFE_BACKEND_SETTINGS`] that have literal values.
    pub(crate) settings: BTreeMap<String, Value>,
}

impl Backend {
    /// Whether the state is a file on the local disk.
    pub(crate) fn is_local(&self) -> bool {
        self.kind == "local"
    }
}

/// A file the HCL parser rejected and that was read heuristically instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ParseFallback {
    pub(crate) file: Utf8PathBuf,
    pub(crate) error: String,
}

/// Everything read from the Terraform files directly inside one directory.
#[derive(Debug, Default)]
pub(crate) struct Configuration {
    pub(crate) resources: Vec<Resource>,
    pub(crate) module_calls: Vec<ModuleCall>,
    pub(crate) backend: Option<Backend>,
    pub(crate) has_provider: bool,
    pub(crate) fallbacks: Vec<ParseFallback>,
}

/// Whether `name` is a Terraform configuration file (`.tf` or `.tf.json`).
#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "Terraform only loads files with exactly these suffixes"
)]
pub(crate) fn is_configuration_file(name: &str) -> bool {
    name.ends_with(".tf") || is_json_file(name)
}

fn is_json_file(name: &str) -> bool {
    name.ends_with(".tf.json")
}

/// Whether a setting value is a template, such as `"state-${var.env}"`, whose
/// actual value is only known to Terraform.
pub(crate) fn is_template(value: &str) -> bool {
    value.contains("${") || value.contains("%{")
}

fn is_safe_backend_setting(name: &str) -> bool {
    SAFE_BACKEND_SETTINGS.contains(&name)
}

/// Read the configuration files directly inside `directory`, in name order.
///
/// This never fails: an unreadable file is skipped, and a file with a syntax
/// error is read heuristically and reported in [`Configuration::fallbacks`].
pub(crate) fn read(directory: &Utf8Path) -> Configuration {
    let mut configuration = Configuration::default();
    for name in configuration_files(directory) {
        let path = directory.join(&name);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if is_json_file(&name) {
            json::read(text, &name, &mut configuration);
        } else if let Err(error) = native::read(text, &name, &mut configuration) {
            heuristic::read(text, &name, &mut configuration);
            configuration
                .fallbacks
                .push(ParseFallback { file: path, error });
        }
    }
    configuration
}

fn configuration_files(directory: &Utf8Path) -> Vec<String> {
    let Ok(entries) = directory.read_dir_utf8() else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_owned())
        .filter(|name| is_configuration_file(name))
        .collect();
    names.sort_unstable();
    names
}

/// Maps byte offsets in a text to 1-based line numbers.
struct LineIndex {
    newlines: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let newlines = text
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(offset, _)| offset)
            .collect();
        LineIndex { newlines }
    }

    fn line_at(&self, offset: usize) -> usize {
        self.newlines.partition_point(|newline| *newline < offset) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_counts_from_one() {
        let lines = LineIndex::new("a\nbc\n\nd");
        assert_eq!(lines.line_at(0), 1);
        assert_eq!(lines.line_at(1), 1, "the newline belongs to its own line");
        assert_eq!(lines.line_at(2), 2);
        assert_eq!(lines.line_at(5), 3);
        assert_eq!(lines.line_at(6), 4);
    }

    #[test]
    fn addresses_follow_terraform_conventions() {
        let mut resource =
            Resource::new(ResourceKind::Managed, "aws_s3_bucket", "logs", "main.tf", 1);
        assert_eq!(resource.address(), "aws_s3_bucket.logs");
        resource.kind = ResourceKind::Data;
        assert_eq!(resource.address(), "data.aws_s3_bucket.logs");
    }

    #[test]
    fn recognizes_templates() {
        assert!(is_template("state-${var.env}"));
        assert!(is_template("%{ if var.x }a%{ endif }"));
        assert!(!is_template("plain-$value-{braces}"));
    }

    #[test]
    fn recognizes_configuration_files() {
        assert!(is_configuration_file("main.tf"));
        assert!(is_configuration_file("main.tf.json"));
        assert!(!is_configuration_file("terraform.tfstate"));
        assert!(!is_configuration_file("main.tfvars"));
        assert!(!is_configuration_file("notes.json"));
    }
}
