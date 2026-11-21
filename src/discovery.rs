//! Finding Terraform directories and judging which of them are roots.

use std::collections::{BTreeSet, HashSet};
use std::sync::mpsc;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::{DirEntry, WalkBuilder, WalkState};

use crate::config::{self, Outline};
use crate::state;

/// Directories that are never descended into: version control data,
/// dependency caches, and Terraform's own working directories.
///
/// Skipping `.terraform` matters for correctness, not just speed: the
/// `terraform.tfstate` in it is backend metadata, not infrastructure state.
const EXCLUDED_DIRECTORIES: [&str; 6] = [
    ".git",
    ".terraform",
    ".venv",
    "node_modules",
    "vendor",
    ".terragrunt-cache",
];

/// Every directory under any of `bases` that directly contains a Terraform
/// configuration file. Symlinks are not followed and unreadable entries are
/// skipped.
///
/// The trees are walked on several threads: on a large checkout, listing
/// directories takes far longer than reading the configuration in them.
pub(crate) fn terraform_directories(bases: &[Utf8PathBuf]) -> BTreeSet<Utf8PathBuf> {
    let Some((first, rest)) = bases.split_first() else {
        return BTreeSet::new();
    };
    let mut walk = WalkBuilder::new(first);
    for base in rest {
        walk.add(base);
    }
    // Everything is visited except the excluded directories: hidden entries
    // and ones matched by `.gitignore` may hold Terraform too.
    walk.standard_filters(false)
        .follow_links(false)
        .filter_entry(|entry| !is_excluded(entry));

    let (sender, receiver) = mpsc::channel();
    walk.build_parallel().run(|| {
        let sender = sender.clone();
        Box::new(move |entry| {
            if let Ok(entry) = entry
                && is_configuration_file(&entry)
                && let Some(directory) = containing_directory(&entry)
            {
                // The receiver outlives the walk, so sending cannot fail.
                let _ = sender.send(directory);
            }
            WalkState::Continue
        })
    });
    drop(sender);
    receiver.into_iter().collect()
}

fn is_directory(entry: &DirEntry) -> bool {
    entry
        .file_type()
        .is_some_and(|file_type| file_type.is_dir())
}

fn is_excluded(entry: &DirEntry) -> bool {
    // A base is always scanned, whatever it is called.
    entry.depth() > 0
        && is_directory(entry)
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| EXCLUDED_DIRECTORIES.contains(&name))
}

fn is_configuration_file(entry: &DirEntry) -> bool {
    !is_directory(entry)
        && entry
            .file_name()
            .to_str()
            .is_some_and(config::is_configuration_file)
}

fn containing_directory(entry: &DirEntry) -> Option<Utf8PathBuf> {
    let directory = Utf8Path::from_path(entry.path())?.parent()?;
    Some(directory.to_owned())
}

/// How confident tfex is that a directory is a root module, that is, a
/// directory Terraform is run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    /// It owns a state.
    Confirmed,
    /// It is a reusable module called by another directory.
    Module,
    /// It looks like a root, but nothing shows that it owns a state.
    Probable,
    /// Nothing tells either way.
    Unknown,
}

impl Classification {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Classification::Confirmed => "confirmed",
            Classification::Module => "module",
            Classification::Probable => "probable",
            Classification::Unknown => "unknown",
        }
    }
}

/// The strongest signal found for what a directory is, from which its
/// [`Classification`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    BackendDeclared,
    LocalStateFound,
    ReferencedAsLocalModule,
    ProviderDeclared,
    DeclaresInfrastructure,
    Nothing,
}

impl Evidence {
    pub(crate) fn classification(self) -> Classification {
        match self {
            Evidence::BackendDeclared | Evidence::LocalStateFound => Classification::Confirmed,
            Evidence::ReferencedAsLocalModule => Classification::Module,
            Evidence::ProviderDeclared | Evidence::DeclaresInfrastructure => {
                Classification::Probable
            }
            Evidence::Nothing => Classification::Unknown,
        }
    }

