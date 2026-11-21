//! Shallow reading of the Terraform configuration in one directory.
//!
//! Only what the index needs is extracted: which resources, data sources and
//! module calls are declared and where, which of them refer to which, whether
//! a provider is configured, and which backend holds the state. Expressions
//! are never evaluated.

mod heuristic;
mod json;
mod native;
mod references;

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde_json::Value;

use crate::{identity, paths};

/// How recently a file may have been modified for its modification time to
/// still be trusted. A file written again within the resolution of the file
/// system's timestamps keeps its modification time, so a directory with such
/// a fresh file is read again by the next scan whatever its fingerprint says.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Number of hexadecimal digits in a fingerprint.
const FINGERPRINT_LENGTH: usize = 32;

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
    /// The directory that is called, when `source` is a local path (`./...`
    /// or `../...`). Filled in by [`read`], which knows the caller.
    pub(crate) directory: Option<Utf8PathBuf>,
    /// File name inside the directory.
    pub(crate) file: String,
    /// 1-based line of the block keyword.
    pub(crate) line: usize,
}

impl ModuleCall {
    fn new(name: &str, source: Option<String>, file: &str, line: usize) -> Self {
        ModuleCall {
            name: name.to_owned(),
            source,
            directory: None,
            file: file.to_owned(),
            line,
        }
    }

    /// The address expressions refer to the call by, such as `module.network`.
    pub(crate) fn address(&self) -> String {
        format!("module.{}", self.name)
    }
}

/// A reference from one declaration to another in the same directory:
/// `from` cannot be evaluated without `to`.
///
/// Both ends are addresses as they are written in expressions. `from` is a
/// resource, a data source, a module call (`module.<name>`), a local value
/// (`local.<name>`) or an output (`output.<name>`); `to` is any of the first
/// four, or an input variable (`var.<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reference {
    pub(crate) from: String,
    pub(crate) to: String,
    /// File name inside the directory.
    pub(crate) file: String,
    /// 1-based line of the referring expression.
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
    /// The first reference from each declaration to each other one.
    pub(crate) references: Vec<Reference>,
    /// The addresses of the declared input variables (`var.<name>`) and
    /// local values (`local.<name>`), which can be referred to but are not
    /// indexed themselves.
    named_values: HashSet<String>,
}

impl Configuration {
    /// Note that the declaration at `from` refers to each of `candidates`,
    /// if they turn out to be declared; see [`Configuration::resolve`].
    fn refer(
        &mut self,
        from: &str,
        candidates: Vec<references::Candidate>,
        file: &str,
        lines: &LineIndex,
    ) {
        for (to, offset) in candidates {
            self.references.push(Reference {
                from: from.to_owned(),
                to,
                file: file.to_owned(),
                line: lines.line_at(offset),
            });
        }
    }

    /// Finish a configuration once every file of `directory` is read: locate
    /// the local modules it calls, and keep only the references that name
    /// something the directory declares, once each.
    ///
    /// Filtering is what separates a reference from an expression that only
    /// looks like one, such as `each.value` or the variable of a `for`.
    fn resolve(&mut self, directory: &Utf8Path) {
        for call in &mut self.module_calls {
            call.directory = call
                .source
                .as_deref()
                .and_then(|source| local_module_directory(directory, source));
        }

        let mut declared = std::mem::take(&mut self.named_values);
        declared.extend(self.resources.iter().map(Resource::address));
        declared.extend(self.module_calls.iter().map(ModuleCall::address));
        let mut seen = HashSet::new();
        self.references.retain(|reference| {
            reference.from != reference.to
                && declared.contains(&reference.to)
                && seen.insert((reference.from.clone(), reference.to.clone()))
        });
        self.named_values = declared;
    }

    /// What the rest of a scan needs to know about the configuration.
    pub(crate) fn outline(&self) -> Outline {
        Outline {
            backend: self.backend.clone(),
            has_provider: self.has_provider,
            declares_infrastructure: !self.resources.is_empty() || !self.module_calls.is_empty(),
            local_modules: self
                .module_calls
                .iter()
                .filter_map(|call| call.directory.clone())
                .collect(),
        }
    }
}

/// The part of a configuration that decides what a directory is, which
/// directories it makes modules of, and where its state is.
///
/// It is kept in the index, so that a scan can do without reading a
/// directory whose files did not change.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Outline {
    pub(crate) backend: Option<Backend>,
    pub(crate) has_provider: bool,
    /// Whether resources, data sources or module calls are declared.
    pub(crate) declares_infrastructure: bool,
    /// The directories called as local modules.
    pub(crate) local_modules: Vec<Utf8PathBuf>,
}

/// The directory that a module `source` written in `caller` refers to, when
/// it is a local path (`./...` or `../...`).
fn local_module_directory(caller: &Utf8Path, source: &str) -> Option<Utf8PathBuf> {
    let is_local = source.starts_with("./") || source.starts_with("../");
    is_local.then(|| paths::canonical(&caller.join(source)))
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
    configuration.resolve(directory);
    configuration
}

