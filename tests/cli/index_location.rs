//! Where the index is created when `--db` is not given.

use assert_cmd::cargo::cargo_bin_cmd;

use crate::support::Workspace;

/// A `tfex roots` run with only the given environment variables deciding the
/// index location.
fn roots_with_environment(variables: &[(&str, &str)]) {
    let mut command = cargo_bin_cmd!("tfex");
    command
        .env_remove("TFEX_DB")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME");
    for (name, value) in variables {
        command.env(name, value);
    }
    command.arg("roots").assert().success().stdout("[]\n");
}

#[test]
fn defaults_to_the_data_directory_under_home() {
    let workspace = Workspace::new();
    roots_with_environment(&[("HOME", workspace.root.as_str())]);
    assert!(workspace.path(".local/share/tfex/index.sqlite3").is_file());
}

#[test]
fn xdg_data_home_takes_precedence_over_home() {
    let workspace = Workspace::new();
    roots_with_environment(&[
        ("HOME", workspace.path("home").as_str()),
        ("XDG_DATA_HOME", workspace.path("data").as_str()),
    ]);
    assert!(workspace.path("data/tfex/index.sqlite3").is_file());
    assert!(!workspace.path("home").exists());
}

#[test]
fn tfex_db_takes_precedence_over_everything() {
    let workspace = Workspace::new();
    roots_with_environment(&[
        ("HOME", workspace.path("home").as_str()),
        ("TFEX_DB", workspace.path("custom/index.db").as_str()),
    ]);
    assert!(workspace.path("custom/index.db").is_file());
    assert!(!workspace.path("home").exists());
}

#[test]
fn fails_when_no_location_can_be_determined() {
    let mut command = cargo_bin_cmd!("tfex");
    command
        .env_remove("TFEX_DB")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .arg("roots")
        .assert()
        .code(1)
        .stdout("")
        .stderr(
            "{\"error\":\"Cannot locate the index: set --db, TFEX_DB, XDG_DATA_HOME, or \
             HOME\",\"kind\":\"index_location_unknown\"}\n",
        );
}