    /// A short explanation shown next to the classification.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Evidence::BackendDeclared => "backend declared",
            Evidence::LocalStateFound => "local state found",
            Evidence::ReferencedAsLocalModule => "referenced as a local module",
            Evidence::ProviderDeclared => "provider declared",
            Evidence::DeclaresInfrastructure => "contains resources or module calls",
            Evidence::Nothing => "no root-specific signal",
        }
    }
}

/// Judge `directory` from the outline of its configuration, its files, and
/// the set of directories that other configurations call as local modules.
///
/// Owning a state outweighs being called as a module, which in turn outweighs
/// merely looking like a root.
pub(crate) fn classify(
    directory: &Utf8Path,
    outline: &Outline,
    local_modules: &HashSet<&Utf8Path>,
) -> Evidence {
    if outline.backend.is_some() {
        Evidence::BackendDeclared
    } else if state::has_local_state(directory) {
        Evidence::LocalStateFound
    } else if local_modules.contains(directory) {
        Evidence::ReferencedAsLocalModule
    } else if outline.has_provider {
        Evidence::ProviderDeclared
    } else if outline.declares_infrastructure {
        Evidence::DeclaresInfrastructure
    } else {
        Evidence::Nothing
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::paths;

    fn canonical_root(directory: &TempDir) -> Utf8PathBuf {
        paths::resolve(Utf8Path::from_path(directory.path()).unwrap()).unwrap()
    }

    fn touch(path: &Utf8Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn finds_directories_with_configuration_files() {
        let directory = TempDir::new().unwrap();
        let base = canonical_root(&directory);
        for file in [
            "main.tf",
            "envs/prod/main.tf",
            "envs/prod/variables.tf",
            "json/main.tf.json",
            "docs/readme.md",
            ".terraform/modules/network/main.tf",
            "node_modules/pkg/main.tf",
            "envs/prod/.terragrunt-cache/x/main.tf",
        ] {
            touch(&base.join(file));
        }

        let found = terraform_directories(std::slice::from_ref(&base));
        let expected: BTreeSet<Utf8PathBuf> =
            [base.clone(), base.join("envs/prod"), base.join("json")].into();
        assert_eq!(found, expected);
    }

    #[test]
    fn walks_every_base_and_finds_hidden_and_ignored_directories() {
        let directory = TempDir::new().unwrap();
        let root = canonical_root(&directory);
        touch(&root.join("one/.hidden/main.tf"));
        touch(&root.join("one/generated/main.tf"));
        fs::write(root.join("one/.gitignore"), "generated/\n").unwrap();
        touch(&root.join("two/main.tf"));
        touch(&root.join("three/main.tf"));

        let found = terraform_directories(&[root.join("one"), root.join("two")]);
        let expected: BTreeSet<Utf8PathBuf> = [
            root.join("one/.hidden"),
            root.join("one/generated"),
            root.join("two"),
        ]
        .into();
        assert_eq!(found, expected);
        assert_eq!(terraform_directories(&[]), BTreeSet::new());
    }

    #[test]
    fn scans_a_base_with_an_excluded_name() {
        let directory = TempDir::new().unwrap();
        let base = canonical_root(&directory).join("vendor");
        touch(&base.join("main.tf"));
        assert_eq!(
            terraform_directories(std::slice::from_ref(&base)),
            [base].into()
        );
    }

    #[test]
    fn evidence_is_ranked() {
        let directory = TempDir::new().unwrap();
        let root = canonical_root(&directory);
        let called: HashSet<&Utf8Path> = [root.as_path()].into();
        let nothing_called = HashSet::new();
        let mut outline = Outline::default();

        assert_eq!(
            classify(&root, &outline, &nothing_called),
            Evidence::Nothing
        );
        outline.declares_infrastructure = true;
        assert_eq!(
            classify(&root, &outline, &nothing_called),
            Evidence::DeclaresInfrastructure
        );
        outline.has_provider = true;
        assert_eq!(
            classify(&root, &outline, &nothing_called),
            Evidence::ProviderDeclared
        );
        assert_eq!(
            classify(&root, &outline, &called),
            Evidence::ReferencedAsLocalModule
        );
        touch(&root.join("terraform.tfstate"));
        assert_eq!(
            classify(&root, &outline, &called),
            Evidence::LocalStateFound
        );
    }
}