/// A digest of the names, sizes and modification times of the configuration
/// files directly inside `directory`: while it stays the same, [`read`]
/// would return what it returned before.
///
/// `None` when that cannot be relied on: a file cannot be examined, or was
/// modified so recently that another change could go unnoticed.
pub(crate) fn fingerprint(directory: &Utf8Path) -> Option<String> {
    let newest_trusted = SystemTime::now().checked_sub(RACY_WINDOW)?;
    // What is extracted from a file depends on the version doing it.
    let mut description = format!("tfex {}\n", env!("CARGO_PKG_VERSION"));
    let mut describe = |name: &str, size: u64, modified: Duration| {
        // Writing to a `String` cannot fail.
        let _ = writeln!(description, "{name}\0{size}\0{}", modified.as_nanos());
    };
    for name in configuration_files(directory) {
        let metadata = fs::metadata(directory.join(&name)).ok()?;
        let modified = metadata.modified().ok()?;
        if modified > newest_trusted {
            return None;
        }
        let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;
        describe(&name, metadata.len(), since_epoch);
    }
    Some(identity::sha256_prefix(&description, FINGERPRINT_LENGTH))
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
    fn only_relative_sources_are_local_modules() {
        let caller = Utf8Path::new("/nonexistent-tfex/envs/prod");
        assert_eq!(
            local_module_directory(caller, "../../modules/network").unwrap(),
            "/nonexistent-tfex/modules/network"
        );
        assert_eq!(
            local_module_directory(caller, "./child").unwrap(),
            "/nonexistent-tfex/envs/prod/child"
        );
        assert_eq!(local_module_directory(caller, "hashicorp/vpc/aws"), None);
    }

    fn write(directory: &Utf8Path, name: &str, contents: &str, age: Duration) {
        let path = directory.join(name);
        fs::write(&path, contents).unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    fn temporary_directory() -> (tempfile::TempDir, Utf8PathBuf) {
        let directory = tempfile::TempDir::new().unwrap();
        let path = paths::resolve(Utf8Path::from_path(directory.path()).unwrap()).unwrap();
        (directory, path)
    }

    #[test]
    fn reading_keeps_only_references_to_what_is_declared() {
        let (_guard, directory) = temporary_directory();
        let hour = Duration::from_secs(3600);
        write(
            &directory,
            "main.tf",
            r#"resource "aws_instance" "web" {
  for_each  = var.hosts
  subnet_id = aws_subnet.main.id
  user_data = "${each.value.name} ${aws_subnet.main.cidr_block} ${self.id}"
  tags      = { for k, v in local.tags : k => v.text }
  other     = aws_instance.web
  missing   = aws_eip.undeclared.id
}

module "network" {
  source = "./network"
}
"#,
            hour,
        );
        // References are resolved across the files of the directory.
        write(
            &directory,
            "other.tf",
            "resource \"aws_subnet\" \"main\" {}\nvariable \"hosts\" {}\nlocals {\n  tags = {}\n}\n",
            hour,
        );

        let configuration = read(&directory);
        let references: Vec<(&str, &str, &str, usize)> = configuration
            .references
            .iter()
            .map(|reference| {
                (
                    reference.from.as_str(),
                    reference.to.as_str(),
                    reference.file.as_str(),
                    reference.line,
                )
            })
            .collect();
        assert_eq!(
            references,
            [
                ("aws_instance.web", "var.hosts", "main.tf", 2),
                ("aws_instance.web", "aws_subnet.main", "main.tf", 3),
                ("aws_instance.web", "local.tags", "main.tf", 5),
            ]
        );
        assert_eq!(
            configuration.outline(),
            Outline {
                backend: None,
                has_provider: false,
                declares_infrastructure: true,
                local_modules: vec![directory.join("network")],
            }
        );
    }

    #[test]
    fn fingerprint_follows_the_configuration_files() {
        let (_guard, directory) = temporary_directory();
        let hour = Duration::from_secs(3600);
        write(&directory, "main.tf", "", hour);
        write(&directory, "notes.md", "", hour);
        let original = fingerprint(&directory).unwrap();
        assert_eq!(original.len(), FINGERPRINT_LENGTH);
        assert_eq!(fingerprint(&directory).unwrap(), original);

        // Other files do not count; size, time and the set of files do.
        write(&directory, "notes.md", "changed", hour * 2);
        assert_eq!(fingerprint(&directory).unwrap(), original);
        write(&directory, "main.tf", "#", hour);
        let resized = fingerprint(&directory).unwrap();
        assert_ne!(resized, original);
        write(&directory, "main.tf", "#", hour * 2);
        let touched = fingerprint(&directory).unwrap();
        assert_ne!(touched, resized);
        write(&directory, "extra.tf.json", "{}", hour);
        assert_ne!(fingerprint(&directory).unwrap(), touched);
    }

    #[test]
    fn fingerprint_distrusts_files_that_were_just_written() {
        let (_guard, directory) = temporary_directory();
        write(&directory, "main.tf", "", Duration::from_secs(3600));
        write(&directory, "fresh.tf", "", Duration::ZERO);
        assert_eq!(fingerprint(&directory), None);
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
