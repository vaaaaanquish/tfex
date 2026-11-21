//! A temporary workspace to build fixtures in and run `tfex` against.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::{env, fs, iter};

use assert_cmd::cargo::cargo_bin_cmd;
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The marker used for every value that must never reach the index or the
/// output.
pub(crate) const SECRET: &str = "SENSITIVE";

pub(crate) struct Workspace {
    /// Canonical path of the temporary directory.
    pub(crate) root: Utf8PathBuf,
    /// The index used by [`Workspace::tfex`]; it is outside any scanned path.
    pub(crate) index: Utf8PathBuf,
    _directory: TempDir,
}

impl Workspace {
    pub(crate) fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = Utf8Path::from_path(directory.path())
            .unwrap()
            .canonicalize_utf8()
            .unwrap();
        Workspace {
            index: root.join("index.sqlite3"),
            root,
            _directory: directory,
        }
    }

    /// Path of `relative` inside the workspace.
    pub(crate) fn path(&self, relative: &str) -> Utf8PathBuf {
        self.root.join(relative)
    }

    /// Write a file, creating its parent directories.
    pub(crate) fn write(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub(crate) fn remove_directory(&self, relative: &str) {
        fs::remove_dir_all(self.path(relative)).unwrap();
    }

    /// Run Git in `repository`, isolated from the user's configuration, and
    /// return its output.
    pub(crate) fn git(&self, repository: &str, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.path(repository))
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test Person")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test Person")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Turn `repository` into a Git repository.
    pub(crate) fn git_init(&self, repository: &str) {
        fs::create_dir_all(self.path(repository)).unwrap();
        self.git(repository, &["init", "--quiet"]);
    }

    /// Commit everything in `repository`.
    pub(crate) fn git_commit(&self, repository: &str, message: &str) {
        self.git(repository, &["add", "--all"]);
        self.git(repository, &["commit", "--quiet", "--message", message]);
    }

    /// Run `tfex` with the workspace index; it must succeed. Returns the JSON
    /// it printed.
    pub(crate) fn tfex(&self, arguments: &[&str]) -> Value {
        let assert = self.command(arguments).assert().success();
        serde_json::from_slice(&assert.get_output().stdout).unwrap()
    }

    /// Run `tfex` with the workspace index; it must fail with status 1 and
    /// print nothing to standard output. Returns the JSON failure it printed.
    pub(crate) fn tfex_failure(&self, arguments: &[&str]) -> Value {
        let assert = self.command(arguments).assert().code(1).stdout("");
        serde_json::from_slice(&assert.get_output().stderr).unwrap()
    }

    /// A `tfex` invocation with the workspace index. The workspace's `bin`
    /// directory comes first on its `PATH`; see [`Workspace::fake_remote`].
    pub(crate) fn command(&self, arguments: &[&str]) -> assert_cmd::Command {
        let path = env::join_paths(
            iter::once(self.path("bin").into_std_path_buf())
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let mut command = cargo_bin_cmd!("tfex");
        command
            .env_remove("TFEX_DB")
            .env("PATH", path)
            .arg("--db")
            .arg(&self.index)
            .args(arguments);
        command
    }

    /// Install stand-ins for `gcloud` and `aws` that serve objects from the
    /// workspace instead of the network, so that `tfex fetch` can be tested.
    ///
    /// An object `gs://bucket/key` is the file `remote/gs/bucket/key`; a
    /// missing file is reported the way the real tools report a missing
    /// object, and a file named `<key>.slow` makes the read hang. Every
    /// invocation is appended to `remote/calls.log`.
    pub(crate) fn fake_remote(&self) {
        let script = r#"#!/bin/sh
root="$(dirname "$0")/../remote"
echo "$(basename "$0") $*" >> "$root/calls.log"
uri="$3"
object="$root/${uri%%://*}/${uri#*://}"
if [ -f "$object.slow" ]; then
  exec sleep 30
elif [ -f "$object" ]; then
  cat "$object"
else
  echo "Reading $uri" >&2
  echo "ERROR: $uri: no such object, or access denied" >&2
  exit 1
fi
"#;
        fs::create_dir_all(self.path("remote")).unwrap();
        for tool in ["gcloud", "aws"] {
            let path = self.path("bin").join(tool);
            self.write(&format!("bin/{tool}"), script);
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// The commands the stand-ins installed by [`Workspace::fake_remote`]
    /// were run with, one per line, sorted.
    pub(crate) fn remote_calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.path("remote/calls.log")).unwrap_or_default();
        let mut calls: Vec<String> = log.lines().map(str::to_owned).collect();
        calls.sort();
        calls
    }

    /// The ID of the only non-module root at `relative_path`.
    pub(crate) fn root_id(&self, relative_path: &str) -> String {
        let roots = self.tfex(&["roots", "--all"]);
        let matching: Vec<&Value> = roots
            .as_array()
            .unwrap()
            .iter()
            .filter(|root| root["relative_path"] == relative_path)
            .collect();
        assert_eq!(matching.len(), 1, "roots at {relative_path}: {roots}");
        matching[0]["id"].as_str().unwrap().to_owned()
    }

    /// The whole index file, as text, for checking what was persisted.
    pub(crate) fn index_contents(&self) -> String {
        String::from_utf8_lossy(&fs::read(&self.index).unwrap()).into_owned()
    }
}

/// A repository named `infra` with one root and the module it calls:
///
/// - `envs/prod`: local backend, one resource, a call to `modules/network`,
///   and a state file holding secrets.
/// - `modules/network`: one resource.
pub(crate) fn infra_repository() -> Workspace {
    let workspace = Workspace::new();
    workspace.git_init("infra");
    workspace.write(
        "infra/envs/prod/main.tf",
        r#"
# resource "aws_s3_bucket" "commented_out" {}
terraform {
  backend "local" {}
}

resource "aws_s3_bucket" "logs" {}

module "network" {
  source = "../../modules/network"
}
"#,
    );
    workspace.write(
        "infra/envs/prod/terraform.tfstate",
        &state_file(7, 1).to_string(),
    );
    workspace.write(
        "infra/modules/network/main.tf",
        "resource \"aws_vpc\" \"main\" {}\n",
    );
    workspace.git_commit("infra", "Create production infra");
    workspace
}

/// A state with `instances` managed resource instances and secrets in every
/// place Terraform may put them.
pub(crate) fn state_file(serial: u32, instances: usize) -> Value {
    json!({
        "version": 4,
        "terraform_version": "1.9.0",
        "serial": serial,
        "lineage": "test-lineage",
        "outputs": {"password": {"value": SECRET, "type": "string"}},
        "resources": [{
            "mode": "managed",
            "type": "aws_s3_bucket",
            "name": "logs",
            "instances": vec![json!({"attributes": {"password": SECRET}}); instances],
        }],
    })
}
