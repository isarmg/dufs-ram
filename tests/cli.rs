//! Run cli with different args, not starting a server

#[path = "support/fixtures.rs"]
mod fixtures;

use assert_cmd::prelude::*;
use fixtures::Error;
use predicates::str::{contains, is_match};
use std::process::Command;

#[test]
/// Show help and exit.
fn help_shows() -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg("-h")
        .assert()
        .success();
    Ok(())
}

#[test]
fn version_includes_source_revision() -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg("--version")
        .assert()
        .success()
        .stdout(is_match(r" \(git [0-9a-f]{7,64}\)\n$")?);
    Ok(())
}

#[test]
fn account_is_required_to_start() -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .assert()
        .failure()
        .stderr(contains("At least one account is required"));

    Ok(())
}

#[test]
fn unknown_option_is_rejected() -> Result<(), Error> {
    Command::new(assert_cmd::cargo::cargo_bin!())
        .arg("--definitely-unknown-option")
        .assert()
        .failure()
        .stderr(contains(
            "unexpected argument '--definitely-unknown-option'",
        ));
    Ok(())
}
